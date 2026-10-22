//! What every TCP service shares: an accept loop that survives transient errors, a cap on the
//! connections one address may hold open, and a throttle for warnings that anyone who can reach a
//! port can trigger.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

/// Connections one address may hold open on one port. A console uses a few at once; the cap keeps
/// a single source from using up the server's sockets.
pub const MAX_CONNECTIONS_PER_IP: usize = 32;

/// Lines a throttled warning logs per minute; see [`Throttle`].
pub const WARNINGS_PER_MINUTE: u32 = 10;

/// A listening port and the connections each address holds open on it.
pub struct Listener {
    inner: TcpListener,
    open: Connections,
    service: &'static str,
    port: u16,
    accept_errors: Throttle,
}

impl Listener {
    pub fn new(inner: TcpListener, service: &'static str) -> Self {
        let port = inner.local_addr().map_or(0, |a| a.port());
        Self {
            inner,
            open: Connections::default(),
            service,
            port,
            accept_errors: Throttle::default(),
        }
    }

    /// The next connection that fits under the per-address cap, with its place in the count (keep
    /// it until the connection ends). Accept errors are usually transient (a client that reset
    /// before being accepted, or no free file descriptors under load): they are logged and the
    /// service keeps running. A connection over the cap is closed at once.
    pub async fn accept(&self) -> (TcpStream, SocketAddr, Slot) {
        loop {
            match self.inner.accept().await {
                Ok((stream, peer)) => match self.open.open(peer.ip()) {
                    Some(slot) => return (stream, peer, slot),
                    None => debug!(%peer, service = self.service, port = self.port,
                                   "too many open connections from this address; closed"),
                },
                Err(e) => {
                    if let Some(suppressed) = self.accept_errors.allow() {
                        warn!(service = self.service, port = self.port, error = %e, suppressed, "accept failed");
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

/// Open connections by client address.
#[derive(Clone, Default)]
struct Connections(Arc<Mutex<HashMap<IpAddr, usize>>>);

/// One open connection's place in the count; given back when dropped.
pub struct Slot {
    conns: Connections,
    ip: IpAddr,
}

impl Connections {
    fn open(&self, ip: IpAddr) -> Option<Slot> {
        let mut open = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let n = open.entry(ip).or_default();
        if *n >= MAX_CONNECTIONS_PER_IP {
            return None;
        }
        *n += 1;
        Some(Slot {
            conns: self.clone(),
            ip,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut open = self.conns.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = open.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.ip);
            }
        }
    }
}

/// Caps a repeated warning at a number of lines a minute, so a flood of junk can neither fill the
/// log nor slow a service down writing it. Lines over the cap are counted, and the count is
/// reported with the next line shown.
pub struct Throttle(Mutex<Window>);

struct Window {
    per_minute: u32,
    start: Instant,
    shown: u32,
    suppressed: u64,
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new(WARNINGS_PER_MINUTE)
    }
}

impl Throttle {
    pub fn new(per_minute: u32) -> Self {
        Self(Mutex::new(Window {
            per_minute,
            start: Instant::now(),
            shown: 0,
            suppressed: 0,
        }))
    }

    /// Some(lines suppressed since the last one shown) when this line may be logged.
    pub fn allow(&self) -> Option<u64> {
        self.allow_at(Instant::now())
    }

    fn allow_at(&self, now: Instant) -> Option<u64> {
        let mut w = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if now.duration_since(w.start) >= Duration::from_secs(60) {
            w.start = now;
            w.shown = 0;
        }
        if w.shown < w.per_minute {
            w.shown += 1;
            Some(std::mem::take(&mut w.suppressed))
        } else {
            w.suppressed += 1;
            None
        }
    }
}

#[cfg(test)]
mod tests;
