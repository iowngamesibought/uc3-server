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
//! list service issued) and the number of stored files is capped. Accepted uploads are written under
//! the storage directory and served back on GET; every other upload is refused. The client uploads the same profile several times within a few milliseconds,
//! on separate connections: storage access is serialized and each upload is written to a temporary
//! file that then replaces the stored one, so a reader gets either the old or the new file, never
//! a mix, and the last upload received wins.
//!
//! Header names are sent in Title-Case, as the original servers did: the client looks the header up with a
//! case-sensitive "Last-Modified". With hyper's default lowercase names it read time 0, stamped its
//! cached copy with mtime 0, and then treated that copy as current on every later start, so changes
//! to the served file never arrived.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use md5::{Digest, Md5};
use tracing::{debug, info, warn};

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
}

pub async fn serve(
    bind: std::net::SocketAddr,
    assets: Assets,
    static_dir: Option<PathBuf>,
    storage: PathBuf,
    games: Arc<GameIds>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(%bind, generated = assets.len(), static_dir = ?static_dir, storage = %storage.display(), "live-data HTTP listening");
    serve_on(listener, assets, static_dir, storage, games).await
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
) -> anyhow::Result<()> {
    let stored_files = Arc::new(AtomicUsize::new(count_files(&storage)));
    let ctx = Ctx {
        assets: Arc::new(assets),
        static_dir: static_dir.map(Arc::new),
        storage: Arc::new(storage),
        storage_lock: Arc::default(),
        stored_files,
        games,
    };
    let app = Router::new().fallback(handle).with_state(ctx);
    loop {
        let (stream, peer) = listener.accept().await?;
        let service = hyper_util::service::TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            let conn = hyper::server::conn::http1::Builder::new()
                .title_case_headers(true)
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

async fn handle(State(ctx): State<Ctx>, req: Request) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let path = object_path(req.uri().path());
    let response = if method == Method::PUT || method == Method::POST {
        match axum::body::to_bytes(req.into_body(), MAX_UPLOAD).await {
            Ok(body) => store(&ctx, &path, &body).await,
            Err(_) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        }
    } else {
        respond(&ctx, &method, &path, &headers).await
    };
    info!(%method, %host, %path, status = response.status().as_u16(), "http");
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

/// Saves an upload to an allowed prefix; answers like the original object store (200 and an ETag).
async fn store(ctx: &Ctx, path: &str, body: &[u8]) -> Response {
    let Some(key) = upload_key(path).filter(|k| writable(k, &ctx.games)) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let file = stored_file(ctx, &key);
    // Uploads run one at a time, so one temporary name per file is enough.
    let mut tmp = file.clone().into_os_string();
    tmp.push(".uploading");
    let tmp = PathBuf::from(tmp);
    let _guard = ctx.storage_lock.lock().await;
    let new_file = !tokio::fs::try_exists(&file).await.unwrap_or(false);
    if new_file && ctx.stored_files.load(Ordering::Relaxed) >= MAX_STORED_FILES {
        warn!(%key, limit = MAX_STORED_FILES, "upload refused: the storage directory holds the maximum number of files");
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
        warn!(file = %file.display(), error = %e, "upload write failed; the stored file is unchanged");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if new_file {
        ctx.stored_files.fetch_add(1, Ordering::Relaxed);
    }
    info!(%key, bytes = body.len(), "stored upload");
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
                warn!(file = %file.display(), error = %e, "read failed");
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
mod tests {
    use super::*;

    fn temp_storage(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("uc3-http-{tag}-{}", std::process::id()))
    }

    async fn request(
        assets: Assets,
        static_dir: Option<PathBuf>,
        storage: PathBuf,
        raw: String,
    ) -> String {
        request_with(Arc::default(), assets, static_dir, storage, raw).await
    }

    async fn request_with(
        games: Arc<GameIds>,
        assets: Assets,
        static_dir: Option<PathBuf>,
        storage: PathBuf,
        raw: String,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_on(listener, assets, static_dir, storage, games));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(raw.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await.unwrap();
        String::from_utf8_lossy(&resp).into_owned()
    }

    async fn get(assets: Assets, static_dir: Option<PathBuf>, path: &str) -> String {
        let raw = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        request(assets, static_dir, temp_storage("none"), raw).await
    }

    /// Profiles and game records are stored and served back; other uploads are refused.
    #[tokio::test]
    async fn stores_uploads_under_allowed_prefixes() {
        let storage = temp_storage("store");
        let put = |path: &str, body: &str| {
            format!(
                "PUT {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let ok = request(
            Assets::new(),
            None,
            storage.clone(),
            put("/profiles/TESTER/profile.86", "PROFILE"),
        )
        .await;
        let bad = request(
            Assets::new(),
            None,
            storage.clone(),
            put("/net18.bin.psarc.crypt", "x"),
        )
        .await;
        let back = request(
            Assets::new(),
            None,
            storage.clone(),
            "GET /profiles/TESTER/profile.86 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
                .into(),
        )
        .await;
        std::fs::remove_dir_all(&storage).ok();
        assert!(
            ok.starts_with("HTTP/1.1 200") && ok.contains("\r\nEtag: \""),
            "{ok}"
        );
        assert!(bad.starts_with("HTTP/1.1 403"), "{bad}");
        assert!(
            back.starts_with("HTTP/1.1 200") && back.ends_with("PROFILE"),
            "{back}"
        );
    }

    /// Overlapping uploads of one profile (the client sends several within milliseconds) leave exactly
    /// one of the uploaded bodies, never a mix, and no temporary file.
    #[tokio::test]
    async fn overlapping_uploads_never_mix() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let storage = temp_storage("overlap");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_on(
            listener,
            Assets::new(),
            None,
            storage.clone(),
            Arc::default(),
        ));
        let bodies: Vec<String> = (0..16)
            .map(|i| {
                char::from(b'a' + i as u8)
                    .to_string()
                    .repeat(4000 + 997 * i)
            })
            .collect();
        let tasks: Vec<_> = bodies.iter().cloned().map(|body| tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            let raw = format!("PUT /profiles/TESTER/profile.86 HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            s.write_all(raw.as_bytes()).await.unwrap();
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.unwrap();
            assert!(resp.starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&resp));
        })).collect();
        for t in tasks {
            t.await.unwrap();
        }
        let dir = storage.join("profiles").join("TESTER");
        let stored = std::fs::read_to_string(dir.join("profile.86")).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        std::fs::remove_dir_all(&storage).ok();
        assert!(
            bodies.contains(&stored),
            "stored file is not one of the uploads ({} bytes)",
            stored.len()
        );
        assert_eq!(leftovers, ["profile.86"]);
    }

    /// Generated assets come from memory, with "Last-Modified" spelled exactly as the client expects (see
    /// module docs) and dated from their content.
    #[tokio::test]
    async fn serves_generated_assets_with_title_case_last_modified() {
        let asset = Asset::new(b"abc".to_vec());
        let date = httpdate::fmt_http_date(asset.last_modified);
        let text = get(
            Assets::from([("a/f.crypt".to_string(), asset)]),
            None,
            "/./a/f.crypt",
        )
        .await;
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(
            text.contains(&format!("\r\nLast-Modified: {date}\r\n")),
            "{text}"
        );
        assert!(text.contains("\r\nContent-Length: 3\r\n"), "{text}");
        assert!(text.ends_with("abc"), "{text}");
    }

    /// Paths that are not generated fall back to the optional static directory; generated ones win.
    #[tokio::test]
    async fn static_dir_is_a_fallback() {
        let dir = std::env::temp_dir().join(format!("uc3-http-static-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("languages/english")).unwrap();
        std::fs::write(dir.join("languages/english/motd.jpg"), b"jpg").unwrap();
        std::fs::write(dir.join("net.crypt"), b"stale file").unwrap();
        let assets = Assets::from([("net.crypt".to_string(), Asset::new(b"fresh".to_vec()))]);
        let motd = get(
            assets.clone(),
            Some(dir.clone()),
            "/languages/english/motd.jpg",
        )
        .await;
        let net = get(assets.clone(), Some(dir.clone()), "/net.crypt").await;
        let missing = get(assets, None, "/languages/english/motd.jpg").await;
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            motd.starts_with("HTTP/1.1 200") && motd.ends_with("jpg"),
            "{motd}"
        );
        assert!(net.ends_with("fresh"), "{net}");
        assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    }

    /// Only the client's key shapes are writable; a game record only for an issued id.
    #[test]
    fn only_client_key_shapes_are_writable() {
        let games = GameIds::default();
        games.issue("6ad76c5f0000");
        for ok in [
            "profiles/TESTER/profile.86",
            "profiles/a-b_c/profile.1",
            "games/6ad76c5f0000",
        ] {
            assert!(writable(ok, &games), "{ok}");
        }
        for bad in [
            "profiles/TESTER/other.86",
            "profiles/TESTER/profile.",
            "profiles/TESTER/profile.86/x",
            "profiles/TOO-LONG-ONLINE-ID-X/profile.86",
            "profiles/a.b/profile.86",
            "profiles/TESTER",
            "games/unissued",
            "games/6ad76c5f0000/x",
            "other/file",
        ] {
            assert!(!writable(bad, &games), "{bad}");
        }
    }

    /// Dates come from the content, so only an exact If-Modified-Since match gets a 304.
    #[tokio::test]
    async fn not_modified_only_on_an_exact_date() {
        let asset = Asset::new(b"abc".to_vec());
        let date = asset.last_modified;
        let assets = Assets::from([("f.crypt".to_string(), asset)]);
        let ask = |since: SystemTime| {
            format!(
                "GET /f.crypt HTTP/1.1\r\nHost: x\r\nIf-Modified-Since: {}\r\nConnection: close\r\n\r\n",
                httpdate::fmt_http_date(since)
            )
        };
        let same = request(assets.clone(), None, temp_storage("ims"), ask(date)).await;
        let later = request(
            assets,
            None,
            temp_storage("ims"),
            ask(date + Duration::from_secs(86_400)),
        )
        .await;
        assert!(same.starts_with("HTTP/1.1 304"), "{same}");
        assert!(later.starts_with("HTTP/1.1 200"), "{later}");
    }

    #[test]
    fn content_date_is_stable_per_content() {
        assert_eq!(content_date(b"same"), content_date(b"same"));
        assert_ne!(content_date(b"one"), content_date(b"two"));
        let secs = content_date(b"x")
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            secs >= 978_307_200,
            "dates are never near 0, which the client's cache check treats specially"
        );
    }

    #[test]
    fn path_style_requests_drop_the_bucket() {
        assert_eq!(object_path("/some.bucket/games/abc"), "/games/abc");
        assert_eq!(
            object_path("/b/profiles/X/profile.86"),
            "/profiles/X/profile.86"
        );
        assert_eq!(object_path("/games/abc"), "/games/abc");
        // An online id that equals an upload prefix stays a virtual-host key.
        assert_eq!(
            object_path("/profiles/games/profile.86"),
            "/profiles/games/profile.86"
        );
        assert_eq!(object_path("/eula/page-1.png"), "/eula/page-1.png");
        assert_eq!(
            object_path("/build/main/text1/2.networking.crypt"),
            "/build/main/text1/2.networking.crypt"
        );
    }

    /// A path-style upload is served back under its key, as the client reads it virtual-host style.
    #[tokio::test]
    async fn path_style_uploads_are_read_back_by_key() {
        let storage = temp_storage("pathstyle");
        let games = Arc::new(GameIds::default());
        games.issue("abc");
        let raw = "PUT /some.bucket/games/abc HTTP/1.1\r\nHost: y\r\nContent-Length: 4\r\nConnection: close\r\n\r\nGAME";
        let put = request_with(games, Assets::new(), None, storage.clone(), raw.into()).await;
        let back = request(
            Assets::new(),
            None,
            storage.clone(),
            "GET /games/abc HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".into(),
        )
        .await;
        std::fs::remove_dir_all(&storage).ok();
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        assert!(
            back.starts_with("HTTP/1.1 200") && back.ends_with("GAME"),
            "{back}"
        );
    }

    #[test]
    fn normalizes_game_paths() {
        assert_eq!(
            normalize("/./net.bin.psarc.crypt").as_deref(),
            Some("net.bin.psarc.crypt")
        );
        assert_eq!(
            normalize("/net-patch9.bin.psarc.crypt").as_deref(),
            Some("net-patch9.bin.psarc.crypt")
        );
        assert_eq!(normalize("/a/./b.jpg").as_deref(), Some("a/b.jpg"));
        assert_eq!(normalize("/../etc/passwd"), None);
        assert_eq!(normalize("/"), None);
        assert_eq!(normalize("/c:/x"), None);
    }
}
