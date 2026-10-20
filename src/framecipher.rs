//! The cipher of the client's framed TCP connections: a 128-bit-state stream cipher with a 16-byte
//! MAC, keyed by a 16-byte key from the user's executable plus a per-frame counter. Data is processed
//! as little-endian 32-bit words.
//!
//! Frame: '3' | pad | u16 BE payload length | MAC[16] | encrypted (payload + pad bytes), where
//! pad = -len & 3. The MAC is taken over the plaintext; sender and receiver use the same counter,
//! which advances by one per frame.

use anyhow::{Result, bail};

use crate::elf::Elf;

const C1: u32 = 0x5B3A_A654;
const C2: u32 = 0x7597_0A4D;
/// Address of the frame key in each supported build: (label, pointer slot, key address).
const BUILDS: [(&str, u64, u64); 1] = [("1.19", 0x0116_4AC4, 0x00DF_0470)];
pub const HEADER: usize = 20;

#[derive(Clone)]
pub struct FrameKey([u32; 4]);

impl FrameKey {
    pub fn new(key: &[u8; 16]) -> Self {
        Self(std::array::from_fn(|i| {
            u32::from_le_bytes(key[4 * i..4 * i + 4].try_into().unwrap())
        }))
    }

    /// Reads the key from the user's decrypted executable; nothing is written to disk.
    pub fn from_executable(elf: &Elf) -> Result<Self> {
        for &(label, slot, addr) in &BUILDS {
            let Ok(p) = elf.read(slot, 4) else { continue };
            if u32::from_be_bytes(p.try_into().unwrap()) as u64 == addr {
                let key: [u8; 16] = elf.read(addr, 16)?.try_into().unwrap();
                tracing::info!(build = label, "frame key found");
                return Ok(Self::new(&key));
            }
        }
        bail!(
            "the frame key location of no supported build (1.19) matches this executable; see AGENTS.md"
        )
    }
}

struct Ctx([u32; 4]);

impl Ctx {
    fn new(key: &FrameKey, counter: u32) -> Self {
        let mut c = Ctx(key.0);
        c.absorb_word(counter);
        let warm = c.0;
        for w in warm {
            c.encrypt_word(w);
        }
        c
    }

    /// One step: returns the keystream word and updates a, b, c (d is set by the caller).
    fn step(&mut self) -> u32 {
        let [a, b, c, d] = self.0;
        let d = d ^ C1;
        let c = c.wrapping_add(a).wrapping_add(d).rotate_left(7);
        let b = b.wrapping_add(d).wrapping_add(c);
        let c = c ^ C2;
        let t5 = b.rotate_left(11);
        let a = a.wrapping_add(c).wrapping_add(t5);
        let t3 = a.rotate_left(17);
        let k = d.wrapping_add((t3 & t5) | (c & !t5));
        self.0 = [!t3, !t5, c, d];
        k
    }

    fn absorb_word(&mut self, w: u32) {
        let k = self.step();
        self.0[3] = k ^ w;
    }

    fn encrypt_word(&mut self, w: u32) -> u32 {
        let k = self.step();
        self.0[3] = k ^ w;
        k ^ w
    }

    fn decrypt_word(&mut self, w: u32) -> u32 {
        let k = self.step();
        self.0[3] = w;
        k ^ w
    }

    fn mac(&self) -> [u8; 16] {
        let x = self.0.iter().fold(0, |a, v| a ^ v);
        let mut out = [0u8; 16];
        for (i, v) in self.0.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&(x ^ v).to_le_bytes());
        }
        out
    }
}

fn words(data: &[u8]) -> impl Iterator<Item = u32> + '_ {
    data.chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
}

/// Builds one frame for `payload` with the sender's counter.
pub fn seal(key: &FrameKey, counter: u32, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= u16::MAX as usize, "frame payload too long");
    let pad = payload.len().wrapping_neg() & 3;
    let mut body = payload.to_vec();
    body.resize(payload.len() + pad, 0);
    let mut m = Ctx::new(key, counter);
    for w in words(&body) {
        m.absorb_word(w);
    }
    let mut e = Ctx::new(key, counter);
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&[0x33, pad as u8]);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(&m.mac());
    for w in words(&body) {
        out.extend_from_slice(&e.encrypt_word(w).to_le_bytes());
    }
    out
}

/// Total frame length announced by a header, or None if the header is not a frame header.
pub fn frame_len(header: &[u8; HEADER]) -> Option<usize> {
    (header[0] == 0x33)
        .then(|| HEADER + u16::from_be_bytes([header[2], header[3]]) as usize + header[1] as usize)
}

/// Decrypts and authenticates one complete frame; returns the payload.
pub fn open(key: &FrameKey, counter: u32, frame: &[u8]) -> Result<Vec<u8>> {
    let header: &[u8; HEADER] = frame
        .get(..HEADER)
        .and_then(|h| h.try_into().ok())
        .ok_or_else(|| anyhow::anyhow!("short frame"))?;
    let Some(total) = frame_len(header) else {
        bail!("not a frame")
    };
    if frame.len() != total || (total - HEADER) % 4 != 0 {
        bail!("bad frame length");
    }
    let len = u16::from_be_bytes([frame[2], frame[3]]) as usize;
    let mut d = Ctx::new(key, counter);
    let mut plain = Vec::with_capacity(total - HEADER);
    for w in words(&frame[HEADER..]) {
        plain.extend_from_slice(&d.decrypt_word(w).to_le_bytes());
    }
    let mut m = Ctx::new(key, counter);
    for w in words(&plain) {
        m.absorb_word(w);
    }
    if m.mac() != frame[4..HEADER] {
        bail!("frame MAC mismatch");
    }
    plain.truncate(len);
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn test_key() -> FrameKey {
        FrameKey::new(&std::array::from_fn(|i| i as u8))
    }

    /// Vectors from the reference implementation that was checked against the client's own send and
    /// receive code (dummy key 00..0f).
    #[test]
    fn matches_reference_vectors() {
        let k = test_key();
        assert_eq!(
            seal(&k, 0x1234_5678, b"is-banned TESTER"),
            hex("330000107745d7d9d7b3346793adbf5a64355f27cb7f579ab871a7e4d5ebe6af335b5ce4")
        );
        assert_eq!(
            seal(&k, 0x0bad_f00d, b"+ok"),
            hex("330100033310b6088c04ef315820e50a5b2fb26de734bc33")
        );
    }

    #[test]
    fn round_trip_and_tamper() {
        let k = test_key();
        for len in 0..40 {
            let msg: Vec<u8> = (0..len as u8).collect();
            let f = seal(&k, 7, &msg);
            assert_eq!(frame_len(f[..HEADER].try_into().unwrap()), Some(f.len()));
            assert_eq!(open(&k, 7, &f).unwrap(), msg);
            assert!(open(&k, 8, &f).is_err());
        }
        let mut f = seal(&k, 1, b"hello world");
        f[HEADER] ^= 1;
        assert!(open(&k, 1, &f).is_err());
    }
}
