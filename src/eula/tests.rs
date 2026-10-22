use super::*;

#[test]
fn crc_matches_the_standard_check_value() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

#[test]
fn pages_are_valid_pngs_that_fit_the_client_buffer() {
    let p = page(1);
    assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(&p[12..16], b"IHDR");
    assert_eq!(
        u32::from_be_bytes(p[16..20].try_into().unwrap()),
        WIDTH as u32
    );
    assert!(p.len() < 0x20000, "{} bytes", p.len());
    // The pixel data decompresses back to the full image.
    let idat_len = u32::from_be_bytes(p[33..37].try_into().unwrap()) as usize;
    assert_eq!(&p[37..41], b"IDAT");
    let mut raw = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::ZlibDecoder::new(&p[41..41 + idat_len]),
        &mut raw,
    )
    .unwrap();
    assert_eq!(raw.len(), HEIGHT * (1 + WIDTH * 3));
    assert!(p.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]));
}
