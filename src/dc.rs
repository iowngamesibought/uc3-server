//! The client's online config module (a compiled script module, "DC00") and the changes the server
//! makes to it: every server host is pointed at this server, the EULA and level downloads are
//! served from here, and optionally playlists get a lower minimum player count.
//!
//! Layout: "DC00" | u32 1 | u32 data end | u32 0 | u32 1 | u32 root count | root entries
//! { u32 sid, u32 type, u32 pointer } sorted by sid, from 0x18; data up to "data end"; then
//! u32 bitmap length and a relocation bitmap: one bit per 32-bit word of the data, most significant
//! bit first, set where the word is a module-relative pointer. The client's loader adds the module's
//! base address to every marked word.
//!
//! The two server-config objects (one per ticket issuer range) are 0x80 bytes. Their server-list
//! fields point to { u32 count, pointer elements } with 8-byte elements { pointer host, u32 port }.

use anyhow::{Context, Result, bail};

pub const SID_SERVER_CONFIG_A: u32 = 0x9BD0_38AC; // ticket issuer ids <= 7
pub const SID_SERVER_CONFIG_B: u32 = 0x1D38_F2F9; // issuer ids > 7 (retail)
/// Server-list fields of a server-config object. Services by field: 0x40 invite, 0x44 heartbeat,
/// 0x48 location (plain TCP), 0x50 leaderboard, 0x60 game list, 0x68 text filter, 0x78 report and ban
/// check, 0x7c login ticket; the others are not identified yet.
pub const SERVER_LIST_FIELDS: [usize; 13] = [
    0x40, 0x44, 0x48, 0x4C, 0x50, 0x54, 0x58, 0x5C, 0x60, 0x64, 0x68, 0x78, 0x7C,
];
/// Level bundles the client downloads before going online: { u32 count, pointer elements } with
/// 8-byte elements { pointer directory, pointer name }, fetched as `<dir>/<name>-7.psarc.crypt` from
/// the live-data host. The bundles only existed on the original host, and a missing one ends the
/// online session ("failed syncing levels"), so the served module lists none.
pub const SID_LEVEL_DOWNLOADS: u32 = 0x4E45_F19F;

/// The EULA shown before going online: `{count, elements}` with 0x14-byte elements { pointer title
/// prefix, u32 language, u32 version, u32 page count, pointer URL format with `%i` for the page }.
/// The client loads each page as an image (PNG when the URL ends in "png", else JPEG) into a
/// 0x20000-byte buffer; a failed page ends the online session ("Failed to download EULA").
pub const SID_EULA: u32 = 0x75E3_B9A9;
const EULA_ENTRY: usize = 0x14;

/// Points every EULA entry at `url` (a format with one `%i`) and sets its page count to `pages`.
/// Returns the new module and the number of entries.
pub fn rewrite_eula(module: &[u8], url: &str, pages: u32) -> Result<(Vec<u8>, usize)> {
    if !url.is_ascii() || url.contains('\0') || url.matches('%').count() != 1 || !url.contains("%i")
    {
        bail!("invalid EULA URL format {url:?}");
    }
    let m = Module::parse(module)?;
    let list = m.root(SID_EULA)?;
    let (count, elements) = (be32(module, list)?, be32(module, list + 4)?);
    for i in 0..count {
        let url_slot = elements + EULA_ENTRY * i + 0x10;
        if !m.is_pointer(url_slot) {
            bail!("EULA URL slot {url_slot:?} is not a pointer");
        }
    }
    let mut data = module[..m.data_end].to_vec();
    let at = data.len();
    data.extend_from_slice(url.as_bytes());
    data.push(0);
    data.resize(data.len().next_multiple_of(16), 0);
    for i in 0..count {
        let e = elements + EULA_ENTRY * i;
        data[e + 0xc..e + 0x10].copy_from_slice(&pages.to_be_bytes());
        data[e + 0x10..e + 0x14].copy_from_slice(&(at as u32).to_be_bytes());
    }
    let mut bitmap = m.bitmap.to_vec();
    bitmap.resize((data.len() / 4).div_ceil(8), 0);
    let end = data.len() as u32;
    data[8..12].copy_from_slice(&end.to_be_bytes());
    data.extend_from_slice(&(bitmap.len() as u32).to_be_bytes());
    data.extend_from_slice(&bitmap);
    Ok((data, count))
}

