use super::*;
use tokio::io::AsyncReadExt;

#[test]
fn connections_per_address_are_capped() {
    let conns = Connections::default();
    let (a, b): (IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
    let held: Vec<Slot> = (0..MAX_CONNECTIONS_PER_IP)
        .map(|_| conns.open(a).unwrap())
        .collect();
    assert!(conns.open(a).is_none(), "one over the cap is refused");
    assert!(conns.open(b).is_some(), "other addresses are unaffected");
    drop(held);
    assert!(
        conns.open(a).is_some(),
        "closed connections give their places back"
    );
    assert!(
        conns.0.lock().unwrap().is_empty(),
        "addresses with no open connection are forgotten"
    );
}

/// Over a real socket: the connection over the cap is closed at once, the others stay open.
#[tokio::test]
async fn a_connection_over_the_cap_is_closed_at_once() {
    let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = inner.local_addr().unwrap();
    let listener = Listener::new(inner, "test");
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            held.push(listener.accept().await);
        }
    });
    let mut clients = Vec::new();
    for _ in 0..MAX_CONNECTIONS_PER_IP {
        clients.push(TcpStream::connect(addr).await.unwrap());
    }
    let mut extra = TcpStream::connect(addr).await.unwrap();
    let mut b = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(5), extra.read(&mut b))
        .await
        .expect("the extra connection is closed at once");
    assert!(matches!(closed, Ok(0) | Err(_)), "{closed:?}");
    let held = tokio::time::timeout(Duration::from_millis(200), clients[0].read(&mut b)).await;
    assert!(held.is_err(), "connections under the cap stay open");
}

#[test]
fn throttle_limits_lines_per_minute_and_counts_the_rest() {
    let t = Throttle::new(2);
    let start = t.0.lock().unwrap().start;
    assert_eq!(t.allow_at(start), Some(0));
    assert_eq!(t.allow_at(start), Some(0));
    assert_eq!(t.allow_at(start), None);
    assert_eq!(t.allow_at(start + Duration::from_secs(59)), None);
    assert_eq!(
        t.allow_at(start + Duration::from_secs(60)),
        Some(2),
        "the next minute's first line reports the dropped ones"
    );
    assert_eq!(t.allow_at(start + Duration::from_secs(60)), Some(0));
}
