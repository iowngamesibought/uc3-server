//! The matchmaking ("session manager") service: the client's own room protocol on plain TCP (config
//! list +0x5c, port 6303). UC3 does not use the platform's room service; rooms live here.
//!
//! The stream is a sequence of fixed-size big-endian records, each starting with a u32 type
//! (layouts read from the client's session manager):
//!   client -> server: 0x12d ClientHello (0x30), 0x12f RoomCreate (0xe8), 0x130 RoomJoin (0x58),
//!     0x133 RoomLeave, 0x135 RoomSearch (0x24), 0x137 Kickout, 0x13a MemberSetData (0x50),
//!     0x13c Promote, 0x13e SetAttrFlags, 0x140 SetRoomFlags, 0x142 HostRank (0x10 + 2n),
//!     0x143 SetRoomName (0x90), 0x145 Ping (4), 0x146 ClientHello2 (8); the rest are 0x10.
//!   server -> client: 0x12e ServerHello (0x10), 0x131 RoomJoined (0xa0 + n*0x68),
//!     0x132 MemberJoined (0x78), 0x134 RoomLeft (0x18), 0x136 RoomSearchResult (0x10 + n*0x38),
//!     0x138 Kickedout, 0x139 RoomDestroyed, 0x13b MemberUpdatedData (0x50), 0x13d OwnerChanged,
//!     0x13f UpdatedAttrFlags, 0x141 UpdatedRoomFlags, 0x144 UpdatedRoomName (0x90).
//! The client's parser stops at a type it does not handle, so only those types are ever sent.
//!
//! Session lifecycle (one TCP connection = one session):
//!   * A session is registered before its ServerHello is queued. A newer connection of the same
//!     online id from the same IP address replaces it: the old session's memberships end and its
//!     connection is closed. The online id is not authenticated, so a connection from another
//!     address never replaces a session; both stay until the older one times out.
//!   * It ends on the first of: read error, no complete record for 90 s (the client pings every
//!     30 s), a write failing or stalling for 30 s, or replacement. Cleanup runs once: the session
//!     leaves every room, and the other members are told (RoomLeft, then OwnerChanged if needed).
//!   * Memberships are keyed by the client's room object (the pointer in RoomCreate/RoomJoin). The
//!     client resets a room object without a RoomLeave before reusing it, so a create/join from an
//!     object that still holds a membership ends that membership first.
//!   * When the owner leaves, the best-ranked remaining member of the owner's last HostRank takes
//!     over, else the longest-standing member; an empty room is removed. Rooms hold at most 10
//!     members (the client's member slots).
//!   * A session is in at most 4 rooms at once (the client's room objects). A create or join
//!     beyond that is refused; a real client never sends one.
//!
//! Matching (as observed on real consoles):
//!   * A room matches a search when its mode (RoomCreate +0x0c / RoomSearch +0x0c) and playlist
//!     (RoomCreate +0x14 / RoomSearch +0x10) both equal the search's, it has a free slot, and its
//!     room flag bit 0 is clear. Mode and playlist are opaque match keys: the server never
//!     interprets them. Seen so far: Find Game competitive, Find Game co-op, custom games (the mode
//!     changes with the chosen game type) and the invite party each use their own pair, so they
//!     never meet in a search.
//!   * A search lists fuller rooms first, so players gather in one lobby, and rooms with as many
//!     members in random order, so rooms opened first cannot keep later ones out of the 24 results
//!     (the client's result array).
//!   * Room flag bit 0 means "not open to searches": consoles set it at every round start and clear
//!     it after the match, custom games set it right after creation (private), and an owner sets it
//!     before abandoning a room. Joins ignore it: an invited player joins a custom lobby after the
//!     flag is set.
//!   * Find Game first looks for an "optimal" game, then for "any" game: each search round
//!     changes RoomSearch +0x18 (5, 10, 0: a skill-like value), widens the range at +0x1c/+0x1e and,
//!     in the later rounds, clears the region at +0x20 ("us\0\x01" → 0). These fields, and the
//!     matching RoomCreate fields (+0x10 region, +0x1c), are **ignored for now**: any room with the
//!     same mode and playlist matches in every round, which suits a small player base. Matching by
//!     region and skill could use them later.
//!   * When nothing is found, the console hosts. A host abandons a lobby that stays below the
//!     playlist's minimum player count for 12 s, then searches again; the cycle is the game's own.
//!     The minimum comes from the online config (6 for most competitive playlists, 2 for co-op) and
//!     can be lowered with `[playlists] min_players` (see dc::set_min_players).
//!
//! Profile uploads: the client uploads its whole profile at sign-in, in the menus, and around every
//! match, and the uploads are not authenticated. The HTTP side therefore keeps a profile only when
//! it comes from an address that has a session under that online id in a played room, or had one
//! that left such a room less than [`UPLOAD_GRACE`] ago: a match's last uploads arrive just before
//! or just after leaving, and after the room flag is cleared. A room is played once it is closed to
//! searches while its members are on at least two addresses (a Find Game match started, or a
//! custom game with a guest), and stays played. Every other profile upload is acknowledged but not
//! kept: the next match uploads the whole profile again, so menu changes are kept once the player
//! finishes a match with another console. Room membership is no proof of identity either (see
//! "Trust"); this only limits when and from where a profile can be replaced.
//!
//! Trust: every member may set the room-wide attr and room flags (the protocol's update records
//! carry no member id, and guests do set them: co-op round starts, a new owner closing the room).
//! The server keeps the last value and relays it. A modified client could therefore close, reopen
//! or relabel a room it is in; restricting these to the owner, or validating them against the
//! lobby phase, is where tracking can be hardened later. Kicks are accepted from the owner only but
//! were never seen from a console.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, RandomState};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, info, warn};

