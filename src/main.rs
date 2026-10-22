//! uc3-server: a community server for the online mode of UC3. Not affiliated with or endorsed by the
//! game's developer or publisher.
//!   * live-data HTTP: the online config module and networking texts, generated at startup from the
//!     user's own game files and held in memory only; plus optional static files from data_dir
//!   * framed TCP services: login ticket, ban check and reports, heartbeat, invites, leaderboards,
//!     game list, text filter
//!   * location (plain TCP)
//!   * matchmaking rooms (the session manager's own protocol, plain TCP)
//! Platform services (sign-in, stats storage) are not provided here.

mod archive;
mod cipher;
mod config;
mod dc;
mod elf;
mod eula;
mod framecipher;
mod framed;
mod http;
mod listen;
mod location;
mod matchmaking;
mod services;
mod ticket;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{info, warn};

/// Name the client downloads its online config as: `/./<name>.psarc.crypt` on the live-data host.
pub const CONFIG_NAME: &str = "net18.bin";
/// The online config module inside the patch archive.
const CONFIG_ENTRY: &str = "dc1/net.bin";

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run all services.
    Run {
        #[arg(short, long, default_value = "uc3-server.toml")]
        config: PathBuf,
    },
    /// Write the served config (module, archive, encrypted archive) into a directory for inspection.
    /// The output is derived from the user's own game files: keep it local.
    GenConfig {
        #[arg(short, long, default_value = "uc3-server.toml")]
        config: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
    },
    /// List a game archive's entries and say whether it fits `patch_archive` or `text_archive`.
    /// Reads the file only; writes nothing.
    ListArchive {
        file: PathBuf,
        #[arg(long)]
        all: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::Run { config } => run(&config).await,
        Cmd::GenConfig { config, out } => {
            let cfg = config::Config::load(&config)?;
            let blob = std::fs::read(&cfg.patch_archive)
                .with_context(|| format!("reading {}", cfg.patch_archive.display()))?;
            let module = served_module(&cfg, &blob)?;
            let cipher = cipher::BlobCipher::from_executable(&cfg.executable)?;
            let archive = config_archive(&module);
            std::fs::create_dir_all(&out)?;
            std::fs::write(out.join(CONFIG_NAME), &module)?;
            std::fs::write(out.join(format!("{CONFIG_NAME}.archive")), &archive)?;
            std::fs::write(
                out.join(format!("{CONFIG_NAME}.archive.crypt")),
                cipher.encrypt_stream(&archive),
            )?;
            println!("module {} bytes -> {}", module.len(), out.display());
            Ok(())
        }
        Cmd::ListArchive { file, all } => list_archive(&file, all),
    }
}

/// The update's own config module, which only the latest update's patch archive holds.
const LATEST_UPDATE_ENTRY: &str = "dc1/net-patch18.bin";

/// Whether an archive has the entry `suffix`, at its root or under a folder.
fn holds(names: &[String], suffix: &str) -> bool {
    names
        .iter()
        .any(|n| n == suffix || n.ends_with(&format!("/{suffix}")))
}

fn archive_role(names: &[String]) -> &'static str {
    let has = |suffix: &str| holds(names, suffix);
    // The base text archive names its tables "<n>.<category>" without a path; patch archives that
    // carry text tables use full paths.
    let networking = names.iter().any(|n| {
        !n.contains('/')
            && n.split_once('.')
                .is_some_and(|(i, c)| c == "networking" && i.parse::<u32>().is_ok())
    });
    match (has(CONFIG_ENTRY), has(LATEST_UPDATE_ENTRY), networking) {
        (true, true, _) => "fits patch_archive (holds the online config module and online texts)",
        (true, false, _) => {
            "fits neither: a patch archive, but not the latest update's (no online config module of that update)"
        }
        (false, _, true) => {
            "fits text_archive (per-language <n>.<category> tables, including <n>.networking)"
        }
        _ => "fits neither patch_archive nor text_archive",
    }
}

fn list_archive(file: &Path, all: bool) -> Result<()> {
    let blob = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let names = archive::archive_manifest(&blob)
        .with_context(|| format!("{} is not a readable game archive", file.display()))?;
    println!("{}: {} entries", file.display(), names.len());
    let shown = if all {
        names.len()
    } else {
        names.len().min(20)
    };
    for n in &names[..shown] {
        println!("  {n}");
    }
    if shown < names.len() {
        println!("  ... {} more (use --all)", names.len() - shown);
    }
    println!("=> {}", archive_role(&names));
    Ok(())
}

