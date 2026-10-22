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
