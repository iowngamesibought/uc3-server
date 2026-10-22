use super::*;
use crate::framecipher::seal;

fn client_hello(nonce: u32, service: &str) -> [u8; HELLO_LEN] {
    let mut h = [0u8; HELLO_LEN];
    h[0] = 0x11;
    h[4..8].copy_from_slice(&nonce.to_be_bytes());
    h[8..0x18].fill(0xEE); // the client leaves these bytes uninitialised
    h[0x18..0x18 + service.len()].copy_from_slice(service.as_bytes());
    h
}

#[test]
fn parses_hello() {
    let h = parse_hello(&client_hello(0x0123_4567, "heartbeat-server")).unwrap();
    assert_eq!(
        (h.client_nonce, h.service.as_str()),
        (0x0123_4567, "heartbeat-server")
    );
    assert!(parse_hello(&[0u8; HELLO_LEN]).is_err());
}

/// Plays the client side over a real socket: hello, one request frame, one reply frame.
#[tokio::test]
async fn serves_one_request() {
    let key = Arc::new(FrameKey::new(&[7; 16]));
    let services = Arc::new(Services::new(Default::default()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (k2, s2) = (key.clone(), services.clone());
    tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        connection(stream, peer, addr.port(), &k2, &s2, &Throttle::default())
            .await
            .unwrap();
    });
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(&client_hello(42, "report-server"))
        .await
        .unwrap();
    let mut r = [0u8; 8];
    c.read_exact(&mut r).await.unwrap();
    assert_eq!(r[0], b'"');
    let server_nonce = u32::from_be_bytes(r[4..].try_into().unwrap());
    c.write_all(&seal(&key, server_nonce, b"is-banned TESTER\n"))
        .await
        .unwrap();
    let mut header = [0u8; HEADER];
    c.read_exact(&mut header).await.unwrap();
    let mut frame = header.to_vec();
    frame.resize(framecipher::frame_len(&header).unwrap(), 0);
    c.read_exact(&mut frame[HEADER..]).await.unwrap();
    let reply = framecipher::open(&key, 42, &frame).unwrap();
    assert!(
        !reply.starts_with(b"+"),
        "a ban check must not answer '+' (banned)"
    );
}
