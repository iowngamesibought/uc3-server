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

/// Matchmaking state in which TESTER's profile uploads from this machine are kept.
fn tester_in_a_match() -> Arc<Matchmaking> {
    let mm = Matchmaking::new();
    mm.permit_uploads_for_test("TESTER", std::net::Ipv4Addr::LOCALHOST.into());
    Arc::new(mm)
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
    tokio::spawn(serve_on(
        listener,
        assets,
        static_dir,
        storage,
        games,
        tester_in_a_match(),
    ));
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
        "GET /profiles/TESTER/profile.86 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".into(),
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
        tester_in_a_match(),
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
    assert_eq!(profile_owner("profiles/TESTER/profile.86"), Some("TESTER"));
    assert_eq!(profile_owner("games/6ad76c5f0000"), None);
}

/// A profile upload from a player who isn't in or just out of a match is answered like a kept
/// one, so the client sees no error, but nothing is stored.
#[tokio::test]
async fn profiles_outside_a_match_are_acknowledged_but_not_kept() {
    let storage = temp_storage("notkept");
    let put = request(
        Assets::new(),
        None,
        storage.clone(),
        "PUT /profiles/OTHER/profile.86 HTTP/1.1\r\nHost: x\r\nContent-Length: 7\r\nConnection: close\r\n\r\nPROFILE"
            .into(),
    )
    .await;
    let back = request(
        Assets::new(),
        None,
        storage.clone(),
        "GET /profiles/OTHER/profile.86 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".into(),
    )
    .await;
    std::fs::remove_dir_all(&storage).ok();
    assert!(
        put.starts_with("HTTP/1.1 200") && put.contains("\r\nEtag: \""),
        "{put}"
    );
    assert!(back.starts_with("HTTP/1.1 404"), "{back}");
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
