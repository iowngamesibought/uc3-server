//! The client's message and file cipher: Blowfish-ECB (standard tables, big-endian halves) with an
//! HMAC-SHA1 tag. Both keys are read at runtime from the user's own executable; none are stored.

use anyhow::{Result, bail};
use blowfish::Blowfish;
use blowfish::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::elf::Elf;

/// Where the two key strings sit in each supported client build: (label, MAC key address, Blowfish
/// key address).
const BUILDS: [(&str, u64, u64); 1] = [("1.19", 0x00DE_E388, 0x00DE_E3D0)];

pub struct BlobCipher {
    mac_key: Vec<u8>,
    bf: Blowfish,
}

impl BlobCipher {
    /// Reads both keys from the user's decrypted executable; nothing is written to disk.
    pub fn from_executable(path: &std::path::Path) -> Result<Self> {
        let elf = Elf::open(path)?;
        let printable =
            |k: &[u8], len: usize| k.len() == len && k.iter().all(|b| b.is_ascii_graphic());
        let found: Vec<_> = BUILDS
            .iter()
            .filter_map(|&(label, mac, bf)| {
                let mac_key = elf.cstr(mac, 128).ok()?;
                let bf_key = elf.cstr(bf, 128).ok()?;
                (printable(&mac_key, 64) && printable(&bf_key, 20))
                    .then_some((label, mac_key, bf_key))
            })
            .collect();
        let [(label, mac_key, bf_key)] = found.as_slice() else {
            bail!(
                "{} known key locations match this executable; expected exactly one. It is not the main executable of a supported build (1.19), or it is a different file; see AGENTS.md",
                found.len()
            );
        };
        tracing::info!(build = label, "cipher keys found");
        Self::new(mac_key, bf_key)
    }

    pub fn new(mac_key: &[u8], bf_key: &[u8]) -> Result<Self> {
        let bf = Blowfish::new_from_slice(bf_key)
            .map_err(|_| anyhow::anyhow!("invalid Blowfish key length"))?;
        Ok(Self {
            mac_key: mac_key.to_vec(),
            bf,
        })
    }

    fn mac(&self, data: &[u8]) -> [u8; 20] {
        let mut m = <Hmac<Sha1> as Mac>::new_from_slice(&self.mac_key)
            .expect("HMAC accepts any key length");
        m.update(data);
        m.finalize().into_bytes().into()
    }

    fn ecb(&self, data: &mut [u8], encrypt: bool) {
        for block in data.chunks_exact_mut(8) {
            let b = GenericArray::from_mut_slice(block);
            if encrypt {
                self.bf.encrypt_block(b)
            } else {
                self.bf.decrypt_block(b)
            }
        }
    }

    /// Blob format (presence messages over UDP; not served yet): Blowfish-ECB(plain || zero pad ||
    /// HMAC(plain || pad)), total length (len + 0x1b) & !7.
    #[allow(dead_code)]
    pub fn encrypt_blob(&self, plain: &[u8]) -> Vec<u8> {
        let total = (plain.len() + 0x1b) & !7;
        let mut out = plain.to_vec();
        out.resize(total - 20, 0);
        let tag = self.mac(&out);
        out.extend_from_slice(&tag);
        self.ecb(&mut out, true);
        out
    }

    #[allow(dead_code)]
    /// Returns the padded plaintext (len - 20 bytes), or None if the length or tag is wrong.
    pub fn decrypt_blob(&self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 24 || data.len() % 8 != 0 {
            return None;
        }
        let mut plain = data.to_vec();
        self.ecb(&mut plain, false);
        let tag: [u8; 20] = plain[plain.len() - 20..].try_into().ok()?;
        plain.truncate(plain.len() - 20);
        (self.mac(&plain) == tag).then_some(plain)
    }

    /// Stream format (downloaded files): u32be length || HMAC(padded)[20] || Blowfish-ECB(padded).
    pub fn encrypt_stream(&self, plain: &[u8]) -> Vec<u8> {
        let mut padded = plain.to_vec();
        padded.resize(plain.len().div_ceil(8) * 8, 0);
        let tag = self.mac(&padded);
        self.ecb(&mut padded, true);
        let mut out = Vec::with_capacity(24 + padded.len());
        out.extend_from_slice(&(plain.len() as u32).to_be_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&padded);
        out
    }

    #[cfg_attr(not(test), allow(dead_code))] // used by tests; kept for future tooling
    pub fn decrypt_stream(&self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 24 || (data.len() - 24) % 8 != 0 {
            return None;
        }
        let len = u32::from_be_bytes(data[..4].try_into().ok()?) as usize;
        let mut body = data[24..].to_vec();
        self.ecb(&mut body, false);
        if self.mac(&body) != data[4..24] || len > body.len() {
            return None;
        }
        body.truncate(len);
        Some(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cipher() -> BlobCipher {
        BlobCipher::new(&[0x41; 64], b"0123456789abcdefghij").unwrap()
    }

    #[test]
    fn blowfish_standard_vector() {
        // Schneier's vector: zero key, zero block -> 4EF997456198DD78.
        let bf: Blowfish = Blowfish::new_from_slice(&[0u8; 8]).unwrap();
        let mut block = [0u8; 8];
        bf.encrypt_block(GenericArray::from_mut_slice(&mut block));
        assert_eq!(block, [0x4E, 0xF9, 0x97, 0x45, 0x61, 0x98, 0xDD, 0x78]);
    }

    #[test]
    fn blob_round_trip_and_lengths() {
        let c = test_cipher();
        for len in 0..64 {
            let msg: Vec<u8> = (0..len as u8).collect();
            let blob = c.encrypt_blob(&msg);
            assert_eq!(blob.len(), (len + 0x1b) & !7);
            let plain = c.decrypt_blob(&blob).unwrap();
            assert_eq!(&plain[..len], &msg[..]);
        }
        let mut bad = c.encrypt_blob(b"hello");
        bad[0] ^= 1;
        assert!(c.decrypt_blob(&bad).is_none());
    }

    #[test]
    fn stream_round_trip() {
        let c = test_cipher();
        let msg = b"some archive bytes ".repeat(100);
        assert_eq!(c.decrypt_stream(&c.encrypt_stream(&msg)).unwrap(), msg);
    }
}
