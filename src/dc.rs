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
mod tests;
