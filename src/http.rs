//! Live-data HTTP server. Answers for any Host, so DNS can point the client's live-data host names
//! here (see AGENTS.md).
//!
//! Files generated from the user's game data (the net config module, the networking texts) are held
//! in memory as [`Asset`]s and never written to disk. An optional static directory serves extra files
//! the operator adds (e.g. languages/english/motd.jpg); generated paths take precedence over it.
//!
//! Last-Modified: the client compares it for equality with its cached copy's mtime to decide whether
//! to download again. An asset's date is therefore derived from its content ([`content_date`]):
//! stable across restarts while the bytes are unchanged, different when they change.
//!
//! Uploads: the client stores player profiles (`profiles/<online id>/profile.<n>`) and hosted-game
//! records (`games/<id>`) on the live-data host, as the original object store did. Uploads are not
//! authenticated, so only those exact key shapes are accepted (a game record only for an id the game
//! list service issued) and the number of stored files is capped; every other upload is refused. A
//! profile is kept only around a match with another console, and only from the player's own
//! address (see "Profile uploads" in matchmaking); other profile uploads are answered like kept
//! ones but not stored. Kept uploads are written under the storage directory and served back on
//! GET. The client uploads the same profile several times within a few milliseconds, on separate
//! connections: storage access is serialized and each upload is written to a temporary file that
//! then replaces the stored one, so a reader gets either the old or the new file, never a mix, and
//! the last upload received wins.
//!
//! Connections: an address may hold a limited number open (see listen). A connection must send a
//! complete request head within 30 s of opening or of its previous response, and an upload body
//! within 30 s.
//!
//! Header names are sent in Title-Case, as the original servers did: the client looks the header up with a
//! case-sensitive "Last-Modified". With hyper's default lowercase names it read time 0, stamped its
//! cached copy with mtime 0, and then treated that copy as current on every later start, so changes
//! to the served file never arrived.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Router};
use md5::{Digest, Md5};
use tracing::{debug, info, warn};

use crate::listen::{Listener, Throttle};
use crate::matchmaking::Matchmaking;
use crate::services::GameIds;

/// A file served from memory.
#[derive(Clone, Debug)]
pub struct Asset {
    bytes: Bytes,
    last_modified: SystemTime,
}

impl Asset {
    pub fn new(bytes: Vec<u8>) -> Self {
        let last_modified = content_date(&bytes);
        Self {
            bytes: Bytes::from(bytes),
            last_modified,
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// Generated files by normalized request path ("build/main/text1/2.networking.crypt").
pub type Assets = HashMap<String, Asset>;

/// A deterministic date for some bytes: 2001-01-01 plus an MD5-derived offset of up to ~20 years,
/// at whole seconds (HTTP dates have 1 s resolution).
pub fn content_date(bytes: &[u8]) -> SystemTime {
    const START: u64 = 978_307_200; // 2001-01-01T00:00:00Z
    const SPAN: u64 = 20 * 365 * 86_400;
    let digest = Md5::digest(bytes);
    let n = u64::from_be_bytes(digest[..8].try_into().unwrap());
    UNIX_EPOCH + Duration::from_secs(START + n % SPAN)
}

/// Path prefixes the client may upload to.
const UPLOAD_PREFIXES: [&str; 2] = ["profiles/", "games/"];
/// Largest accepted upload. Profiles and game records are a few KB at most.
const MAX_UPLOAD: usize = 64 << 10;
/// Most files the storage directory may hold; a new key beyond it is refused (existing ones can
/// still be replaced).
const MAX_STORED_FILES: usize = 50_000;
/// A connection that hasn't sent a complete request head this long after opening, or after its
/// previous response, is closed (hyper's default, which only applies once a timer is set).
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// An upload body not received within this time is refused.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Ctx {
    assets: Arc<Assets>,
    static_dir: Option<Arc<PathBuf>>,
    storage: Arc<PathBuf>,
    /// Held for every read and write of the storage directory (see module docs). Readers hold it
    /// too: on Windows a file open for reading cannot be replaced.
    storage_lock: Arc<tokio::sync::Mutex<()>>,
    /// Files in the storage directory (counted at startup, then kept up to date).
    stored_files: Arc<AtomicUsize>,
    games: Arc<GameIds>,
    /// Decides which profile uploads are kept.
    matchmaking: Arc<Matchmaking>,
    /// Storage warnings can be triggered by anyone who can upload: they are throttled.
    warnings: Arc<Throttle>,
}

pub async fn serve(
    bind: SocketAddr,
    assets: Assets,
    static_dir: Option<PathBuf>,
    storage: PathBuf,
    games: Arc<GameIds>,
    matchmaking: Arc<Matchmaking>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(%bind, generated = assets.len(), static_dir = ?static_dir, storage = %storage.display(), "live-data HTTP listening");
    serve_on(listener, assets, static_dir, storage, games, matchmaking).await
}

/// Number of regular files under `dir` (0 if it does not exist).
fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => count_files(&e.path()),
            Ok(t) if t.is_file() => 1,
            _ => 0,
        })
        .sum()
}