use crate::framed::hex;
use crate::listen::{Listener, Throttle};

const CLIENT_HELLO: u32 = 0x12d;
const SERVER_HELLO: u32 = 0x12e;
const ROOM_CREATE: u32 = 0x12f;
const ROOM_JOIN: u32 = 0x130;
const ROOM_JOINED: u32 = 0x131;
const MEMBER_JOINED: u32 = 0x132;
const ROOM_LEAVE: u32 = 0x133;
const ROOM_LEFT: u32 = 0x134;
const ROOM_SEARCH: u32 = 0x135;
const ROOM_SEARCH_RESULT: u32 = 0x136;
const KICKOUT: u32 = 0x137;
const KICKEDOUT: u32 = 0x138;
const MEMBER_SET_DATA: u32 = 0x13a;
const MEMBER_UPDATED_DATA: u32 = 0x13b;
const PROMOTE: u32 = 0x13c;
const OWNER_CHANGED: u32 = 0x13d;
const SET_ATTR_FLAGS: u32 = 0x13e;
const UPDATED_ATTR_FLAGS: u32 = 0x13f;
const SET_ROOM_FLAGS: u32 = 0x140;
const UPDATED_ROOM_FLAGS: u32 = 0x141;
const HOST_RANK: u32 = 0x142;
const SET_ROOM_NAME: u32 = 0x143;
const UPDATED_ROOM_NAME: u32 = 0x144;
const PING: u32 = 0x145;
const CLIENT_HELLO2: u32 = 0x146;

const PLAYER_ID_LEN: usize = 0x24;
const DATA_LEN: usize = 0x40;
const NAME_LEN: usize = 0x80;
const MEMBER_LEN: usize = 0x68;
const SEARCH_ENTRY_LEN: usize = 0x38;
/// The client receives into a 0x1000-byte buffer and needs a whole record there.
const CLIENT_BUFFER: usize = 0x1000;
/// The client copies every entry of a RoomSearchResult into a 24-slot array of its room object
/// without a bound check; a 25th entry would overwrite the object's member slots.
const MAX_SEARCH_RESULTS: usize = 24;
/// The client's room object has 10 member slots; an 11th member would be dropped there without
/// signaling. Ten records also fit one RoomJoined in the client's receive buffer.
const MAX_MEMBERS: usize = 10;
const _: () = assert!(0xa0 + MAX_MEMBERS * MEMBER_LEN <= CLIENT_BUFFER);
/// The client has 4 room objects and each holds one membership (see `Member::slot`), so a session
/// never needs more rooms at once.
const MAX_ROOMS_PER_SESSION: usize = 4;
/// Room flag bit 0: the room is not open to searches (see "Matching" in the module docs).
const ROOM_FLAG_CLOSED: u16 = 1;
/// The client pings every 30 s. Three missed pings drop the connection, so a vanished console's
/// rooms leave the search results quickly instead of sending joiners to a dead host.
const IDLE: Duration = Duration::from_secs(90);
/// A record not written within this time means the client stopped reading: the session ends.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after leaving a played room a player's profile uploads are still kept (see "Profile
/// uploads"). In tests on consoles the last upload came within 0.1 s of leaving.
const UPLOAD_GRACE: Duration = Duration::from_secs(15);

