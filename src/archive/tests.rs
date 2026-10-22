use super::*;

#[test]
fn archive_round_trip() {
    let blob = build_archive(&[("2.networking", b"hello"), ("2.common", &[7u8; 0x12345])]);
    assert_eq!(
        archive_manifest(&blob).unwrap(),
        ["2.networking", "2.common"]
    );
    assert_eq!(read_archive_entry(&blob, "2.networking").unwrap(), b"hello");
    assert_eq!(
        read_archive_entry(&blob, "2.common").unwrap(),
        vec![7u8; 0x12345]
    );
    assert!(read_archive_entry(&blob, "3.networking").is_err());
}

#[test]
fn entry_by_suffix_matches_full_paths() {
    let blob = build_archive(&[
        ("build/main/bin/dc1/net.bin", b"a"),
        ("build/main/text1/2.networking", b"b"),
    ]);
    assert_eq!(read_entry_by_suffix(&blob, "dc1/net.bin").unwrap().1, b"a");
    assert_eq!(
        read_entry_by_suffix(&blob, "2.networking").unwrap().0,
        "build/main/text1/2.networking"
    );
    assert!(read_entry_by_suffix(&blob, "et.bin").is_err());
}