async fn serve_on(
    listener: tokio::net::TcpListener,
    assets: Assets,
    static_dir: Option<PathBuf>,
    storage: PathBuf,
    games: Arc<GameIds>,
    matchmaking: Arc<Matchmaking>,
) -> anyhow::Result<()> {
    let stored_files = Arc::new(AtomicUsize::new(count_files(&storage)));
    let ctx = Ctx {
        assets: Arc::new(assets),
        static_dir: static_dir.map(Arc::new),
        storage: Arc::new(storage),
        storage_lock: Arc::default(),
        stored_files,
        games,
        matchmaking,
        warnings: Arc::default(),
    };
    let app = Router::new().fallback(handle).with_state(ctx);
    let listener = Listener::new(listener, "http");
    loop {
        let (stream, peer, slot) = listener.accept().await;
        // The handler needs the client's address (see `store`).
        let service = hyper_util::service::TowerToHyperService::new(
            app.clone().layer(Extension(ConnectInfo(peer))),
        );
        tokio::spawn(async move {
            let _slot = slot;
            let conn = hyper::server::conn::http1::Builder::new()
                .title_case_headers(true)
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service);
            if let Err(e) = conn.await {
                debug!(%peer, error = %e, "http connection ended with an error");
            }
        });
    }
}

/// Normalizes a request path to an asset key: drops empty and "." segments (the older build's URL
/// contains a literal "/./"), rejects "..", backslashes and drive letters. None for the root or a
/// rejected path.
pub fn normalize(uri_path: &str) -> Option<String> {
    let mut segs = Vec::new();
    for seg in uri_path.split('/') {
        match seg {
            "" | "." => {}
            ".." => return None,
            s if s.contains('\\') || s.contains(':') => return None,
            s => segs.push(s),
        }
    }
    (!segs.is_empty()).then(|| segs.join("/"))
}

async fn handle(
    State(ctx): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let path = object_path(req.uri().path());
    let response = if method == Method::PUT || method == Method::POST {
        let body = axum::body::to_bytes(req.into_body(), MAX_UPLOAD);
        match tokio::time::timeout(BODY_READ_TIMEOUT, body).await {
            Ok(Ok(body)) => store(&ctx, peer, &path, &body).await,
            Ok(Err(_)) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
        }
    } else {
        respond(&ctx, &method, &path, &headers).await
    };
    info!(%method, %host, %path, status = response.status().as_u16(), %peer, "http");
    response
}

/// The client's object-store requests come in two forms: virtual-host style (the bucket is part of
/// the host name, path = `/<key>`) for downloads, and path style (path = `/<bucket>/<key>`) for
/// uploads. Both map to the key. A path is taken as path style when it does not start with an upload
/// prefix but does after its first segment; host names are never looked at.
fn object_path(path: &str) -> String {
    let Some(key) = normalize(path) else {
        return path.to_string();
    };
    let is_upload = |k: &str| UPLOAD_PREFIXES.iter().any(|p| k.starts_with(p));
    match key.split_once('/') {
        Some((_bucket, rest)) if !is_upload(&key) && is_upload(rest) => format!("/{rest}"),
        _ => path.to_string(),
    }
}

fn upload_key(path: &str) -> Option<String> {
    normalize(path).filter(|k| {
        UPLOAD_PREFIXES
            .iter()
            .any(|p| k.starts_with(p) && k.len() > p.len())
    })
}

/// Whether a client may write `key`: `profiles/<online id>/profile.<n>` (online ids are up to 16
/// letters, digits, '-' and '_'), or `games/<id>` for an id the game list service issued.
fn writable(key: &str, games: &GameIds) -> bool {
    let segs: Vec<&str> = key.split('/').collect();
    match segs.as_slice() {
        ["profiles", id, file] => {
            let id_ok = (1..=16).contains(&id.len())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            let n = file.strip_prefix("profile.").unwrap_or("");
            id_ok && (1..=4).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())
        }
        ["games", id] => games.issued(id),
        _ => false,
    }
}

fn stored_file(ctx: &Ctx, key: &str) -> PathBuf {
    key.split('/')
        .fold(ctx.storage.to_path_buf(), |p, s| p.join(s))
}

