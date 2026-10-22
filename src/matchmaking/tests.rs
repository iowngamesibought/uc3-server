use super::*;

fn player_id(name: &str) -> [u8; PLAYER_ID_LEN] {
    let mut n = [0u8; PLAYER_ID_LEN];
    n[..name.len()].copy_from_slice(name.as_bytes());
    n
}

const LAN: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

fn client(mm: &Matchmaking, name: &str) -> (ClientId, mpsc::UnboundedReceiver<Vec<u8>>) {
    client_at(mm, name, LAN)
}

fn client_at(
    mm: &Matchmaking,
    name: &str,
    ip: IpAddr,
) -> (ClientId, mpsc::UnboundedReceiver<Vec<u8>>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (mm.connect(name.into(), ip, tx).0, rx)
}

fn room_flags(room: u64, flags: u16) -> Vec<u8> {
    Rec::new(SET_ROOM_FLAGS, 0x10)
        .u16(4, flags)
        .u64(8, room)
        .done()
}

fn create(ptr: u32, max: u16) -> Vec<u8> {
    Rec::new(ROOM_CREATE, 0xe8)
        .u32(8, ptr)
        .u32(0xc, 7)
        .u32(0x14, 3)
        .u16(0x24, max)
        .u8(0x26, 4)
        .bytes(0x28, b"room")
        .bytes(0xa8, b"DATA")
        .done()
}

fn join(ptr: u32, room: u64) -> Vec<u8> {
    Rec::new(ROOM_JOIN, 0x58)
        .u32(8, ptr)
        .u8(0xc, 2)
        .u64(0x10, room)
        .bytes(0x18, b"JD")
        .done()
}

/// Creates a room for `name` and returns its id (consuming the RoomJoined).
fn host(
    mm: &Matchmaking,
    c: ClientId,
    rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    name: &str,
    ptr: u32,
    max: u16,
) -> u64 {
    mm.handle(c, &player_id(name), &create(ptr, max));
    be64(&rx.try_recv().unwrap(), 0x10)
}

