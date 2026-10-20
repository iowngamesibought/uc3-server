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
//!
//! Matching (as observed on real consoles):
//!   * A room matches a search when its mode (RoomCreate +0x0c / RoomSearch +0x0c) and playlist
//!     (RoomCreate +0x14 / RoomSearch +0x10) both equal the search's, it has a free slot, and its
//!     room flag bit 0 is clear. Mode and playlist are opaque match keys: the server never
//!     interprets them. Seen so far: Find Game competitive, Find Game co-op, custom games (the mode
//!     changes with the chosen game type) and the invite party each use their own pair, so they
//!     never meet in a search.
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
//! Trust: every member may set the room-wide attr and room flags (the protocol's update records
//! carry no member id, and guests do set them: co-op round starts, a new owner closing the room).
//! The server keeps the last value and relays it. A modified client could therefore close, reopen
//! or relabel a room it is in; restricting these to the owner, or validating them against the
//! lobby phase, is where tracking can be hardened later. Kicks are accepted from the owner only but
//! were never seen from a console.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, info, warn};

use crate::framed::hex;

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
/// Room flag bit 0: the room is not open to searches (see "Matching" in the module docs).
const ROOM_FLAG_CLOSED: u16 = 1;
/// The client pings every 30 s. Three missed pings drop the connection, so a vanished console's
/// rooms leave the search results quickly instead of sending joiners to a dead host.
const IDLE: Duration = Duration::from_secs(90);
/// A record not written within this time means the client stopped reading: the session ends.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

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
        info!(
            room = room_id,
            member = gone.id,
            kicked,
            remaining = others.len(),
            "member left"
        );
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
}

pub struct Matchmaking {
    lobby: Mutex<Lobby>,
}

impl Matchmaking {
    pub fn new() -> Self {
        Self {
            lobby: Mutex::new(Lobby {
                next_room: 1,
                ..Default::default()
            }),
        }
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
        if same.iter().any(|(_, same_ip)| !same_ip) {
            warn!(online_id = %name, %ip, "online id already connected from another address; keeping both sessions");
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
                rooms.sort_by_key(|r| r.id);
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
                        warn!(%who, room = room_id, target, "kick ignored (not the owner or no such member)")
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
                            warn!(%who, room = room_id, target, "promote ignored");
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
            _ => warn!(%who, kind = format!("{kind:#x}"), raw = %hex(rec), "unexpected record"),
        }
    }
}

pub async fn serve(bind: SocketAddr, mm: Arc<Matchmaking>) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding matchmaking port {bind}"))?;
    info!(%bind, "matchmaking listening");
    loop {
        let (stream, peer) = listener.accept().await?;
        let mm = mm.clone();
        tokio::spawn(async move {
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
mod tests {
    use super::*;

    fn player_id(name: &str) -> [u8; PLAYER_ID_LEN] {
        let mut n = [0u8; PLAYER_ID_LEN];
        n[..name.len()].copy_from_slice(name.as_bytes());
        n
    }

    const LAN: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    fn client(mm: &Matchmaking, name: &str) -> (ClientId, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (mm.connect(name.into(), LAN, tx).0, rx)
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
}