/// The online id a profile key belongs to.
fn profile_owner(key: &str) -> Option<&str> {
    key.strip_prefix("profiles/")?
        .split_once('/')
        .map(|(id, _)| id)
}

/// Saves an upload to an allowed prefix; answers like the original object store (200 and an ETag).
async fn store(ctx: &Ctx, peer: SocketAddr, path: &str, body: &[u8]) -> Response {
    let Some(key) = upload_key(path).filter(|k| writable(k, &ctx.games)) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if let Some(id) = profile_owner(&key)
        && !ctx.matchmaking.keeps_profile(id, peer.ip())
    {
        info!(%key, %peer, "profile upload not kept: profiles are kept only during and just after a match with another console");
        return uploaded(body);
    }
    let file = stored_file(ctx, &key);
    // Uploads run one at a time, so one temporary name per file is enough.
    let mut tmp = file.clone().into_os_string();
    tmp.push(".uploading");
    let tmp = PathBuf::from(tmp);
    let _guard = ctx.storage_lock.lock().await;
    let new_file = !tokio::fs::try_exists(&file).await.unwrap_or(false);
    if new_file && ctx.stored_files.load(Ordering::Relaxed) >= MAX_STORED_FILES {
        if let Some(suppressed) = ctx.warnings.allow() {
            warn!(%key, limit = MAX_STORED_FILES, suppressed, "upload refused: the storage directory holds the maximum number of files");
        }
        return StatusCode::INSUFFICIENT_STORAGE.into_response();
    }
    let write = async {
        if let Some(parent) = file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&tmp, body).await?;
        tokio::fs::rename(&tmp, &file).await
    };
    if let Err(e) = write.await {
        let _ = tokio::fs::remove_file(&tmp).await;
        if let Some(suppressed) = ctx.warnings.allow() {
            warn!(file = %file.display(), error = %e, suppressed, "upload write failed; the stored file is unchanged");
        }
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if new_file {
        ctx.stored_files.fetch_add(1, Ordering::Relaxed);
    }
    info!(%key, bytes = body.len(), "stored upload");
    uploaded(body)
}

/// The object store's answer to an upload: 200, with the body's MD5 as ETag.
fn uploaded(body: &[u8]) -> Response {
    let etag = format!("\"{:x}\"", Md5::digest(body));
    let mut resp = StatusCode::OK.into_response();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp
}

async fn respond(ctx: &Ctx, method: &Method, path: &str, headers: &HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(key) = normalize(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(asset) = ctx.assets.get(&key) {
        return reply(
            method,
            headers,
            asset.bytes.clone(),
            Some(asset.last_modified),
        );
    }
    let stored = upload_key(path).is_some();
    let file = if stored {
        stored_file(ctx, &key)
    } else if let Some(dir) = &ctx.static_dir {
        key.split('/').fold(dir.to_path_buf(), |p, s| p.join(s))
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let _guard = if stored {
        Some(ctx.storage_lock.lock().await)
    } else {
        None
    };
    match tokio::fs::metadata(&file).await {
        Ok(m) if m.is_file() => match tokio::fs::read(&file).await {
            Ok(bytes) => reply(method, headers, Bytes::from(bytes), m.modified().ok()),
            Err(e) => {
                if let Some(suppressed) = ctx.warnings.allow() {
                    warn!(file = %file.display(), error = %e, suppressed, "read failed");
                }
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        },
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn reply(
    method: &Method,
    headers: &HeaderMap,
    bytes: Bytes,
    modified: Option<SystemTime>,
) -> Response {
    let last_modified = modified.map(httpdate::fmt_http_date);
    if let (Some(m), Some(ims)) = (modified, headers.get(header::IF_MODIFIED_SINCE)) {
        if let Some(since) = ims
            .to_str()
            .ok()
            .and_then(|s| httpdate::parse_http_date(s).ok())
        {
            // HTTP dates have 1 s resolution: compare at that resolution. Generated assets are
            // dated from their content, so a changed asset can carry an earlier date: only an
            // exact match means "not modified".
            let secs = |t: SystemTime| {
                t.duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            };
            if secs(m) == secs(since) {
                return with_lm(StatusCode::NOT_MODIFIED.into_response(), &last_modified);
            }
        }
    }
    let len = bytes.len();
    let mut resp = Response::new(if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes)
    });
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    with_lm(resp, &last_modified)
}

fn with_lm(mut resp: Response, lm: &Option<String>) -> Response {
    if let Some(v) = lm.as_deref().and_then(|s| HeaderValue::from_str(s).ok()) {
        resp.headers_mut().insert(header::LAST_MODIFIED, v);
    }
    resp
}

#[cfg(test)]
mod tests;
