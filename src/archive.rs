//! The game's archive format (version 1.3, zlib, 64 KiB blocks, entry 0 = manifest): reading the
//! user's installed archives and packing the served config.

use std::io::Write;

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};

/// One entry of a game archive (zlib, any block size), by manifest path.
pub fn read_archive_entry(blob: &[u8], name: &str) -> Result<Vec<u8>> {
    let index = archive_manifest(blob)?
        .iter()
        .position(|p| p == name)
        .with_context(|| format!("archive has no entry {name}"))?;
    archive_extract(blob, index + 1)
}

/// The entry paths of a game archive, in TOC order (entry 0, the manifest itself, excluded).
pub fn archive_manifest(blob: &[u8]) -> Result<Vec<String>> {
    let manifest = archive_extract(blob, 0)?;
    Ok(String::from_utf8_lossy(&manifest)
        .split('\n')
        .map(str::to_owned)
        .collect())
}

fn archive_extract(blob: &[u8], entry: usize) -> Result<Vec<u8>> {
    if blob.get(..4) != Some(b"PSAR") {
        bail!("not a game archive");
    }
    let (toc_len, entry_size, count, block) = (
        be32(blob, 12)?,
        be32(blob, 16)?,
        be32(blob, 20)?,
        be32(blob, 24)?,
    );
    let width = if block <= 0x10000 {
        2
    } else if block <= 0x100_0000 {
        3
    } else {
        4
    };
    let table = 32 + count * entry_size;
    let sizes: Vec<usize> = blob
        .get(table..toc_len)
        .context("archive block table truncated")?
        .chunks_exact(width)
        .map(|c| c.iter().fold(0usize, |a, &x| a << 8 | x as usize))
        .collect();
    let extract = |i: usize| -> Result<Vec<u8>> {
        let e = 32 + i * entry_size;
        let toc = blob.get(e..e + 30).context("archive TOC truncated")?;
        let mut blk = u32::from_be_bytes(toc[16..20].try_into().unwrap()) as usize;
        let size = toc[20..25].iter().fold(0usize, |a, &x| a << 8 | x as usize);
        let mut pos = toc[25..30].iter().fold(0usize, |a, &x| a << 8 | x as usize);
        // The size comes from the file: reserve at most 16 MiB up front, grow if it is real.
        let mut out = Vec::with_capacity(size.min(1 << 24));
        while out.len() < size {
            let stored = match sizes.get(blk).context("archive block index out of range")? {
                0 => block,
                &s => s,
            };
            let chunk = blob
                .get(pos..pos + stored)
                .context("archive data truncated")?;
            let want = block.min(size - out.len());
            if stored != want && chunk.first() == Some(&0x78) {
                std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(chunk), &mut out)?;
            } else {
                out.extend_from_slice(chunk);
            }
            pos += stored;
            blk += 1;
        }
        out.truncate(size);
        Ok(out)
    };
    if entry >= count {
        bail!("archive has no entry #{entry}");
    }
    extract(entry)
}

/// Game archive (format version 1.3), zlib, 64 KiB blocks, flags 0, entry 0 = manifest.
pub fn build_archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    const BLOCK: usize = 0x10000;
    let manifest = files.iter().map(|(p, _)| *p).collect::<Vec<_>>().join("\n");
    let mut entries: Vec<([u8; 16], &[u8])> = vec![([0; 16], manifest.as_bytes())];
    for (p, d) in files {
        entries.push((Md5::digest(p.as_bytes()).into(), d));
    }
    let (mut blocks, mut data, mut toc) = (Vec::<u16>::new(), Vec::<u8>::new(), Vec::new());
    for (md5, payload) in &entries {
        let first = blocks.len() as u32;
        let offset = data.len();
        for chunk in payload.chunks(BLOCK) {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
            z.write_all(chunk).expect("in-memory write");
            let packed = z.finish().expect("in-memory write");
            let stored: &[u8] = if packed.len() < chunk.len() {
                &packed
            } else {
                chunk
            };
            blocks.push(if stored.len() == BLOCK {
                0
            } else {
                stored.len() as u16
            });
            data.extend_from_slice(stored);
        }
        toc.push((*md5, first, payload.len() as u64, offset as u64));
    }
    let toc_len = 32 + 30 * toc.len() + 2 * blocks.len();
    let mut out = Vec::with_capacity(toc_len + data.len());
    out.extend_from_slice(b"PSAR");
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&3u16.to_be_bytes());
    out.extend_from_slice(b"zlib");
    for v in [toc_len as u32, 30, toc.len() as u32, BLOCK as u32, 0] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    for (md5, first, size, offset) in toc {
        out.extend_from_slice(&md5);
        out.extend_from_slice(&first.to_be_bytes());
        out.extend_from_slice(&size.to_be_bytes()[3..]);
        out.extend_from_slice(&(toc_len as u64 + offset).to_be_bytes()[3..]);
    }
    for s in blocks {
        out.extend_from_slice(&s.to_be_bytes());
    }
    out.extend_from_slice(&data);
    out
}

fn be32(b: &[u8], o: usize) -> Result<usize> {
    match b.get(o..o + 4) {
        Some(w) => Ok(u32::from_be_bytes(w.try_into().unwrap()) as usize),
        None => bail!("DC module truncated at {o:#x}"),
    }
}

/// The first entry whose manifest path ends with `suffix` (installed archives use full paths such as
/// `build/main/bin/dc1/<name>`).
pub fn read_entry_by_suffix(blob: &[u8], suffix: &str) -> Result<(String, Vec<u8>)> {
    let names = archive_manifest(blob)?;
    let index = names
        .iter()
        .position(|p| p == suffix || p.ends_with(&format!("/{suffix}")))
        .with_context(|| format!("archive has no entry ending in {suffix}"))?;
    Ok((names[index].clone(), archive_extract(blob, index + 1)?))
}

#[cfg(test)]
mod tests;