/// The installed online config module with every server host pointed at public_host.
fn served_module(cfg: &config::Config, patch: &[u8]) -> Result<Vec<u8>> {
    // Older archives (the base game's, an edition's, the update's other one) also hold a config
    // module, but an older one: only the latest update's is served.
    let names = archive::archive_manifest(patch).with_context(|| {
        format!(
            "patch_archive = {}: not a readable game archive",
            cfg.patch_archive.display()
        )
    })?;
    if !holds(&names, LATEST_UPDATE_ENTRY) {
        anyhow::bail!(
            "patch_archive = {}: not the latest update's patch data archive (no {LATEST_UPDATE_ENTRY}; see AGENTS.md)",
            cfg.patch_archive.display()
        );
    }
    let (name, base) = archive::read_entry_by_suffix(patch, CONFIG_ENTRY).with_context(|| {
        format!(
            "patch_archive = {}: not the patch data archive (no {CONFIG_ENTRY}; see AGENTS.md)",
            cfg.patch_archive.display()
        )
    })?;
    let module = dc::Module::parse(&base).with_context(|| format!("{name} in patch_archive"))?;
    let entries = module.server_entries()?;
    let ports: std::collections::BTreeSet<u32> = entries
        .iter()
        .filter(|e| !e.host.is_empty())
        .map(|e| e.port)
        .collect();
    info!(entry = %name, servers = entries.len(), ?ports, "online config module found");
    let served = module.with_host(&cfg.public_host)?;
    let (mut served, eula) = dc::rewrite_eula(&served, &eula_url(cfg), eula::PAGES)?;
    info!(entries = eula, url = %eula_url(cfg), "EULA pages served by this server");
    let levels = dc::clear_list(&mut served, dc::SID_LEVEL_DOWNLOADS)?;
    info!(
        removed = levels,
        "level download list served empty (the bundles only existed on the original host)"
    );
    let changes = dc::set_min_players(&mut served, &cfg.playlists.min_players)
        .context("[playlists] min_players")?;
    for c in &changes {
        info!(playlist = %c.name, from = c.from, to = c.to, "playlist minimum players changed");
    }
    let playlists = dc::Module::parse(&served)?.playlists()?;
    let summary: Vec<String> = playlists
        .iter()
        .map(|p| format!("{} {}-{}", p.name, p.min_players, p.max_players))
        .collect();
    info!(playlists = %summary.join(", "), "matchmaking playlists (name min-max)");
    Ok(served)
}

/// Where the client fetches EULA pages (`%i` = page number).
fn eula_url(cfg: &config::Config) -> String {
    let port = cfg.http.bind.port();
    let host = if port == 80 {
        cfg.public_host.clone()
    } else {
        format!("{}:{port}", cfg.public_host)
    };
    format!("http://{host}/eula/page-%i.png")
}

/// The archive the client downloads: the module under the requested name and under net.bin.
fn config_archive(module: &[u8]) -> Vec<u8> {
    archive::build_archive(&[(CONFIG_NAME, module), ("net.bin", module)])
}

/// `build/main/text1/<n>.networking.crypt` for every language index, from the patch archive first,
/// then from text_archive.
fn networking_texts(
    cfg: &config::Config,
    patch: &[u8],
    cipher: &cipher::BlobCipher,
    assets: &mut http::Assets,
) -> Result<()> {
    let mut sources = vec![("patch_archive", patch.to_vec())];
    if let Some(p) = &cfg.text_archive {
        sources.push((
            "text_archive",
            std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
        ));
    }
    let mut served = std::collections::BTreeMap::new();
    for (label, blob) in &sources {
        for name in archive::archive_manifest(blob)? {
            let file = name.rsplit('/').next().unwrap_or(&name);
            let Some(index) = file.strip_suffix(".networking") else {
                continue;
            };
            if index.parse::<u32>().is_err() || served.contains_key(index) {
                continue;
            }
            let text = archive::read_archive_entry(blob, &name)?;
            assets.insert(
                format!("build/main/text1/{index}.networking.crypt"),
                http::Asset::new(cipher.encrypt_stream(&text)),
            );
            served.insert(index.to_string(), *label);
        }
    }
    if served.is_empty() {
        warn!("no <n>.networking text found: the client stops with a text sync error");
    } else {
        info!(indexes = %served.keys().cloned().collect::<Vec<_>>().join(","), "networking texts ready");
    }
    Ok(())
}

async fn run(config_path: &Path) -> Result<()> {
    let cfg = config::Config::load(config_path)?;
    let exe = elf::Elf::open(&cfg.executable)?;
    let cipher = Arc::new(cipher::BlobCipher::from_executable(&cfg.executable)?);
    let frame_key = Arc::new(framecipher::FrameKey::from_executable(&exe)?);
    drop(exe);

    let patch = std::fs::read(&cfg.patch_archive)
        .with_context(|| format!("reading {}", cfg.patch_archive.display()))?;
    let module = served_module(&cfg, &patch)?;
    let mut assets = http::Assets::new();
    let crypt = http::Asset::new(cipher.encrypt_stream(&config_archive(&module)));
    info!(path = %format!("/{CONFIG_NAME}.psarc.crypt"), bytes = crypt.len(), host = %cfg.public_host,
          "online config ready (in memory)");
    assets.insert(format!("{CONFIG_NAME}.psarc.crypt"), crypt);
    networking_texts(&cfg, &patch, &cipher, &mut assets)?;
    for i in eula::SERVED_PAGES {
        assets.insert(
            format!("eula/page-{i}.png"),
            http::Asset::new(eula::page(i)),
        );
    }
    drop(patch);

    let services = Arc::new(services::Services::new(Default::default()));
    let rooms = Arc::new(matchmaking::Matchmaking::new());
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(http::serve(
        cfg.http.bind,
        assets,
        cfg.data_dir.clone(),
        cfg.storage_dir.clone(),
        services.game_ids(),
        rooms.clone(),
    ));
    for port in config::FRAMED_PORTS {
        tasks.spawn(framed::serve(
            SocketAddr::new(cfg.bind_ip, port),
            frame_key.clone(),
            services.clone(),
        ));
    }
    tasks.spawn(location::serve(
        SocketAddr::new(cfg.bind_ip, config::LOCATION_PORT),
        cfg.location,
    ));
    tasks.spawn(matchmaking::serve(
        SocketAddr::new(cfg.bind_ip, config::MATCHMAKING_PORT),
        rooms,
    ));

    tokio::select! {
        Some(r) = tasks.join_next() => r??,
        _ = tokio::signal::ctrl_c() => info!("shutting down"),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