/// Size of a client record by type; HostRank's length comes from its count field.
fn request_len(kind: u32) -> Option<usize> {
    Some(match kind {
        CLIENT_HELLO => 0x30,
        ROOM_CREATE => 0xe8,
        ROOM_JOIN => 0x58,
        ROOM_SEARCH => 0x24,
        MEMBER_SET_DATA => 0x50,
        SET_ROOM_NAME => 0x90,
        PING => 4,
        CLIENT_HELLO2 => 8,
        ROOM_LEAVE | KICKOUT | PROMOTE | SET_ATTR_FLAGS | SET_ROOM_FLAGS | HOST_RANK => 0x10,
        _ => return None,
    })
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// A record under construction: zero-filled, big-endian fields at fixed offsets.
struct Rec(Vec<u8>);

impl Rec {
    fn new(kind: u32, len: usize) -> Self {
        let mut r = Rec(vec![0; len]);
        r.u32(0, kind);
        r
    }
    fn u8(&mut self, at: usize, v: u8) -> &mut Self {
        self.0[at] = v;
        self
    }
    fn u16(&mut self, at: usize, v: u16) -> &mut Self {
        self.0[at..at + 2].copy_from_slice(&v.to_be_bytes());
        self
    }
    fn u32(&mut self, at: usize, v: u32) -> &mut Self {
        self.0[at..at + 4].copy_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, at: usize, v: u64) -> &mut Self {
        self.0[at..at + 8].copy_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(&mut self, at: usize, v: &[u8]) -> &mut Self {
        self.0[at..at + v.len()].copy_from_slice(v);
        self
    }
    fn done(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

/// Name of a server record type, for the log.
fn record_name(kind: u32) -> &'static str {
    match kind {
        SERVER_HELLO => "ServerHello",
        ROOM_JOINED => "RoomJoined",
        MEMBER_JOINED => "MemberJoined",
        ROOM_LEFT => "RoomLeft",
        ROOM_SEARCH_RESULT => "RoomSearchResult",
        KICKEDOUT => "Kickedout",
        MEMBER_UPDATED_DATA => "MemberUpdatedData",
        OWNER_CHANGED => "OwnerChanged",
        UPDATED_ATTR_FLAGS => "UpdatedAttrFlags",
        UPDATED_ROOM_FLAGS => "UpdatedRoomFlags",
        UPDATED_ROOM_NAME => "UpdatedRoomName",
        _ => "?",
    }
}

/// The room id a server record refers to: +0x10 in RoomJoined, +8 in the room notices; 0 for
/// ServerHello and RoomSearchResult.
fn room_of(rec: &[u8]) -> u64 {
    match be32(rec, 0) {
        ROOM_JOINED => be64(rec, 0x10),
        SERVER_HELLO | ROOM_SEARCH_RESULT => 0,
        _ => be64(rec, 8),
    }
}

/// The online id at the start of a player id: up to 16 characters, NUL-padded.
fn online_id(player_id: &[u8]) -> String {
    let id = &player_id[..16];
    let end = id.iter().position(|&b| b == 0).unwrap_or(16);
    String::from_utf8_lossy(&id[..end]).into_owned()
}

type ClientId = u64;

struct Member {
    id: u16,
    client: ClientId,
    /// The client's room object for this membership (the pointer it sent in RoomCreate/RoomJoin).
    /// The client reuses room objects and resets them locally without a RoomLeave, so a new
    /// create/join from the same object ends this membership.
    slot: u32,
    player_id: [u8; PLAYER_ID_LEN],
    flags: u8,
    data_len: u8,
    data: [u8; DATA_LEN],
}

impl Member {
    /// The 0x68-byte member record of RoomJoined: player id, member id, flags, data length, data.
    fn record(&self) -> [u8; MEMBER_LEN] {
        let mut r = [0u8; MEMBER_LEN];
        r[..PLAYER_ID_LEN].copy_from_slice(&self.player_id);
        r[0x24..0x26].copy_from_slice(&self.id.to_be_bytes());
        r[0x26] = self.flags;
        r[0x27] = self.data_len;
        r[0x28..].copy_from_slice(&self.data);
        r
    }
}

struct Room {
    id: u64,
    owner: u16,
    max: u16,
    /// Opaque match keys from RoomCreate +0x0c and +0x14, compared with RoomSearch +0x0c and +0x10
    /// (see "Matching" in the module docs).
    mode: u32,
    playlist: u32,
    room_flags: u16,
    attr_flags: u8,
    name: [u8; NAME_LEN],
    next_member: u16,
    members: Vec<Member>,
    /// The owner's latest HostRank: member ids, best host first. Used to pick the next owner.
    host_rank: Vec<u16>,
    /// Closed to searches at some point while its members were on at least two addresses (see
    /// "Profile uploads").
    played: bool,
}

impl Room {
    /// Whether a search for `mode` / `playlist` may return this room (see "Matching").
    fn matches_search(&self, mode: u32, playlist: u32) -> bool {
        self.mode == mode
            && self.playlist == playlist
            && self.room_flags & ROOM_FLAG_CLOSED == 0
            && self.members.len() < self.max as usize
    }

    /// A member id not in use (ids wrap at u16; 0 is never used).
    fn allocate_member_id(&mut self) -> u16 {
        loop {
            let id = self.next_member;
            self.next_member = self.next_member.wrapping_add(1).max(1);
            if !self.members.iter().any(|m| m.id == id) {
                return id;
            }
        }
    }

    /// After the owner left: the best-ranked member still present, else the longest-standing one.
    fn successor(&self) -> Option<u16> {
        self.host_rank
            .iter()
            .copied()
            .find(|id| self.members.iter().any(|m| m.id == *id))
            .or_else(|| self.members.first().map(|m| m.id))
    }

    fn member_by_client(&self, client: ClientId) -> Option<&Member> {
        self.members.iter().find(|m| m.client == client)
    }
    fn others(&self, except: ClientId) -> Vec<ClientId> {
        self.members
            .iter()
            .map(|m| m.client)
            .filter(|c| *c != except)
            .collect()
    }
    fn all(&self) -> Vec<ClientId> {
        self.members.iter().map(|m| m.client).collect()
    }
    fn joined(&self, request_ptr: u32, you: u16) -> Vec<u8> {
        let mut r = Rec::new(ROOM_JOINED, 0xa0 + self.members.len() * MEMBER_LEN);
        r.u32(8, request_ptr)
            .u16(0xc, self.owner)
            .u16(0xe, you)
            .u64(0x10, self.id)
            .u16(0x18, self.max)
            .u16(0x1a, self.members.len() as u16)
            .bytes(0x20, &self.name);
        for (i, m) in self.members.iter().enumerate() {
            r.bytes(0xa0 + i * MEMBER_LEN, &m.record());
        }
        r.done()
    }
    /// The client keeps only +0x00 (room id), +0x0c and +0x10 (u16 each) and +0x14 (the owner's player
    /// id, which the joiner connects to). The client was not seen reading +0x0c / +0x10 back;
    /// member count and maximum there are a guess.
    fn search_entry(&self) -> [u8; SEARCH_ENTRY_LEN] {
        let mut e = [0u8; SEARCH_ENTRY_LEN];
        e[..8].copy_from_slice(&self.id.to_be_bytes());
        e[0xc..0xe].copy_from_slice(&(self.members.len() as u16).to_be_bytes());
        e[0x10..0x12].copy_from_slice(&self.max.to_be_bytes());
        if let Some(owner) = self.members.iter().find(|m| m.id == self.owner) {
            e[0x14..0x14 + PLAYER_ID_LEN].copy_from_slice(&owner.player_id);
        }
        e
    }
}

/// A connected client: its online id, its outgoing queue, and a signal that ends its connection
/// when the server drops it (a newer connection of the same player).
struct Client {
    name: String,
    ip: IpAddr,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    evicted: Arc<Notify>,
}

#[derive(Default)]
struct Lobby {
    next_client: ClientId,
    next_room: u64,
    clients: HashMap<ClientId, Client>,
    rooms: HashMap<u64, Room>,
    /// (online id, address) that left a played room, and until when their profile uploads are
    /// kept. Expired entries are dropped whenever one is added.
    upload_permits: HashMap<(String, IpAddr), Instant>,
}

impl Lobby {
    fn send(&self, to: &[ClientId], rec: &[u8]) {
        for c in to {
            if let Some(client) = self.clients.get(c) {
                debug!(to = %client.name, record = record_name(be32(rec, 0)), room = room_of(rec), len = rec.len(), "sent");
                let _ = client.tx.send(rec.to_vec());
            }
        }
    }

    fn rooms_of(&self, client: ClientId) -> Vec<u64> {
        self.rooms
            .values()
            .filter(|r| r.member_by_client(client).is_some())
            .map(|r| r.id)
            .collect()
    }

    /// Ends every membership of `client` (it disconnected or was replaced).
    fn leave_all(&mut self, client: ClientId) {
        for r in self.rooms_of(client) {
            self.leave(r, client, false);
        }
    }

    /// Ends the client's membership held by room object `slot`, if any (see `Member::slot`).
    fn leave_slot(&mut self, client: ClientId, slot: u32) {
        let stale: Vec<u64> = self
            .rooms
            .values()
            .filter(|r| {
                r.members
                    .iter()
                    .any(|m| m.client == client && m.slot == slot)
            })
            .map(|r| r.id)
            .collect();
        for r in stale {
            info!(
                room = r,
                slot = format!("{slot:#x}"),
                "room object reused: ending its previous membership"
            );
            self.leave(r, client, false);
        }
    }

    /// Removes a client from a room and tells the others; the room goes when it is empty.
    fn leave(&mut self, room_id: u64, client: ClientId, kicked: bool) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        let Some(pos) = room.members.iter().position(|m| m.client == client) else {
            return;
        };
        let gone = room.members.remove(pos);
        let others = room.all();
        let mut new_owner = None;
        if room.owner == gone.id {
            if let Some(next) = room.successor() {
                room.owner = next;
                new_owner = Some(next);
            }
        }
        room.host_rank.retain(|id| *id != gone.id);
        let empty = room.members.is_empty();
        let played = room.played;
        info!(
            room = room_id,
            member = gone.id,
            kicked,
            remaining = others.len(),
            "member left"
        );
        if played {
            self.permit_uploads(client);
        }
        if kicked {
            self.send(&[client], &Rec::new(KICKEDOUT, 0x10).u64(8, room_id).done());
        }
        self.send(
            &others,
            &Rec::new(ROOM_LEFT, 0x18)
                .u64(8, room_id)
                .u16(0x10, gone.id)
                .done(),
        );
        if let Some(owner) = new_owner {
            self.send(
                &others,
                &Rec::new(OWNER_CHANGED, 0x10)
                    .u16(4, owner)
                    .u64(8, room_id)
                    .done(),
            );
        }
        if empty {
            self.rooms.remove(&room_id);
            info!(room = room_id, "room closed");
        }
    }

    /// Marks a room played once it is closed to searches while its members are on at least two
    /// addresses (see "Profile uploads").
    fn check_played(&mut self, room_id: u64) {
        let Some(room) = self.rooms.get(&room_id) else {
            return;
        };
        if room.played || room.room_flags & ROOM_FLAG_CLOSED == 0 {
            return;
        }
        let addresses: HashSet<IpAddr> = room
            .members
            .iter()
            .filter_map(|m| self.clients.get(&m.client).map(|c| c.ip))
            .collect();
        if addresses.len() < 2 {
            return;
        }
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.played = true;
            info!(
                room = room_id,
                members = room.members.len(),
                "room played: its members' profile uploads are kept"
            );
        }
    }

    /// Keeps `client`'s profile uploads for UPLOAD_GRACE after it left a played room.
    fn permit_uploads(&mut self, client: ClientId) {
        let Some(c) = self.clients.get(&client) else {
            return;
        };
        let now = Instant::now();
        self.upload_permits.retain(|_, until| *until > now);
        self.upload_permits
            .insert((c.name.clone(), c.ip), now + UPLOAD_GRACE);
    }
}

pub struct Matchmaking {
    lobby: Mutex<Lobby>,
    /// Matchmaking needs no key, so anyone can trigger its warnings: they are throttled.
    warnings: Throttle,
}

impl Matchmaking {
    pub fn new() -> Self {
        Self {
            lobby: Mutex::new(Lobby {
                next_room: 1,
                ..Default::default()
            }),
            warnings: Throttle::default(),
        }
    }

    /// Whether a profile upload for `online_id` from `ip` is kept (see "Profile uploads"): the
    /// address has a session under that id in a played room, or one that left such a room less
    /// than UPLOAD_GRACE ago.
    pub fn keeps_profile(&self, online_id: &str, ip: IpAddr) -> bool {
        let l = self.lobby();
        let is_uploader = |client: &ClientId| {
            l.clients
                .get(client)
                .is_some_and(|c| c.name == online_id && c.ip == ip)
        };
        let playing = l
            .rooms
            .values()
            .filter(|r| r.played)
            .any(|r| r.members.iter().any(|m| is_uploader(&m.client)));
        playing
            || l.upload_permits
                .get(&(online_id.to_string(), ip))
                .is_some_and(|until| *until > Instant::now())
    }

    /// Keeps `online_id`'s profile uploads from `ip` for UPLOAD_GRACE, as if it had just left a
    /// played room.
    #[cfg(test)]
    pub(crate) fn permit_uploads_for_test(&self, online_id: &str, ip: IpAddr) {
        self.lobby()
            .upload_permits
            .insert((online_id.to_string(), ip), Instant::now() + UPLOAD_GRACE);
    }

    /// Whether `client` already holds MAX_ROOMS_PER_SESSION rooms, checked after a create or join
    /// ended the membership of the room object it reuses. If so, the request is refused: room id 0
    /// in RoomJoined tells the client it failed.
    fn at_room_limit(&self, l: &Lobby, client: ClientId, who: &str, ptr: u32) -> bool {
        if l.rooms_of(client).len() < MAX_ROOMS_PER_SESSION {
            return false;
        }
        if let Some(suppressed) = self.warnings.allow() {
            warn!(%who, suppressed, "room refused: the session is already in {MAX_ROOMS_PER_SESSION} rooms");
        }
        l.send(&[client], &Rec::new(ROOM_JOINED, 0xa0).u32(8, ptr).done());
        true
    }

    /// The lobby state. A panic in a handler must not take matchmaking down for everyone, so a
    /// poisoned lock is taken over (every handler leaves the state consistent between statements
    /// that can fail).
    fn lobby(&self) -> std::sync::MutexGuard<'_, Lobby> {
        self.lobby.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers a connection. An older connection of the same player from the same address (a
    /// reconnect before the old one timed out) is dropped first: its memberships end and its
    /// connection is closed.
    fn connect(
        &self,
        name: String,
        ip: IpAddr,
        tx: mpsc::UnboundedSender<Vec<u8>>,
    ) -> (ClientId, Arc<Notify>) {
        let mut l = self.lobby();
        // Same player = same online id (the player id's later bytes may differ). The id is not
        // authenticated: only a connection from the same address may replace a session, so nobody
        // can end another player's session by claiming their id.
        let same: Vec<(ClientId, bool)> = l
            .clients
            .iter()
            .filter(|(_, c)| c.name == name)
            .map(|(id, c)| (*id, c.ip == ip))
            .collect();
        if same.iter().any(|(_, same_ip)| !same_ip)
            && let Some(suppressed) = self.warnings.allow()
        {
            warn!(online_id = %name, %ip, suppressed, "online id already connected from another address; keeping both sessions");
        }
        let stale = same
            .into_iter()
            .filter(|(_, same_ip)| *same_ip)
            .map(|(id, _)| id);
        for old in stale {
            info!(online_id = %name, "replacing the player's previous matchmaking connection");
            l.leave_all(old);
            if let Some(c) = l.clients.remove(&old) {
                c.evicted.notify_one();
            }
        }
        l.next_client += 1;
        let id = l.next_client;
        let evicted = Arc::new(Notify::new());
        l.clients.insert(
            id,
            Client {
                name,
                ip,
                tx,
                evicted: evicted.clone(),
            },
        );
        (id, evicted)
    }

    /// Ends a connection's session: every membership ends and the others are told.
    fn disconnect(&self, client: ClientId) {
        let mut l = self.lobby();
        l.leave_all(client);
        l.clients.remove(&client);
    }

    /// Handles one client record (type already known to be valid).
    fn handle(&self, client: ClientId, player_id: &[u8; PLAYER_ID_LEN], rec: &[u8]) {
        let kind = be32(rec, 0);
        let mut l = self.lobby();
        let Some(who) = l.clients.get(&client).map(|c| c.name.clone()) else {
            // Replaced by a newer connection; its records no longer count.
            return;
        };
        match kind {
            ROOM_CREATE => {
                let slot = be32(rec, 8);
                l.leave_slot(client, slot);
                if self.at_room_limit(&l, client, &who, slot) {
                    return;
                }
                let id = l.next_room;
                l.next_room += 1;
                let data_len = rec[0x26].min(DATA_LEN as u8);
                let mut room = Room {
                    id,
                    owner: 1,
                    max: be16(rec, 0x24).clamp(1, MAX_MEMBERS as u16),
                    mode: be32(rec, 0xc),
                    playlist: be32(rec, 0x14),
                    room_flags: be16(rec, 0x18),
                    attr_flags: 0,
                    name: rec[0x28..0xa8].try_into().unwrap(),
                    next_member: 2,
                    members: Vec::new(),
                    host_rank: Vec::new(),
                    played: false,
                };
                room.members.push(Member {
                    id: 1,
                    client,
                    slot,
                    player_id: *player_id,
                    flags: 0,
                    data_len,
                    data: rec[0xa8..0xe8].try_into().unwrap(),
                });
                info!(%who, room = id, max = room.max, mode = format!("{:#x}", room.mode),
                      playlist = format!("{:#x}", room.playlist), raw = %hex(&rec[4..0x28]), "room created");
                let reply = room.joined(slot, 1);
                l.rooms.insert(id, room);
                l.send(&[client], &reply);
            }
            ROOM_JOIN => {
                let room_id = be64(rec, 0x10);
                let ptr = be32(rec, 8);
                let data_len = rec[0xc].min(DATA_LEN as u8);
                l.leave_slot(client, ptr);
                if self.at_room_limit(&l, client, &who, ptr) {
                    return;
                }
                let joined = l.rooms.get_mut(&room_id).and_then(|room| {
                    if room.member_by_client(client).is_some()
                        || room.members.len() >= room.max as usize
                    {
                        return None;
                    }
                    let m = Member {
                        id: room.allocate_member_id(),
                        client,
                        slot: ptr,
                        player_id: *player_id,
                        flags: 0,
                        data_len,
                        data: rec[0x18..0x58].try_into().unwrap(),
                    };
                    let mut note = Rec::new(MEMBER_JOINED, 0x78);
                    note.u64(8, room_id)
                        .bytes(0x10, &m.player_id)
                        .u16(0x34, m.id)
                        .u8(0x36, m.flags)
                        .u8(0x37, m.data_len)
                        .bytes(0x38, &m.data);
                    let others = room.others(client);
                    let id = m.id;
                    room.members.push(m);
                    Some((room.joined(ptr, id), others, note.done(), id))
                });
                match joined {
                    Some((reply, others, note, member)) => {
                        info!(%who, room = room_id, member, "joined room");
                        l.send(&[client], &reply);
                        l.send(&others, &note);
                        // A guest joining a private custom game makes it a match.
                        l.check_played(room_id);
                    }
                    None => {
                        info!(%who, room = room_id, "join refused (no such room, full, or already in it)");
                        // Room id 0 tells the client the join failed.
                        l.send(&[client], &Rec::new(ROOM_JOINED, 0xa0).u32(8, ptr).done());
                    }
                }
            }
            ROOM_LEAVE => l.leave(be64(rec, 8), client, false),
            ROOM_SEARCH => {
                let (mode, playlist) = (be32(rec, 0xc), be32(rec, 0x10));
                // Region and skill fields (+0x14..+0x24) are ignored for now (see "Matching").
                let mut rooms: Vec<&Room> = l
                    .rooms
                    .values()
                    .filter(|r| r.matches_search(mode, playlist))
                    .filter(|r| r.member_by_client(client).is_none())
                    .collect();
                // Fuller rooms first, ties in random order (see "Matching").
                let ties = RandomState::new();
                rooms.sort_by_cached_key(|r| (Reverse(r.members.len()), ties.hash_one(r.id)));
                rooms.truncate(MAX_SEARCH_RESULTS);
                let mut r = Rec::new(ROOM_SEARCH_RESULT, 0x10 + rooms.len() * SEARCH_ENTRY_LEN);
                r.u32(8, be32(rec, 8)).u32(0xc, rooms.len() as u32);
                for (i, room) in rooms.iter().enumerate() {
                    r.bytes(0x10 + i * SEARCH_ENTRY_LEN, &room.search_entry());
                }
                info!(%who, mode = format!("{:#x}", mode), playlist = format!("{:#x}", playlist), found = rooms.len(),
                      raw = %hex(&rec[0x14..0x24]), "room search");
                let reply = r.done();
                l.send(&[client], &reply);
            }
            KICKOUT => {
                let (room_id, target) = (be64(rec, 8), be16(rec, 4));
                let victim = l
                    .rooms
                    .get(&room_id)
                    .filter(|r| r.member_by_client(client).is_some_and(|m| m.id == r.owner))
                    .and_then(|r| r.members.iter().find(|m| m.id == target).map(|m| m.client));
                match victim {
                    Some(v) if v != client => l.leave(room_id, v, true),
                    _ => {
                        if let Some(suppressed) = self.warnings.allow() {
                            warn!(%who, room = room_id, target, suppressed, "kick ignored (not the owner or no such member)")
                        }
                    }
                }
            }
            MEMBER_SET_DATA => {
                let room_id = be64(rec, 8);
                let len = rec[4].min(DATA_LEN as u8);
                let Some(room) = l.rooms.get_mut(&room_id) else {
                    return;
                };
                let Some(m) = room.members.iter_mut().find(|m| m.client == client) else {
                    return;
                };
                m.data_len = len;
                m.data.copy_from_slice(&rec[0x10..0x50]);
                let mut note = Rec::new(MEMBER_UPDATED_DATA, 0x50);
                note.u16(4, m.id)
                    .u8(6, len)
                    .u64(8, room_id)
                    .bytes(0x10, &rec[0x10..0x50]);
                let others = room.others(client);
                debug!(%who, room = room_id, len, "member data");
                l.send(&others, &note.done());
            }
            PROMOTE | SET_ATTR_FLAGS | SET_ROOM_FLAGS | SET_ROOM_NAME => {
                let room_id = be64(rec, 8);
                let Some(room) = l.rooms.get_mut(&room_id) else {
                    return;
                };
                let Some(me) = room.member_by_client(client).map(|m| m.id) else {
                    return;
                };
                let note = match kind {
                    PROMOTE => {
                        let target = be16(rec, 4);
                        if me != room.owner || !room.members.iter().any(|m| m.id == target) {
                            if let Some(suppressed) = self.warnings.allow() {
                                warn!(%who, room = room_id, target, suppressed, "promote ignored");
                            }
                            return;
                        }
                        room.owner = target;
                        info!(%who, room = room_id, owner = target, "owner changed");
                        Rec::new(OWNER_CHANGED, 0x10)
                            .u16(4, target)
                            .u64(8, room_id)
                            .done()
                    }
                    // Attr and room flags are accepted from any member (see "Trust" in the module docs).
                    SET_ATTR_FLAGS => {
                        room.attr_flags = rec[4];
                        debug!(%who, room = room_id, attr = rec[4], "attr flags");
                        Rec::new(UPDATED_ATTR_FLAGS, 0x10)
                            .u8(4, rec[4])
                            .u64(8, room_id)
                            .done()
                    }
                    SET_ROOM_FLAGS => {
                        room.room_flags = be16(rec, 4);
                        debug!(%who, room = room_id, flags = room.room_flags, "room flags");
                        Rec::new(UPDATED_ROOM_FLAGS, 0x10)
                            .u16(4, room.room_flags)
                            .u64(8, room_id)
                            .done()
                    }
                    _ => {
                        room.name.copy_from_slice(&rec[0x10..0x90]);
                        Rec::new(UPDATED_ROOM_NAME, 0x90)
                            .u64(8, room_id)
                            .bytes(0x10, &room.name)
                            .done()
                    }
                };
                let to = if kind == PROMOTE {
                    room.all()
                } else {
                    room.others(client)
                };
                l.send(&to, &note);
                if kind == SET_ROOM_FLAGS {
                    l.check_played(room_id);
                }
            }
            HOST_RANK => {
                let room_id = be64(rec, 8);
                let ranks: Vec<u16> = rec[0x10..]
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                let Some(room) = l.rooms.get_mut(&room_id) else {
                    return;
                };
                // Only the owner's ranking decides who takes over when the owner leaves.
                if room
                    .member_by_client(client)
                    .is_some_and(|m| m.id == room.owner)
                {
                    debug!(%who, room = room_id, ?ranks, "host ranking");
                    room.host_rank = ranks;
                }
            }
            PING => {}
            CLIENT_HELLO2 => debug!(%who, proof = %hex(&rec[4..8]), "client hello 2"),
            _ => {
                if let Some(suppressed) = self.warnings.allow() {
                    warn!(%who, kind = format!("{kind:#x}"), raw = %hex(rec), suppressed, "unexpected record")
                }
            }
        }
    }
}

pub async fn serve(bind: SocketAddr, mm: Arc<Matchmaking>) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding matchmaking port {bind}"))?;
    info!(%bind, "matchmaking listening");
    let listener = Listener::new(listener, "matchmaking");
    loop {
        let (stream, peer, slot) = listener.accept().await;
        let mm = mm.clone();
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(e) = connection(stream, peer, &mm).await {
                debug!(%peer, "matchmaking connection ended: {e:#}");
            }
        });
    }
}