fn drain(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<Vec<u8>> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn rooms(mm: &Matchmaking) -> usize {
    mm.lobby().rooms.len()
}

#[test]
fn reusing_a_room_object_ends_its_previous_membership() {
    let mm = Matchmaking::new();
    let (a, mut ra) = client(&mm, "alice");
    let (b, mut rb) = client(&mm, "bob");
    let room = host(&mm, a, &mut ra, "alice", 0x100, 4);
    mm.handle(b, &player_id("bob"), &join(0x200, room));
    drain(&mut ra);
    drain(&mut rb);
    // Bob's client resets room object 0x200 locally and creates a room with it: no RoomLeave.
    mm.handle(b, &player_id("bob"), &create(0x200, 4));
    let left = drain(&mut ra);
    assert!(
        left.iter()
            .any(|r| be32(r, 0) == ROOM_LEFT && be16(r, 0x10) == 2),
        "alice is told bob left"
    );
    assert_eq!(rooms(&mm), 2);
    assert_eq!(mm.lobby().rooms[&room].members.len(), 1);
    // A different room object keeps its membership: Alice creates a second room with 0x101.
    mm.handle(a, &player_id("alice"), &create(0x101, 4));
    assert_eq!(mm.lobby().rooms[&room].members.len(), 1);
    assert_eq!(rooms(&mm), 3);
}

#[test]
fn a_reconnect_replaces_the_old_session() {
    let mm = Matchmaking::new();
    let (a, mut ra) = client(&mm, "alice");
    let (b, mut rb) = client(&mm, "bob");
    let room = host(&mm, a, &mut ra, "alice", 0x100, 4);
    mm.handle(b, &player_id("bob"), &join(0x200, room));
    drain(&mut ra);
    // Bob's console reconnects before the old connection timed out.
    let (tx, _rb2) = mpsc::unbounded_channel();
    let (b2, _) = mm.connect("bob".into(), LAN, tx);
    assert_ne!(b, b2);
    assert!(
        drain(&mut ra).iter().any(|r| be32(r, 0) == ROOM_LEFT),
        "alice sees the old session leave"
    );
    assert!(!mm.lobby().clients.contains_key(&b));
    // The old connection's queue is closed and its late records are ignored.
    drain(&mut rb);
    assert!(matches!(
        rb.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    mm.handle(b, &player_id("bob"), &join(0x200, room));
    assert_eq!(mm.lobby().rooms[&room].members.len(), 1);
}

/// A connection claiming a connected player's id from another address does not end that
/// player's session or memberships.
#[test]
fn the_same_id_from_another_address_does_not_replace_a_session() {
    let mm = Matchmaking::new();
    let (a, mut ra) = client(&mm, "alice");
    let (b, _rb) = client(&mm, "bob");
    let room = host(&mm, a, &mut ra, "alice", 0x100, 4);
    mm.handle(b, &player_id("bob"), &join(0x200, room));
    drain(&mut ra);
    let (tx, _rx) = mpsc::unbounded_channel();
    let other: IpAddr = "192.168.1.99".parse().unwrap();
    let (b2, _) = mm.connect("bob".into(), other, tx);
    assert_ne!(b, b2);
    assert!(mm.lobby().clients.contains_key(&b) && mm.lobby().clients.contains_key(&b2));
    assert_eq!(mm.lobby().rooms[&room].members.len(), 2);
    assert!(drain(&mut ra).is_empty(), "alice sees no change");
}

/// Profiles are kept from a player's own address while in a played room, also after the flag
/// is cleared, and for UPLOAD_GRACE after leaving it.
#[test]
fn profiles_are_kept_around_a_match_with_another_console() {
    let mm = Matchmaking::new();
    let (a_ip, b_ip): (IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
    let (a, mut ra) = client_at(&mm, "alice", a_ip);
    let (b, _rb) = client_at(&mm, "bob", b_ip);
    assert!(!mm.keeps_profile("alice", a_ip), "signed in, no room");
    let room = host(&mm, a, &mut ra, "alice", 1, 4);
    mm.handle(b, &player_id("bob"), &join(2, room));
    assert!(!mm.keeps_profile("alice", a_ip), "a lobby is not a match");

    mm.handle(a, &player_id("alice"), &room_flags(room, 1));
    assert!(mm.keeps_profile("alice", a_ip) && mm.keeps_profile("bob", b_ip));
    assert!(
        !mm.keeps_profile("alice", b_ip),
        "only from the player's own address"
    );
    assert!(
        !mm.keeps_profile("carol", a_ip),
        "only for the player's own id"
    );
    mm.handle(a, &player_id("alice"), &room_flags(room, 0));
    assert!(
        mm.keeps_profile("alice", a_ip),
        "the match end clears the flag before the last uploads"
    );

    mm.handle(
        b,
        &player_id("bob"),
        &Rec::new(ROOM_LEAVE, 0x10).u64(8, room).done(),
    );
    assert!(mm.keeps_profile("bob", b_ip), "just after leaving");
    mm.disconnect(a);
    assert!(
        mm.keeps_profile("alice", a_ip),
        "a dropped session gets the same grace"
    );
    assert!(mm.lobby().rooms.is_empty());
    for until in mm.lobby().upload_permits.values_mut() {
        *until = Instant::now(); // as if the grace ran out
    }
    assert!(!mm.keeps_profile("alice", a_ip) && !mm.keeps_profile("bob", b_ip));
}

/// A room needs members on two addresses to count as a match: a game played alone, or two
/// sessions from one address, do not. A private room counts once a guest from elsewhere joins.
#[test]
fn a_match_needs_members_on_two_addresses() {
    let mm = Matchmaking::new();
    let (a, mut ra) = client(&mm, "alice");
    let (b, _rb) = client(&mm, "bob");
    let room = host(&mm, a, &mut ra, "alice", 1, 4);
    mm.handle(a, &player_id("alice"), &room_flags(room, 1));
    assert!(!mm.keeps_profile("alice", LAN), "alone");
    mm.handle(b, &player_id("bob"), &join(2, room));
    mm.handle(a, &player_id("alice"), &room_flags(room, 1));
    assert!(
        !mm.keeps_profile("alice", LAN) && !mm.keeps_profile("bob", LAN),
        "one address"
    );

    let guest: IpAddr = "10.0.0.9".parse().unwrap();
    let (c, _rc) = client_at(&mm, "carol", guest);
    mm.handle(c, &player_id("carol"), &join(3, room));
    assert!(mm.keeps_profile("carol", guest) && mm.keeps_profile("alice", LAN));
}

#[test]
fn the_owner_s_host_rank_picks_the_next_owner() {
    let mm = Matchmaking::new();
    let (a, mut ra) = client(&mm, "alice");
    let (b, mut rb) = client(&mm, "bob");
    let (c, mut rc) = client(&mm, "carol");
    let room = host(&mm, a, &mut ra, "alice", 1, 4);
    mm.handle(b, &player_id("bob"), &join(2, room));
    mm.handle(c, &player_id("carol"), &join(3, room));
    // Alice ranks Carol (3) before Bob (2); a ranking from a non-owner is ignored.
    let rank = |ids: &[u16]| {
        let mut r = Rec::new(HOST_RANK, 0x10 + 2 * ids.len());
        r.u16(4, ids.len() as u16).u64(8, room);
        for (i, id) in ids.iter().enumerate() {
            r.u16(0x10 + 2 * i, *id);
        }
        r.done()
    };
    mm.handle(a, &player_id("alice"), &rank(&[3, 2]));
    mm.handle(b, &player_id("bob"), &rank(&[2, 3]));
    drain(&mut rb);
    drain(&mut rc);
    mm.disconnect(a);
    let to_bob = drain(&mut rb);
    let owner = to_bob
        .iter()
        .find(|r| be32(r, 0) == OWNER_CHANGED)
        .expect("owner change sent");
    assert_eq!(be16(owner, 4), 3);
    assert_eq!(mm.lobby().rooms[&room].owner, 3);
}

#[test]
fn member_ids_skip_ids_in_use_after_wrapping() {
    let mm = Matchmaking::new();
    let (a, mut ra) = client(&mm, "alice");
    let (b, _rb) = client(&mm, "bob");
    let room = host(&mm, a, &mut ra, "alice", 1, 4);
    mm.lobby().rooms.get_mut(&room).unwrap().next_member = 1; // as if the counter wrapped
    mm.handle(b, &player_id("bob"), &join(2, room));
    let ids: Vec<u16> = mm.lobby().rooms[&room]
        .members
        .iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, [1, 2]);
}

/// A second connection of the same player closes the first one's session.
#[tokio::test]
async fn a_reconnect_closes_the_old_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mm = Arc::new(Matchmaking::new());
    let first = {
        let (mm, listener) = (mm.clone(), listener);
        tokio::spawn(async move {
            let (s, peer) = listener.accept().await.unwrap();
            let r = connection(s, peer, &mm).await;
            (r, listener)
        })
    };
    let hello = Rec::new(CLIENT_HELLO, 0x30)
        .bytes(8, &player_id("tester"))
        .done();
    let mut sh = [0u8; 0x10];
    let mut s1 = TcpStream::connect(addr).await.unwrap();
    s1.write_all(&hello).await.unwrap();
    s1.read_exact(&mut sh).await.unwrap();
    // The first session is registered before its ServerHello is sent; connect the second directly.
    let (tx, _rx) = mpsc::unbounded_channel();
    mm.connect("tester".into(), LAN, tx);
    let (ended, _listener) = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("old session ended")
        .unwrap();
    assert!(ended.unwrap_err().to_string().contains("replaced"));
    assert_eq!(mm.lobby().clients.len(), 1);
}

#[tokio::test]
async fn a_stalled_record_ends_the_session_and_its_rooms() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mm = Arc::new(Matchmaking::new());
    let server = {
        let mm = mm.clone();
        tokio::spawn(async move {
            let (s, peer) = listener.accept().await.unwrap();
            connection_with(s, peer, &mm, Duration::from_millis(300)).await
        })
    };
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        &Rec::new(CLIENT_HELLO, 0x30)
            .bytes(8, &player_id("tester"))
            .done(),
    )
    .await
    .unwrap();
    let mut sh = [0u8; 0x10];
    s.read_exact(&mut sh).await.unwrap();
    s.write_all(&create(5, 4)).await.unwrap();
    let mut joined = [0u8; 0xa0 + MEMBER_LEN];
    s.read_exact(&mut joined).await.unwrap();
    assert_eq!(rooms(&mm), 1);
    // Half a record, then silence: the session must end and the room must go.
    s.write_all(&create(6, 4)[..20]).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("session ended")
        .unwrap();
    assert!(ended.is_err());
    assert_eq!(rooms(&mm), 0);
    assert!(mm.lobby().clients.is_empty());
}

