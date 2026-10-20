//! Best-effort parser for the login ticket the client sends during the login handshake.
//! Used for logging and session identity only; the handshake itself just echoes the ticket.
//! Layout (publicly documented): u32 version, u32 size, then typed fields { u16 type, u16 length,
//! data }, where types 0x3000/0x3002 are nested sections (body, footer).
//! Signature verification is not performed.

#[derive(Debug, Default, Clone)]
#[allow(dead_code)] // parsed for completeness; not all fields are used
pub struct TicketInfo {
    pub version: u32,
    pub issuer_id: Option<u32>,
    pub user_id: Option<u64>,
    pub online_id: Option<String>,
    pub region: Option<String>,
    pub domain: Option<String>,
    pub service_id: Option<String>,
    pub issued_ms: Option<u64>,
    pub expires_ms: Option<u64>,
}

struct Field<'a> {
    ty: u16,
    data: &'a [u8],
}

fn fields(mut b: &[u8]) -> Vec<Field<'_>> {
    let mut out = Vec::new();
    while b.len() >= 4 {
        let ty = u16::from_be_bytes([b[0], b[1]]);
        let len = u16::from_be_bytes([b[2], b[3]]) as usize;
        if b.len() < 4 + len {
            break;
        }
        out.push(Field {
            ty,
            data: &b[4..4 + len],
        });
        b = &b[4 + len..];
    }
    out
}

fn text(d: &[u8]) -> String {
    let end = d.iter().position(|&c| c == 0).unwrap_or(d.len());
    String::from_utf8_lossy(&d[..end]).into_owned()
}

fn be_u64(d: &[u8]) -> Option<u64> {
    d.try_into().ok().map(u64::from_be_bytes)
}
fn be_u32(d: &[u8]) -> Option<u32> {
    d.try_into().ok().map(u32::from_be_bytes)
}

pub fn parse(ticket: &[u8]) -> Option<TicketInfo> {
    if ticket.len() < 8 {
        return None;
    }
    let mut info = TicketInfo {
        version: u32::from_be_bytes(ticket[..4].try_into().ok()?),
        ..Default::default()
    };
    let top = fields(&ticket[8..]);
    let body = top.iter().find(|f| f.ty == 0x3000)?;
    // Body field order: serial, issuer id, issued, expires, user id, online id, region, domain,
    // service id, status, ...
    let f = fields(body.data);
    let get = |i: usize| f.get(i).map(|x| x.data);
    info.issuer_id = get(1).and_then(be_u32);
    info.issued_ms = get(2).and_then(be_u64);
    info.expires_ms = get(3).and_then(be_u64);
    info.user_id = get(4).and_then(be_u64);
    info.online_id = get(5).map(text);
    info.region = get(6).map(text);
    info.domain = get(7).map(text);
    info.service_id = get(8).map(text);
    Some(info)
}

#[cfg(test)]
mod tests {
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
}
