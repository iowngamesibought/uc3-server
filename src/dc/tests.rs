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
        changed
            .iter()
            .all(|&i| (8..12).contains(&i)
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
