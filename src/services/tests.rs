use super::*;

fn peer() -> SocketAddr {
    "192.168.1.2:50000".parse().unwrap()
}

#[test]
fn replies_fit_the_client_buffers() {
    let s = Services::new(Options::default());
    let cases: [(&str, &[u8]); 9] = [
        ("ticket-server", b"\x00\x04abcd"),
        ("report-server", b"is-banned TESTER\n"),
        ("report-server", b"report A B cheating\n"),
        (
            "heartbeat-server",
            b"heartbeat TESTER menu,0,0,0,0,0,0,0,0,0,0,0,0,0,0,x\n",
        ),
        ("invite-server", b"invite-list TESTER\n"),
        ("leaderboard-server", b"leaderboard-get 3 1"),
        ("gamelist-server", b"new-id\n"),
        ("vulgarity-server", b"filter hello\n"),
        ("unknown-server", b"anything"),
    ];
    for (svc, req) in cases {
        let r = s.handle(svc, req, peer());
        let limit = REPLY_LIMITS
            .iter()
            .find(|(n, _)| *n == svc)
            .map_or(DEFAULT_LIMIT, |l| l.1);
        assert!(!r.is_empty() && r.len() < limit, "{svc}");
    }
}

#[test]
fn ban_check_says_not_banned_and_lists_are_empty() {
    let s = Services::new(Options::default());
    assert!(
        !s.handle("report-server", b"is-banned X", peer())
            .starts_with(b"+")
    );
    assert_eq!(
        s.handle(
            "leaderboard-server",
            b"leaderboard-range 1 1 10 1\n",
            peer()
        ),
        b"\n"
    );
    assert_eq!(s.handle("invite-server", b"invite-list X\n", peer()), b"\n");
}

#[test]
fn game_ids_are_unique_and_short() {
    let s = Services::new(Options::default());
    let a = s.handle("gamelist-server", b"new-id\n", peer());
    let b = s.handle("gamelist-server", b"new-id\n", peer());
    assert!(a.starts_with(b"+") && a.len() < 0x20 && a != b);
    let id = |r: &[u8]| String::from_utf8_lossy(&r[1..]).trim_end().to_string();
    assert!(s.game_ids().issued(&id(&a)) && s.game_ids().issued(&id(&b)));
    assert!(!s.game_ids().issued("abc"));
}
