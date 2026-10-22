//! The location service: plain TCP (no hello, no frames). The client sends "get-location\n" and parses
//! "+ <lat> <lon> <code> <code> <code>" (tokens split on space and newline; the codes are up to 4
//! characters). The coordinates feed geographic matchmaking: one shared value keeps all players in
//! one region. The client takes 0/0 for "no location yet" and asks again every 5 s while it holds
//! that value, so the answer must not be 0/0.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info};

use crate::listen::Listener;

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub lat: f32,
    pub lon: f32,
}

impl Default for Location {
    fn default() -> Self {
        Self { lat: 1.0, lon: 1.0 }
    }
}

impl Location {
    pub fn check(self) -> Result<()> {
        if self.lat == 0.0 && self.lon == 0.0 {
            anyhow::bail!(
                "location = {{ lat = 0, lon = 0 }} means \"no location\" to the client, which then asks \
                           again every 5 s; use any other value"
            );
        }
        Ok(())
    }
}

pub fn answer(loc: Location) -> String {
    format!("+ {} {} 0 0 0\n", loc.lat, loc.lon)
}

pub async fn serve(bind: SocketAddr, loc: Location) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding location port {bind}"))?;
    info!(%bind, "location service listening");
    let listener = Listener::new(listener, "location");
    loop {
        let (mut s, peer, slot) = listener.accept().await;
        tokio::spawn(async move {
            let _slot = slot;
            let mut buf = [0u8; 0x100];
            let n = match tokio::time::timeout(std::time::Duration::from_secs(10), s.read(&mut buf))
                .await
            {
                Ok(Ok(n)) => n,
                _ => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]).trim_end().to_string();
            debug!(%peer, request = %req, "location request");
            let _ = s.write_all(answer(loc).as_bytes()).await;
        });
    }
}

#[cfg(test)]
mod tests;
