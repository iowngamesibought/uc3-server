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
