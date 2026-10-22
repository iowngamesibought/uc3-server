//! Server configuration (TOML). Paths are resolved relative to the config file.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::location::Location;

/// Ports the client uses for its services. They come from the installed online config and are kept
/// as they are; only the host is changed (see dc::Module::with_host).
pub const FRAMED_PORTS: [u16; 3] = [6300, 6302, 5203];
pub const LOCATION_PORT: u16 = 6301;
/// The session manager's room protocol (config list +0x5c).
pub const MATCHMAKING_PORT: u16 = 6303;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The user's own decrypted game executable; the keys are read from it at startup.
    pub executable: PathBuf,
    /// The user's installed patch data archive: it holds the online config module (`…/dc1/net.bin`)
    /// and the per-language online texts (`…/text1/<n>.networking`). Read at startup, never copied.
    pub patch_archive: PathBuf,
    /// Optional: the base game's text archive, used for language indexes the patch archive lacks.
    #[serde(default)]
    pub text_archive: Option<PathBuf>,
    /// Optional folder of extra static files served over HTTP. Generated files take precedence.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    /// Where player uploads (profiles, hosted-game records) are kept. Default "storage".
    #[serde(default = "default_storage")]
    pub storage_dir: PathBuf,
    /// Host name or IPv4 address the console should use for this server (written into the config).
    pub public_host: String,
    /// Address to bind the service ports on.
    #[serde(default = "any_ip")]
    pub bind_ip: IpAddr,
    pub http: HttpConfig,
    #[serde(default)]
    pub location: Location,
    /// Optional changes to the matchmaking playlists in the served online config.
    #[serde(default)]
    pub playlists: PlaylistConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistConfig {
    /// Minimum players per playlist name ("*" = every playlist not named), written into the served
    /// online config (see dc::set_min_players). Empty: the installed values are served unchanged.
    #[serde(default)]
    pub min_players: BTreeMap<String, u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub bind: SocketAddr,
}

fn default_storage() -> PathBuf {
    PathBuf::from("storage")
}

fn any_ip() -> IpAddr {
    IpAddr::from([0, 0, 0, 0])
}

/// public_host is written into the served config, so it must be an address the console can reach.
fn check_public_host(host: &str) -> Result<()> {
    let h = host.trim();
    let wildcard = h.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified());
    if h.is_empty()
        || wildcard
        || h.contains("CHANGE")
        || !h.is_ascii()
        || h.contains(char::is_whitespace)
    {
        anyhow::bail!(
            "public_host = {host:?}: set it to this machine's LAN address as the console sees it \
                       (e.g. 192.168.1.50); the bind addresses can stay 0.0.0.0"
        );
    }
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        check_public_host(&cfg.public_host)?;
        cfg.location.check()?;
        let base = path.parent().unwrap_or(Path::new("."));
        cfg.executable = base.join(&cfg.executable);
        cfg.patch_archive = base.join(&cfg.patch_archive);
        cfg.storage_dir = base.join(&cfg.storage_dir);
        for p in [&mut cfg.data_dir, &mut cfg.text_archive] {
            if let Some(rel) = p.take() {
                *p = Some(base.join(rel));
            }
        }
        cfg.check_data_dir()?;
        Ok(cfg)
    }

    /// Everything in data_dir is served to anyone who can reach the HTTP port, so it must not hold
    /// the game files (above all the decrypted executable). Checked on the resolved real paths.
    fn check_data_dir(&self) -> Result<()> {
        let Some(configured) = self.data_dir.as_deref() else {
            return Ok(());
        };
        let Ok(dir) = std::fs::canonicalize(configured) else {
            return Ok(());
        };
        let files = [
            Some(&self.executable),
            Some(&self.patch_archive),
            self.text_archive.as_ref(),
        ];
        for file in files.into_iter().flatten() {
            if std::fs::canonicalize(file).is_ok_and(|f| f.starts_with(&dir)) {
                anyhow::bail!(
                    "data_dir = {}: it contains {}, and everything in data_dir is served to anyone who \
                     can reach the HTTP port; use a folder that holds only the extra static files",
                    configured.display(),
                    file.display()
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
