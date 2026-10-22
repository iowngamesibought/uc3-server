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