#[test]
fn records_have_the_client_sizes() {
    for (kind, len) in [
        (CLIENT_HELLO, 0x30),
        (ROOM_CREATE, 0xe8),
        (ROOM_JOIN, 0x58),
        (ROOM_SEARCH, 0x24),
        (MEMBER_SET_DATA, 0x50),
        (SET_ROOM_NAME, 0x90),
        (PING, 4),
        (CLIENT_HELLO2, 8),
    ] {
        assert_eq!(request_len(kind), Some(len));
    }
    assert_eq!(
        request_len(ROOM_JOINED),
        None,
        "server-only types are not accepted from clients"
    );
}

#[test]
fn create_search_join_leave() {
    let mm = Matchmaking::new();
    let (a, mut ra) = client(&mm, "alice");
    let (b, mut rb) = client(&mm, "bob");
    mm.handle(a, &player_id("alice"), &create(0x1000, 2));
    let joined = ra.try_recv().unwrap();
    assert_eq!(
        (
            be32(&joined, 0),
            be32(&joined, 8),
            be16(&joined, 0xc),
            be16(&joined, 0xe)
        ),
        (ROOM_JOINED, 0x1000, 1, 1)
    );
    let room = be64(&joined, 0x10);
    assert!(room != 0);
    assert_eq!(
        (be16(&joined, 0x18), be16(&joined, 0x1a), joined.len()),
        (2, 1, 0xa0 + MEMBER_LEN)
    );
    assert_eq!(&joined[0xa0..0xa5], b"alice");
    assert_eq!(&joined[0xa0 + 0x28..0xa0 + 0x2c], b"DATA");

    let search = Rec::new(ROOM_SEARCH, 0x24)
        .u32(8, 0x2000)
        .u32(0xc, 7)
        .u32(0x10, 3)
        .done();
    mm.handle(b, &player_id("bob"), &search);
    let res = rb.try_recv().unwrap();
    assert_eq!(
        (be32(&res, 0), be32(&res, 8), be32(&res, 0xc)),
        (ROOM_SEARCH_RESULT, 0x2000, 1)
    );
    assert_eq!(be64(&res, 0x10), room);
    assert_eq!(&res[0x10 + 0x14..0x10 + 0x19], b"alice");
    assert_eq!((be16(&res, 0x10 + 0xc), be16(&res, 0x10 + 0x10)), (1, 2));

    let join = Rec::new(ROOM_JOIN, 0x58)
        .u32(8, 0x3000)
        .u8(0xc, 2)
        .u64(0x10, room)
        .bytes(0x18, b"BD")
        .done();
    mm.handle(b, &player_id("bob"), &join);
    let jb = rb.try_recv().unwrap();
    assert_eq!(
        (
            be32(&jb, 0),
            be64(&jb, 0x10),
            be16(&jb, 0xe),
            be16(&jb, 0x1a)
        ),
        (ROOM_JOINED, room, 2, 2)
    );
    let note = ra.try_recv().unwrap();
    assert_eq!(
        (be32(&note, 0), note.len(), be16(&note, 0x34), note[0x37]),
        (MEMBER_JOINED, 0x78, 2, 2)
    );
    assert_eq!(&note[0x10..0x13], b"bob");

    // Full now: a third player is refused with room id 0.
    let (c, mut rc) = client(&mm, "carol");
    mm.handle(c, &player_id("carol"), &join);
    let jc = rc.try_recv().unwrap();
    assert_eq!((be32(&jc, 0), be64(&jc, 0x10)), (ROOM_JOINED, 0));

    // The owner leaves: bob sees RoomLeft and becomes the owner.
    mm.disconnect(a);
    let left = rb.try_recv().unwrap();
    assert_eq!(
        (be32(&left, 0), left.len(), be16(&left, 0x10)),
        (ROOM_LEFT, 0x18, 1)
    );
    let owner = rb.try_recv().unwrap();
    assert_eq!((be32(&owner, 0), be16(&owner, 4)), (OWNER_CHANGED, 2));
    mm.handle(
        b,
        &player_id("bob"),
        &Rec::new(ROOM_LEAVE, 0x10).u64(8, room).done(),
    );
    assert!(mm.lobby.lock().unwrap().rooms.is_empty());
}

