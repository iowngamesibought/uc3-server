//! Answers for the framed services, by service name and request text.
//!
//! The client copies each reply into a fixed-size buffer without checking the length, so every reply
//! is kept strictly below that caller's buffer (REPLY_LIMITS). Replies starting with '+' carry data; for the
//! ban check a '+' means "banned".

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

use crate::framed::hex;
use crate::ticket;

/// Largest reply each service's caller can take, in bytes (from the client's receive buffers).
const REPLY_LIMITS: [(&str, usize); 8] = [
    ("ticket-server", 16),
    ("report-server", 0x100),
    ("heartbeat-server", 0x100),
    ("invite-server", 0x800),
    ("leaderboard-server", 0x800),
    ("gamelist-server", 0x20),
    ("vulgarity-server", 0x40),
    ("facebook-server", 0x100),
];
/// For services not identified yet.
const DEFAULT_LIMIT: usize = 0x10;

#[derive(Debug, Clone, Default)]
pub struct Options {}

/// How many issued game ids are remembered for uploads; a record is uploaded right after its id.
const MAX_GAME_IDS: usize = 1024;

/// Game ids handed out by `new-id`. The HTTP side accepts a game record upload (`games/<id>`) only
/// for an id this server issued, so nobody can write arbitrary records.
#[derive(Default)]
pub struct GameIds(Mutex<VecDeque<String>>);

impl GameIds {
    pub(crate) fn issue(&self, id: &str) {
        let mut ids = self.0.lock().unwrap_or_else(|e| e.into_inner());
        ids.push_back(id.to_string());
        if ids.len() > MAX_GAME_IDS {
            ids.pop_front();
        }
    }

    pub fn issued(&self, id: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|i| i == id)
    }
}

pub struct Services {
    #[allow(dead_code)]
    options: Options,
    games: AtomicU64,
    game_ids: Arc<GameIds>,
}

fn text(request: &[u8]) -> String {
    let end = request
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(request.len());
    String::from_utf8_lossy(&request[..end])
        .trim_end_matches(['\n', '\r'])
        .to_string()
}

impl Services {
    pub fn new(options: Options) -> Self {
        Self {
            options,
            games: AtomicU64::new(0),
            game_ids: Arc::default(),
        }
    }

    /// The ids issued by `new-id`, shared with the HTTP side.
    pub fn game_ids(&self) -> Arc<GameIds> {
        self.game_ids.clone()
    }

    /// A game id for games/<id>: unique per run and across restarts (time-based prefix), at most 31
    /// characters as the client's buffer requires.
    fn next_game_id(&self) -> String {
        let n = self.games.fetch_add(1, Ordering::Relaxed);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = format!("{t:x}{n:04x}");
        self.game_ids.issue(&id);
        id
    }

    pub fn handle(&self, service: &str, request: &[u8], peer: SocketAddr) -> Vec<u8> {
        let mut reply = match service {
            "ticket-server" => self.ticket(request, peer),
            _ => {
                let t = text(request);
                let (cmd, rest) = t.split_once(' ').unwrap_or((t.as_str(), ""));
                self.command(service, cmd, rest, peer)
            }
        };
        let limit = REPLY_LIMITS
            .iter()
            .find(|(s, _)| *s == service)
            .map_or(DEFAULT_LIMIT, |l| l.1);
        if reply.len() >= limit {
            warn!(
                service,
                len = reply.len(),
                limit,
                "reply truncated to fit the client's buffer"
            );
            reply.truncate(limit - 1);
        }
        reply
    }

    /// The login ticket: `[u16 BE length][ticket]`. The client reads one reply and does not look at it.
    fn ticket(&self, request: &[u8], peer: SocketAddr) -> Vec<u8> {
        let ticket = request.get(2..).unwrap_or_default();
        match ticket::parse(ticket) {
            Some(t) => {
                let id = t.online_id.unwrap_or_default();
                info!(%peer, online_id = %id, issuer = ?t.issuer_id, "login ticket");
            }
            None => warn!(%peer, len = ticket.len(), "login ticket not parsed"),
        }
        b"+".to_vec()
    }

    fn command(&self, service: &str, cmd: &str, rest: &str, peer: SocketAddr) -> Vec<u8> {
        match (service, cmd) {
            // "+<n> <type>" would mean banned; anything else means not banned.
            ("report-server", "is-banned") => {
                info!(%peer, online_id = rest, "ban check: not banned");
                b"-\n".to_vec()
            }
            ("report-server", "report") => {
                info!(%peer, report = rest, "player report");
                b"+\n".to_vec()
            }
            ("heartbeat-server", "heartbeat") => {
                info!(%peer, heartbeat = rest, "heartbeat");
                b"+\n".to_vec()
            }
            // Lines "+<token> <base64 record>"; none yet.
            ("invite-server", "invite-list") => b"\n".to_vec(),
            ("invite-server", "invite-send" | "invite-delete") => {
                info!(%peer, cmd, args = rest, "invite (not stored yet)");
                b"+\n".to_vec()
            }
            // Lines "+<rank> <name> <score> <base64 64-byte record>", ended by an empty line.
            ("leaderboard-server", "leaderboard-get" | "leaderboard-range") => b"\n".to_vec(),
            ("leaderboard-server", "leaderboard-update") => {
                info!(%peer, args = rest, "leaderboard update (not stored yet)");
                b"+\n".to_vec()
            }
            // "+<id>": the client then sends "game-add2 <record>" and uploads the record to games/<id>.
            ("gamelist-server", "new-id") => {
                let id = self.next_game_id();
                info!(%peer, game = %id, "new game id");
                format!("+{id}\n").into_bytes()
            }
            ("gamelist-server", "game-add2") => {
                info!(%peer, bytes = rest.len(), "game record registered");
                b"+\n".to_vec()
            }
            ("vulgarity-server", "filter") => {
                info!(%peer, text = rest, "text filter (meaning of the '+' answer not known yet; answering '-')");
                b"-\n".to_vec()
            }
            _ => {
                warn!(%peer, service, cmd, args = rest, raw = %hex(cmd.as_bytes()), "unhandled service request");
                b"-\n".to_vec()
            }
        }
    }
}

#[cfg(test)]
mod tests;