/// Matchmaking playlists: `{count, elements}` with 0x5c-byte elements { +0x00 playlist id, +0x10
/// pointer to the name ("net-playlists/<name>"), +0x14 minimum players, +0x18 maximum players (the
/// room size), +0x1c team size, … }. The minimum was confirmed on real consoles: a public host
/// abandons a lobby that stays below it for 12 s, and keeps it once the minimum is lowered.
pub const SID_PLAYLISTS: u32 = 0xC1B0_BE0D;
const PLAYLIST_ENTRY: usize = 0x5c;
const PLAYLIST_PREFIX: &str = "net-playlists/";
/// Key of a min-players override that applies to every playlist not named.
pub const ALL_PLAYLISTS: &str = "*";

#[derive(Debug, Clone, PartialEq)]
pub struct Playlist {
    /// The name without "net-playlists/".
    pub name: String,
    /// Offset of the 0x5c-byte record.
    pub record: usize,
    pub min_players: u32,
    pub max_players: u32,
    pub team_size: u32,
}

/// A playlist whose minimum player count the server changed.
#[derive(Debug, Clone, PartialEq)]
pub struct MinPlayersChange {
    pub name: String,
    pub from: u32,
    pub to: u32,
}

/// Sets the minimum player count of playlists in a finished module. `overrides` maps playlist names
/// (without "net-playlists/") or [`ALL_PLAYLISTS`] to a count from 1 to the playlist's maximum.
/// Every override is checked before anything is written; an empty map changes nothing.
pub fn set_min_players(
    module: &mut [u8],
    overrides: &std::collections::BTreeMap<String, u32>,
) -> Result<Vec<MinPlayersChange>> {
    if overrides.is_empty() {
        return Ok(Vec::new());
    }
    let playlists = Module::parse(module)?.playlists()?;
    let known = || {
        playlists
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(name) = overrides
        .keys()
        .find(|k| *k != ALL_PLAYLISTS && !playlists.iter().any(|p| &p.name == *k))
    {
        bail!(
            "no playlist {name:?} in the online config (playlists: {})",
            known()
        );
    }
    let mut changes = Vec::new();
    for p in &playlists {
        let Some(&to) = overrides
            .get(&p.name)
            .or_else(|| overrides.get(ALL_PLAYLISTS))
        else {
            continue;
        };
        if to == 0 || to > p.max_players {
            bail!(
                "min_players {to} for playlist {:?}: must be from 1 to its maximum, {}",
                p.name,
                p.max_players
            );
        }
        changes.push((
            p.record,
            MinPlayersChange {
                name: p.name.clone(),
                from: p.min_players,
                to,
            },
        ));
    }
    for (record, c) in &changes {
        module[record + 0x14..record + 0x18].copy_from_slice(&c.to.to_be_bytes());
    }
    Ok(changes
        .into_iter()
        .map(|(_, c)| c)
        .filter(|c| c.from != c.to)
        .collect())
}

/// Sets the count of the `{count, elements}` list at root symbol `sid` to 0 in a finished module.
/// Returns the previous count.
pub fn clear_list(module: &mut [u8], sid: u32) -> Result<usize> {
    let list = Module::parse(module)?.root(sid)?;
    let old = be32(module, list)?;
    module[list..list + 4].copy_from_slice(&0u32.to_be_bytes());
    Ok(old)
}

fn be32(b: &[u8], o: usize) -> Result<usize> {
    b.get(o..o + 4)
        .map(|x| u32::from_be_bytes(x.try_into().unwrap()) as usize)
        .with_context(|| format!("module offset {o:#x} out of range"))
}

pub struct Module<'a> {
    blob: &'a [u8],
    data_end: usize,
    bitmap: &'a [u8],
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerEntry {
    pub sid: u32,
    pub field: usize,
    /// Offset of the element's host pointer.
    pub element: usize,
    pub host: String,
    pub port: u32,
}

impl<'a> Module<'a> {
    pub fn parse(blob: &'a [u8]) -> Result<Self> {
        if blob.get(..4) != Some(b"DC00") || be32(blob, 4)? != 1 {
            bail!("not a compiled script module of this game");
        }
        let data_end = be32(blob, 8)?;
        let len = be32(blob, data_end)?;
        if len != (data_end / 4).div_ceil(8) || data_end + 4 + len != blob.len() {
            bail!("unexpected relocation bitmap size");
        }
        Ok(Self {
            blob,
            data_end,
            bitmap: &blob[data_end + 4..],
        })
    }

    fn is_pointer(&self, off: usize) -> bool {
        let w = off / 4;
        self.bitmap
            .get(w / 8)
            .is_some_and(|b| b >> (7 - w % 8) & 1 == 1)
    }

    fn root(&self, sid: u32) -> Result<usize> {
        let n = be32(self.blob, 0x14)?;
        (0..n)
            .map(|i| 0x18 + 12 * i)
            .find(|&e| be32(self.blob, e).ok() == Some(sid as usize))
            .map(|e| be32(self.blob, e + 8))
            .with_context(|| format!("module has no symbol {sid:#010x}"))?
    }

    fn cstr(&self, off: usize) -> Result<String> {
        let tail = self
            .blob
            .get(off..self.data_end)
            .context("string out of range")?;
        let end = tail
            .iter()
            .position(|&b| b == 0)
            .context("unterminated string")?;
        Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
    }

    pub fn server_entries(&self) -> Result<Vec<ServerEntry>> {
        let mut out = Vec::new();
        for sid in [SID_SERVER_CONFIG_A, SID_SERVER_CONFIG_B] {
            let cfg = self.root(sid)?;
            for field in SERVER_LIST_FIELDS {
                let hdr = be32(self.blob, cfg + field)?;
                let (count, elements) = (be32(self.blob, hdr)?, be32(self.blob, hdr + 4)?);
                for i in 0..count {
                    let e = elements + 8 * i;
                    if !self.is_pointer(e) {
                        bail!("server host slot {e:#x} is not a pointer");
                    }
                    let host = self.cstr(be32(self.blob, e)?)?;
                    out.push(ServerEntry {
                        sid,
                        field,
                        element: e,
                        host,
                        port: be32(self.blob, e + 4)? as u32,
                    });
                }
            }
        }
        Ok(out)
    }

    /// The matchmaking playlists (see [`SID_PLAYLISTS`]).
    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        let list = self.root(SID_PLAYLISTS)?;
        let (count, elements) = (be32(self.blob, list)?, be32(self.blob, list + 4)?);
        if count > 64 {
            bail!("unexpected playlist count {count}");
        }
        (0..count)
            .map(|i| {
                let r = elements + PLAYLIST_ENTRY * i;
                if !self.is_pointer(r + 0x10) {
                    bail!("playlist name slot {:#x} is not a pointer", r + 0x10);
                }
                let full = self.cstr(be32(self.blob, r + 0x10)?)?;
                let name = full
                    .strip_prefix(PLAYLIST_PREFIX)
                    .with_context(|| format!("unexpected playlist name {full:?}"))?;
                let field = |o: usize| be32(self.blob, r + o).map(|v| v as u32);
                Ok(Playlist {
                    name: name.to_string(),
                    record: r,
                    min_players: field(0x14)?,
                    max_players: field(0x18)?,
                    team_size: field(0x1c)?,
                })
            })
            .collect()
    }

    /// Appends `host` to the data and points every server entry that has a host at it; ports are
    /// kept. Everything else in the module is unchanged.
    pub fn with_host(&self, host: &str) -> Result<Vec<u8>> {
        if host.is_empty() || !host.is_ascii() || host.contains('\0') {
            bail!("invalid host {host:?}");
        }
        let entries = self.server_entries()?;
        let mut data = self.blob[..self.data_end].to_vec();
        let at = data.len();
        data.extend_from_slice(host.as_bytes());
        data.push(0);
        data.resize(data.len().next_multiple_of(16), 0);
        for e in entries.iter().filter(|e| !e.host.is_empty()) {
            data[e.element..e.element + 4].copy_from_slice(&(at as u32).to_be_bytes());
        }
        let mut bitmap = self.bitmap.to_vec();
        bitmap.resize((data.len() / 4).div_ceil(8), 0);
        let end = data.len() as u32;
        data[8..12].copy_from_slice(&end.to_be_bytes());
        data.extend_from_slice(&(bitmap.len() as u32).to_be_bytes());
        data.extend_from_slice(&bitmap);
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal module: two server configs whose lists all share one element (host "10.0.0.1",
    /// port 6300), plus one unrelated root symbol.
    fn sample() -> Vec<u8> {
        let mut b = vec![0u8; 0x18 + 3 * 12];
        let mut ptrs = Vec::new();
        let host = b.len();
        b.extend_from_slice(b"10.0.0.1\0\0\0\0");
        let elem = b.len();
        ptrs.push(elem);
        b.extend_from_slice(&(host as u32).to_be_bytes());
        b.extend_from_slice(&6300u32.to_be_bytes());
        let hdr = b.len();
        ptrs.push(hdr + 4);
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&(elem as u32).to_be_bytes());
        let mut cfgs = Vec::new();
        for _ in 0..2 {
            let cfg = b.len();
            cfgs.push(cfg);
            b.resize(cfg + 0x80, 0);
            for f in SERVER_LIST_FIELDS {
                b[cfg + f..cfg + f + 4].copy_from_slice(&(hdr as u32).to_be_bytes());
                ptrs.push(cfg + f);
            }
        }
        let other = b.len();
        b.extend_from_slice(&[0xAB; 8]);
        let roots = [
            (0x1D38_F2F9u32, cfgs[1]),
            (0x5000_0000, other),
            (0x9BD0_38AC, cfgs[0]),
        ];
        for (i, (sid, p)) in roots.iter().enumerate() {
            let e = 0x18 + 12 * i;
            b[e..e + 4].copy_from_slice(&sid.to_be_bytes());
            b[e + 8..e + 12].copy_from_slice(&(*p as u32).to_be_bytes());
            ptrs.push(e + 8);
        }
        b[..4].copy_from_slice(b"DC00");
        b[4..8].copy_from_slice(&1u32.to_be_bytes());
        b[16..20].copy_from_slice(&1u32.to_be_bytes());
        b[20..24].copy_from_slice(&3u32.to_be_bytes());
        b.resize(b.len().next_multiple_of(16), 0);
        let end = b.len();
        b[8..12].copy_from_slice(&(end as u32).to_be_bytes());
        let mut bitmap = vec![0u8; (end / 4).div_ceil(8)];
        for p in ptrs {
            bitmap[p / 4 / 8] |= 0x80 >> (p / 4 % 8);
        }
        b.extend_from_slice(&(bitmap.len() as u32).to_be_bytes());
        b.extend_from_slice(&bitmap);
        b
    }

    /// The sample with data appended by `add`, which pushes the offsets of its pointer words and
    /// returns the offset the root symbol `sid` points to. The sample's unrelated root (sid
    /// 0x50000000) is reused for it; root order does not matter to `Module::root`.
    fn sample_with(sid: u32, add: impl FnOnce(&mut Vec<u8>, &mut Vec<usize>) -> usize) -> Vec<u8> {
        let mut b = sample();
        let old_end = u32::from_be_bytes(b[8..12].try_into().unwrap()) as usize;
        let mut bitmap = b[old_end + 4..].to_vec();
        b.truncate(old_end);
        let mut ptrs = Vec::new();
        let target = add(&mut b, &mut ptrs);
        b[0x18 + 12..0x18 + 16].copy_from_slice(&sid.to_be_bytes());
        b[0x18 + 20..0x18 + 24].copy_from_slice(&(target as u32).to_be_bytes());
        b.resize(b.len().next_multiple_of(16), 0);
        let end = b.len();
        b[8..12].copy_from_slice(&(end as u32).to_be_bytes());
        bitmap.resize((end / 4).div_ceil(8), 0);
        for p in ptrs {
            bitmap[p / 4 / 8] |= 0x80 >> (p / 4 % 8);
        }
        b.extend_from_slice(&(bitmap.len() as u32).to_be_bytes());
        b.extend_from_slice(&bitmap);
        b
    }

    /// The sample with a playlist list of (name, minimum, maximum) records, team size 5.
    fn sample_with_playlists(lists: &[(&str, u32, u32)]) -> Vec<u8> {
        sample_with(SID_PLAYLISTS, |b, ptrs| {
            let names: Vec<usize> = lists
                .iter()
                .map(|(name, ..)| {
                    let at = b.len();
                    b.extend_from_slice(format!("{PLAYLIST_PREFIX}{name}\0").as_bytes());
                    b.resize(b.len().next_multiple_of(4), 0);
                    at
                })
                .collect();
            let elements = b.len();
            for (i, (_, min, max)) in lists.iter().enumerate() {
                let r = b.len();
                b.resize(r + PLAYLIST_ENTRY, 0);
                b[r..r + 4].copy_from_slice(&(0x1000 + i as u32).to_be_bytes());
                b[r + 0x10..r + 0x14].copy_from_slice(&(names[i] as u32).to_be_bytes());
                ptrs.push(r + 0x10);
                for (o, v) in [(0x14, *min), (0x18, *max), (0x1c, 5)] {
                    b[r + o..r + o + 4].copy_from_slice(&v.to_be_bytes());
                }
            }
            let list = b.len();
            b.extend_from_slice(&(lists.len() as u32).to_be_bytes());
            b.extend_from_slice(&(elements as u32).to_be_bytes());
            ptrs.push(list + 4);
            list
        })
    }

    #[test]
    fn reads_playlists() {
        let b = sample_with_playlists(&[("versus", 6, 10), ("coop", 2, 3)]);
        let lists = Module::parse(&b).unwrap().playlists().unwrap();
        let summary: Vec<_> = lists
            .iter()
            .map(|p| (p.name.as_str(), p.min_players, p.max_players, p.team_size))
            .collect();
        assert_eq!(summary, [("versus", 6, 10, 5), ("coop", 2, 3, 5)]);
    }

    /// Named overrides win over "*"; only the minimum fields change; unchanged values are not reported.
    #[test]
    fn sets_min_players() {
        let base = sample_with_playlists(&[("versus", 6, 10), ("duel", 4, 4), ("coop", 2, 3)]);
        let mut out = base.clone();
        let overrides = [("*".to_string(), 2), ("duel".to_string(), 3)]
            .into_iter()
            .collect();
        let changes = set_min_players(&mut out, &overrides).unwrap();
        let summary: Vec<_> = changes
            .iter()
            .map(|c| (c.name.as_str(), c.from, c.to))
            .collect();
        assert_eq!(summary, [("versus", 6, 2), ("duel", 4, 3)]);
        let lists = Module::parse(&out).unwrap().playlists().unwrap();
        assert_eq!(
            lists.iter().map(|p| p.min_players).collect::<Vec<_>>(),
            [2, 3, 2]
        );
        let mins: Vec<usize> = lists
            .iter()
            .flat_map(|p| p.record + 0x14..p.record + 0x18)
            .collect();
        assert!(
            (0..base.len())
                .filter(|&i| base[i] != out[i])
                .all(|i| mins.contains(&i))
        );
    }

    #[test]
    fn rejects_bad_min_players() {
        let base = sample_with_playlists(&[("versus", 6, 10), ("duel", 4, 4)]);
        for (name, n) in [("missing", 2), ("versus", 0), ("duel", 5), ("*", 11)] {
            let mut out = base.clone();
            let overrides = [(name.to_string(), n)].into_iter().collect();
            assert!(
                set_min_players(&mut out, &overrides).is_err(),
                "{name} = {n} must be rejected"
            );
            assert_eq!(out, base, "a rejected override must not change the module");
        }
        let mut out = base.clone();
        assert!(
            set_min_players(&mut out, &Default::default())
                .unwrap()
                .is_empty()
        );
        assert_eq!(out, base);
    }

    #[test]
    fn repoints_every_host_and_keeps_the_rest() {
        let base = sample();
        let m = Module::parse(&base).unwrap();
        let before = m.server_entries().unwrap();
        assert_eq!(before.len(), 26);
        let out = m.with_host("192.168.1.50").unwrap();
        let m2 = Module::parse(&out).unwrap();
        let after = m2.server_entries().unwrap();
        assert!(
            after
                .iter()
                .all(|e| e.host == "192.168.1.50" && e.port == 6300)
        );
        let old_end = u32::from_be_bytes(base[8..12].try_into().unwrap()) as usize;
        let changed: Vec<usize> = (0..old_end).filter(|&i| base[i] != out[i]).collect();
        // Only the data-end field and the one shared host pointer change.
        assert!(
            changed.iter().all(|&i| (8..12).contains(&i)
                || (before[0].element..before[0].element + 4).contains(&i))
        );
        assert_eq!(&out[old_end..old_end + 13], b"192.168.1.50\0");
    }

    #[test]
    fn clears_a_root_list() {
        let base = sample();
        let mut out = Module::parse(&base)
            .unwrap()
            .with_host("192.168.1.50")
            .unwrap();
        // The sample's server-config A root points at an 0x80-byte object; use its first word as a "count".
        let cfg = Module::parse(&out)
            .unwrap()
            .root(SID_SERVER_CONFIG_A)
            .unwrap();
        out[cfg..cfg + 4].copy_from_slice(&18u32.to_be_bytes());
        assert_eq!(clear_list(&mut out, SID_SERVER_CONFIG_A).unwrap(), 18);
        assert_eq!(&out[cfg..cfg + 4], &[0, 0, 0, 0]);
        assert!(Module::parse(&out).unwrap().server_entries().is_ok());
        assert!(clear_list(&mut out, 0x1234_5678).is_err());
    }

    #[test]
    fn rewrites_eula_entries() {
        // A module with a two-entry EULA list appended to the sample.
        let mut elems = 0;
        let b = sample_with(SID_EULA, |b, ptrs| {
            let url = b.len();
            b.extend_from_slice(b"http://x/A-%i.jpg\0\0\0\0\0\0\0");
            elems = b.len();
            for _ in 0..2 {
                let e = b.len();
                b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 16]);
                b.extend_from_slice(&(url as u32).to_be_bytes());
                ptrs.push(e + 0x10);
            }
            let list = b.len();
            b.extend_from_slice(&2u32.to_be_bytes());
            b.extend_from_slice(&(elems as u32).to_be_bytes());
            ptrs.push(list + 4);
            list
        });

        let (out, n) = rewrite_eula(&b, "http://10.0.0.5/eula/page-%i.png", 2).unwrap();
        assert_eq!(n, 2);
        let m = Module::parse(&out).unwrap();
        for i in 0..2 {
            let e = elems + EULA_ENTRY * i;
            assert_eq!(be32(&out, e + 0xc).unwrap(), 2);
            assert!(m.is_pointer(e + 0x10));
            assert_eq!(
                m.cstr(be32(&out, e + 0x10).unwrap()).unwrap(),
                "http://10.0.0.5/eula/page-%i.png"
            );
        }
        assert!(m.server_entries().is_ok());
        assert!(rewrite_eula(&b, "http://x/%s", 2).is_err());
    }

    #[test]
    fn rejects_other_files() {
        assert!(Module::parse(b"PSAR....").is_err());
        let mut bad = sample();
        bad.push(0);
        assert!(Module::parse(&bad).is_err());
    }
}
