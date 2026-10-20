//! Minimal big-endian ELF64 reader: maps virtual addresses to file bytes so the cipher keys can be
//! read from the user's executable at runtime instead of being stored anywhere.

use anyhow::{Context, Result, bail};

pub struct Elf {
    data: Vec<u8>,
    loads: Vec<(u64, u64, u64)>, // (vaddr, file offset, file size)
}

impl Elf {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        if data.len() < 0x40 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 2 {
            bail!(
                "{} is not a big-endian ELF64: expected the decrypted main executable (an encrypted one is not an ELF); see AGENTS.md",
                path.display()
            );
        }
        let truncated = || anyhow::anyhow!("{} is a truncated or damaged ELF file", path.display());
        let be64 = |b: &[u8], o: usize| u64::from_be_bytes(b[o..o + 8].try_into().unwrap());
        let (phoff, phnum) = (
            be64(&data, 0x20) as usize,
            u16::from_be_bytes([data[0x38], data[0x39]]) as usize,
        );
        let mut loads = Vec::new();
        for i in 0..phnum {
            let start = phoff.saturating_add(i * 0x38);
            let ph = data
                .get(start..start.saturating_add(0x38))
                .ok_or_else(truncated)?;
            let p_type = u32::from_be_bytes(ph[..4].try_into().unwrap());
            let (off, vaddr, filesz) = (be64(ph, 8), be64(ph, 0x10), be64(ph, 0x20));
            if p_type == 1 && filesz > 0 {
                // Segments must lie inside the file (so every later read is in bounds) and must not
                // wrap the address space.
                if off
                    .checked_add(filesz)
                    .is_none_or(|end| end > data.len() as u64)
                    || vaddr.checked_add(filesz).is_none()
                {
                    return Err(truncated());
                }
                loads.push((vaddr, off, filesz));
            }
        }
        Ok(Self { data, loads })
    }

    pub fn read(&self, addr: u64, len: usize) -> Result<&[u8]> {
        for &(vaddr, off, size) in &self.loads {
            if addr >= vaddr && addr.saturating_add(len as u64) <= vaddr + size {
                let start = (off + addr - vaddr) as usize;
                return Ok(&self.data[start..start + len]);
            }
        }
        bail!("address {addr:#x}+{len:#x} is not file-backed")
    }

    pub fn cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while out.len() < max {
            let b = self.read(addr + out.len() as u64, 1)?[0];
            if b == 0 {
                return Ok(out);
            }
            out.push(b);
        }
        bail!("unterminated string at {addr:#x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Program headers past the end of the file, or a segment larger than the file, are errors.
    #[test]
    fn damaged_files_are_errors_not_panics() {
        let dir = std::env::temp_dir().join(format!("uc3-elf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("bad.elf");
        let mut h = vec![0u8; 0x40];
        h[..6].copy_from_slice(b"\x7fELF\x02\x02");
        h[0x20..0x28].copy_from_slice(&0x1000u64.to_be_bytes());
        h[0x38..0x3a].copy_from_slice(&1u16.to_be_bytes());
        std::fs::write(&f, &h).unwrap();
        let headers_missing = Elf::open(&f);
        h[0x20..0x28].copy_from_slice(&0x40u64.to_be_bytes());
        let mut ph = vec![0u8; 0x38];
        ph[..4].copy_from_slice(&1u32.to_be_bytes());
        ph[0x20..0x28].copy_from_slice(&0x10_0000u64.to_be_bytes());
        h.extend_from_slice(&ph);
        std::fs::write(&f, &h).unwrap();
        let segment_too_big = Elf::open(&f);
        std::fs::remove_dir_all(&dir).ok();
        assert!(headers_missing.is_err() && segment_too_big.is_err());
    }
}
