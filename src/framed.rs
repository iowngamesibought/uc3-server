//! The client's framed TCP services. Each client connection:
//!   1. client -> server, plaintext, 0x58 bytes: 0x11, 0, u16 0, u32 BE client nonce, 16 ignored
//!      bytes, then the service name (NUL-terminated, at most 0x40 bytes);
//!   2. server -> client, 8 bytes: '"', 3 bytes 0, u32 BE server nonce;
//!   3. frames (see framecipher): client frames use the server nonce as counter, server frames use
//!      the client nonce; both advance by one per frame. Usually one request and one reply, then the
//!      client closes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::framecipher::{self, FrameKey, HEADER};
use crate::listen::{Listener, Throttle};
use crate::services::Services;

pub const HELLO_LEN: usize = 0x58;
const IDLE: Duration = Duration::from_secs(30);
/// Largest frame payload accepted from a client.
const MAX_REQUEST: usize = 0x4000;

pub struct Hello {
    pub client_nonce: u32,
    pub service: String,
}

pub fn parse_hello(h: &[u8; HELLO_LEN]) -> Result<Hello> {
    if h[0] != 0x11 {
        bail!("not a service hello (first byte {:#04x})", h[0]);
    }
    let name = &h[0x18..];
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    Ok(Hello {
        client_nonce: u32::from_be_bytes(h[4..8].try_into().unwrap()),
        service: String::from_utf8_lossy(&name[..end]).into_owned(),
    })
}

pub async fn serve(bind: SocketAddr, key: Arc<FrameKey>, services: Arc<Services>) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding service port {bind}"))?;
    info!(%bind, "framed services listening");
    let listener = Listener::new(listener, "framed");
    // The hello is plaintext and checked before any key is used, so anyone can trigger the
    // warnings about it: they are throttled per port.
    let warnings = Arc::new(Throttle::default());
    loop {
        let (stream, peer, slot) = listener.accept().await;
        let (key, services, warnings) = (key.clone(), services.clone(), warnings.clone());
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(e) = connection(stream, peer, bind.port(), &key, &services, &warnings).await
            {
                debug!(%peer, port = bind.port(), "connection ended: {e:#}");
            }
        });
    }
}

async fn connection(
    mut s: TcpStream,
    peer: SocketAddr,
    port: u16,
    key: &FrameKey,
    services: &Services,
    warnings: &Throttle,
) -> Result<()> {
    s.set_nodelay(true)?;
    let mut hello = [0u8; HELLO_LEN];
    let mut got = 0;
    while got < HELLO_LEN {
        match tokio::time::timeout(IDLE, s.read(&mut hello[got..])).await {
            Ok(Ok(n)) if n > 0 => got += n,
            outcome => {
                let why = match outcome {
                    Err(_) => "timeout".to_string(),
                    Ok(Err(e)) => e.to_string(),
                    Ok(Ok(_)) => "closed by peer".to_string(),
                };
                if let Some(suppressed) = warnings.allow() {
                    warn!(%peer, port, received = got, data = %hex(&hello[..got]), suppressed,
                          "connection ended before a full service hello ({why})");
                }
                return Ok(());
            }
        }
    }
    let hello = match parse_hello(&hello) {
        Ok(h) => h,
        Err(e) => {
            if let Some(suppressed) = warnings.allow() {
                warn!(%peer, port, first_bytes = %hex(&hello[..16]), suppressed, "unexpected data on a service port: {e}");
            }
            return Ok(());
        }
    };
    let mut recv_ctr: u32 = rand_u32() & 0x3fff_ffff;
    let mut send_ctr = hello.client_nonce;
    let mut reply = [0u8; 8];
    reply[0] = b'"';
    reply[4..].copy_from_slice(&recv_ctr.to_be_bytes());
    s.write_all(&reply).await?;
    debug!(%peer, port, service = %hello.service, "service connection");
    loop {
        let mut header = [0u8; HEADER];
        match tokio::time::timeout(IDLE, s.read_exact(&mut header)).await {
            Err(_) => bail!("idle timeout"),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Ok(r) => {
                r?;
            }
        }
        let total = framecipher::frame_len(&header).context("bad frame header")?;
        if total - HEADER > MAX_REQUEST {
            bail!("frame too large ({total} bytes)");
        }
        let mut frame = header.to_vec();
        frame.resize(total, 0);
        tokio::time::timeout(IDLE, s.read_exact(&mut frame[HEADER..]))
            .await
            .context("frame timeout")??;
        let request = framecipher::open(key, recv_ctr, &frame)?;
        recv_ctr = recv_ctr.wrapping_add(1);
        let answer = services.handle(&hello.service, &request, peer);
        s.write_all(&framecipher::seal(key, send_ctr, &answer))
            .await?;
        send_ctr = send_ctr.wrapping_add(1);
    }
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

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests;