#[test]
fn search_only_finds_matching_rooms() {
    let mm = Matchmaking::new();
    let (a, _ra) = client(&mm, "alice");
    let (b, mut rb) = client(&mm, "bob");
    mm.handle(a, &player_id("alice"), &create(1, 4));
    mm.handle(
        b,
        &player_id("bob"),
        &Rec::new(ROOM_SEARCH, 0x24).u32(0xc, 7).u32(0x10, 99).done(),
    );
    assert_eq!(be32(&rb.try_recv().unwrap(), 0xc), 0);
}

/// Room flag bit 0 hides a room from searches (a match in progress, a private custom game) but
/// does not stop joins (invites into a custom lobby arrive after the flag is set).
#[test]
fn closed_rooms_are_hidden_from_searches_but_joinable() {
    let mm = Matchmaking::new();
    let (a, mut ra) = client(&mm, "alice");
    let (b, mut rb) = client(&mm, "bob");
    mm.handle(a, &player_id("alice"), &create(0x1000, 4));
    let room = be64(&ra.try_recv().unwrap(), 0x10);
    let flags = |v: u16| Rec::new(SET_ROOM_FLAGS, 0x10).u16(4, v).u64(8, room).done();
    let search = Rec::new(ROOM_SEARCH, 0x24).u32(0xc, 7).u32(0x10, 3).done();

    mm.handle(a, &player_id("alice"), &flags(1));
    mm.handle(b, &player_id("bob"), &search);
    assert_eq!(
        be32(&rb.try_recv().unwrap(), 0xc),
        0,
        "closed room offered to a search"
    );

    mm.handle(b, &player_id("bob"), &join(0x3000, room));
    let jb = rb.try_recv().unwrap();
    assert_eq!(
        (be32(&jb, 0), be64(&jb, 0x10)),
        (ROOM_JOINED, room),
        "join into a closed room refused"
    );

    let (c, mut rc) = client(&mm, "carol");
    mm.handle(a, &player_id("alice"), &flags(0));
    mm.handle(c, &player_id("carol"), &search);
    let found = rc.try_recv().unwrap();
    assert_eq!(
        (be32(&found, 0xc), be64(&found, 0x10)),
        (1, room),
        "reopened room not offered"
    );
}

