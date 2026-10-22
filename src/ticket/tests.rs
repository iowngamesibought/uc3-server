use super::*;

fn field(ty: u16, data: &[u8]) -> Vec<u8> {
    let mut v = ty.to_be_bytes().to_vec();
    v.extend_from_slice(&(data.len() as u16).to_be_bytes());
    v.extend_from_slice(data);
    v
}

#[test]
fn parses_synthetic_ticket() {
    let mut online = b"TestPlayer".to_vec();
    online.resize(0x20, 0);
    let mut body = Vec::new();
    body.extend(field(8, &[0; 0x14]));
    body.extend(field(1, &5u32.to_be_bytes()));
    body.extend(field(7, &1u64.to_be_bytes()));
    body.extend(field(7, &2u64.to_be_bytes()));
    body.extend(field(2, &42u64.to_be_bytes()));
    body.extend(field(8, &online));
    body.extend(field(8, b"us\0\0"));
    body.extend(field(4, b"un\0\0"));
    body.extend(field(8, b"XX0000-TEST00000_00\0\0\0\0\0"));
    let mut t = 0x2101_0000u32.to_be_bytes().to_vec();
    let sec = field(0x3000, &body);
    t.extend_from_slice(&(sec.len() as u32).to_be_bytes());
    t.extend(sec);
    let info = parse(&t).unwrap();
    assert_eq!(info.issuer_id, Some(5));
    assert_eq!(info.user_id, Some(42));
    assert_eq!(info.online_id.as_deref(), Some("TestPlayer"));
    assert_eq!(info.service_id.as_deref(), Some("XX0000-TEST00000_00"));
}