/// Reads one whole record. The idle limit covers the record, not just its first bytes, so a
/// client that stops mid-record does not keep its session open.
async fn read_record(r: &mut (impl AsyncReadExt + Unpin), idle: Duration) -> Result<Vec<u8>> {
    let read = async {
        let mut head = [0u8; 4];
        r.read_exact(&mut head).await?;
        let kind = u32::from_be_bytes(head);
        let Some(mut len) = request_len(kind) else {
            bail!("unknown record type {kind:#x}")
        };
        let mut rec = head.to_vec();
        rec.resize(len, 0);
        r.read_exact(&mut rec[4..]).await?;
        if kind == HOST_RANK {
            len += 2 * be16(&rec, 4) as usize;
            rec.resize(len, 0);
            r.read_exact(&mut rec[0x10..]).await?;
        }
        Ok(rec)
    };
    tokio::time::timeout(idle, read)
        .await
        .context("idle timeout")?
}

/// Writes queued records in order. Returns when the queue closes (the session was removed), or
/// with an error when a write fails or stalls (the client stopped reading).
async fn write_queue(
    wr: &mut (impl AsyncWriteExt + Unpin),
    rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<()> {
    while let Some(rec) = rx.recv().await {
        tokio::time::timeout(WRITE_TIMEOUT, wr.write_all(&rec))
            .await
            .context("write stalled")??;
    }
    Ok(())
}

async fn connection(stream: TcpStream, peer: SocketAddr, mm: &Matchmaking) -> Result<()> {
    connection_with(stream, peer, mm, IDLE).await
}

/// One client session. It ends on the first of: read error or idle timeout, write error or
/// stall, or replacement by a newer connection of the same player. Cleanup (leaving every room,
/// telling the other members) then runs exactly once.
async fn connection_with(
    stream: TcpStream,
    peer: SocketAddr,
    mm: &Matchmaking,
    idle: Duration,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let (mut rd, mut wr) = stream.into_split();
    let hello = read_record(&mut rd, idle).await?;
    if be32(&hello, 0) != CLIENT_HELLO {
        bail!("expected ClientHello, got {}", hex(&hello));
    }
    let player_id: [u8; PLAYER_ID_LEN] = hello[8..8 + PLAYER_ID_LEN].try_into().unwrap();
    let who = online_id(&player_id);
    // The session is registered before the ServerHello goes out (it is the first queued record),
    // so the client never acts on a session the server does not know yet.
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let _ = tx.send(Rec::new(SERVER_HELLO, 0x10).u32(8, rand_u32()).done());
    let (client, evicted) = mm.connect(who.clone(), peer.ip(), tx);
    info!(%peer, online_id = %who, "matchmaking client connected");

    let reader = async {
        loop {
            let rec = read_record(&mut rd, idle).await?;
            mm.handle(client, &player_id, &rec);
        }
    };
    // The queue only closes when the server removed the session, i.e. on replacement.
    let replaced = || anyhow::anyhow!("replaced by a newer connection");
    let result: Result<()> = tokio::select! {
        biased;
        () = evicted.notified() => Err(replaced()),
        r = write_queue(&mut wr, &mut rx) => r.and_then(|()| Err(replaced())),
        r = reader => r,
    };
    mm.disconnect(client);
    info!(%peer, online_id = %who, "matchmaking client disconnected");
    result
}

fn rand_u32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    h.finish() as u32
}

#[cfg(test)]
mod tests;