#[test]
fn search_answers_fit_the_clients_result_array() {
    let mm = Matchmaking::new();
    let mut hosts = Vec::new();
    for i in 0..MAX_SEARCH_RESULTS + 3 {
        let name = format!("host{i}");
        let (h, rh) = client(&mm, &name);
        mm.handle(h, &player_id(&name), &create(1, 4));
        hosts.push(rh);
    }
    let (b, mut rb) = client(&mm, "bob");
    mm.handle(
        b,
        &player_id("bob"),
        &Rec::new(ROOM_SEARCH, 0x24).u32(0xc, 7).u32(0x10, 3).done(),
    );
    let res = rb.try_recv().unwrap();
    assert_eq!(
        (be32(&res, 0xc), res.len()),
        (24, 0x10 + 24 * SEARCH_ENTRY_LEN)
    );
}

/// Fuller rooms come first; rooms with as many members come in random order.
#[test]
fn searches_list_fuller_rooms_first() {
    let mm = Matchmaking::new();
    let search = Rec::new(ROOM_SEARCH, 0x24).u32(0xc, 7).u32(0x10, 3).done();
    let (a, mut ra) = client(&mm, "alice");
    let (b, mut rb) = client(&mm, "bob");
    let (g, _rg) = client(&mm, "guest");
    let lone = host(&mm, a, &mut ra, "alice", 1, 4);
    let busy = host(&mm, b, &mut rb, "bob", 1, 4);
    mm.handle(g, &player_id("guest"), &join(1, busy));
    let (s, mut rs) = client(&mm, "searcher");
    mm.handle(s, &player_id("searcher"), &search);
    let res = rs.try_recv().unwrap();
    assert_eq!(
        (
            be32(&res, 0xc),
            be64(&res, 0x10),
            be64(&res, 0x10 + SEARCH_ENTRY_LEN)
        ),
        (2, busy, lone),
        "the room with more members first, although it is newer"
    );

    let mut hosts = Vec::new();
    for i in 0..10 {
        let name = format!("host{i}");
        let (h, rh) = client(&mm, &name);
        mm.handle(h, &player_id(&name), &create(1, 4));
        hosts.push(rh);
    }
    let firsts: HashSet<u64> = (0..20)
        .map(|_| {
            mm.handle(s, &player_id("searcher"), &search);
            be64(&rs.try_recv().unwrap(), 0x10 + SEARCH_ENTRY_LEN)
        })
        .collect();
    assert!(firsts.len() > 1, "single-member rooms come in random order");
}

/// The client has 4 room objects: a fifth room at once is refused, and reusing a room object
/// still works at the limit.
#[test]
fn a_session_is_in_at_most_four_rooms() {
    let mm = Matchmaking::new();
    let (a, mut ra) = client(&mm, "alice");
    let (b, mut rb) = client(&mm, "bob");
    for ptr in 1..=4 {
        host(&mm, a, &mut ra, "alice", ptr, 4);
    }
    let room = host(&mm, b, &mut rb, "bob", 1, 4);
    mm.handle(a, &player_id("alice"), &create(5, 4));
    mm.handle(a, &player_id("alice"), &join(5, room));
    let refused = drain(&mut ra);
    assert_eq!(refused.len(), 2);
    assert!(
        refused
            .iter()
            .all(|r| be32(r, 0) == ROOM_JOINED && be64(r, 0x10) == 0),
        "both refused with room id 0"
    );
    assert_eq!((rooms(&mm), mm.lobby().rooms[&room].members.len()), (5, 1));
    // Room object 4 is reused: its own room ends first, so the join is allowed.
    mm.handle(a, &player_id("alice"), &join(4, room));
    assert_eq!((rooms(&mm), mm.lobby().rooms[&room].members.len()), (4, 2));
}

#[tokio::test]
async fn handshake_over_tcp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mm = Arc::new(Matchmaking::new());
    tokio::spawn(async move {
        let (s, peer) = listener.accept().await.unwrap();
        let _ = connection(s, peer, &mm).await;
    });
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        &Rec::new(CLIENT_HELLO, 0x30)
            .bytes(8, &player_id("tester"))
            .done(),
    )
    .await
    .unwrap();
    let mut sh = [0u8; 0x10];
    s.read_exact(&mut sh).await.unwrap();
    assert_eq!(be32(&sh, 0), SERVER_HELLO);
    s.write_all(&Rec::new(CLIENT_HELLO2, 8).done())
        .await
        .unwrap();
    s.write_all(&Rec::new(PING, 4).done()).await.unwrap();
    s.write_all(&create(5, 4)).await.unwrap();
    let mut head = [0u8; 0xa0];
    s.read_exact(&mut head).await.unwrap();
    assert_eq!((be32(&head, 0), be32(&head, 8)), (ROOM_JOINED, 5));
}
