//! The EULA pages the client shows before going online. The original pages are no longer available;
//! the server serves its own notice instead, as PNG images drawn with a small built-in font.

use std::io::Write;

const WIDTH: usize = 960;
const HEIGHT: usize = 540;
const SCALE: usize = 3;
const BACKGROUND: [u8; 3] = [16, 20, 28];
const FOREGROUND: [u8; 3] = [230, 230, 230];
/// Pages per language written into the served config.
pub const PAGES: u32 = 2;
/// Page numbers served; the client formats its own page index into the URL.
pub const SERVED_PAGES: std::ops::RangeInclusive<u32> = 0..=16;

const TEXT: [&str; 10] = [
    "UC3 COMMUNITY SERVER",
    "",
    "THIS ONLINE SERVICE IS RUN BY THE COMMUNITY.",
    "IT IS NOT OPERATED OR ENDORSED BY THE GAME'S",
    "DEVELOPER OR PUBLISHER.",
    "",
    "THE ORIGINAL ONLINE TERMS ARE NO LONGER",
    "AVAILABLE. PLAY AT YOUR OWN DISCRETION.",
    "",
    "ACCEPT TO CONTINUE.",
];

/// 5x7 glyphs, one byte per row, bit 4 = leftmost column.
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '.' => [0, 0, 0, 0, 0, 0x0C, 0x0C],
        ',' => [0, 0, 0, 0, 0x0C, 0x04, 0x08],
        '-' => [0, 0, 0, 0x1F, 0, 0, 0],
        ':' => [0, 0x0C, 0x0C, 0, 0x0C, 0x0C, 0],
        '/' => [0, 0x01, 0x02, 0x04, 0x08, 0x10, 0],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '\'' => [0x04, 0x04, 0x08, 0, 0, 0, 0],
        _ => [0; 7],
    }
}

fn render(lines: &[String]) -> Vec<u8> {
    let mut px = vec![0u8; WIDTH * HEIGHT * 3];
    for p in px.chunks_mut(3) {
        p.copy_from_slice(&BACKGROUND);
    }
    let (cell_w, line_h) = (6 * SCALE, 12 * SCALE);
    let top = (HEIGHT.saturating_sub(lines.len() * line_h)) / 2;
    for (row, line) in lines.iter().enumerate() {
        let left = WIDTH.saturating_sub(line.chars().count() * cell_w) / 2;
        for (col, c) in line.chars().enumerate() {
            for (gy, bits) in glyph(c).iter().enumerate() {
                for gx in 0..5 {
                    if bits >> (4 - gx) & 1 == 0 {
                        continue;
                    }
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            let (x, y) = (
                                left + col * cell_w + gx * SCALE + dx,
                                top + row * line_h + gy * SCALE + dy,
                            );
                            if x < WIDTH && y < HEIGHT {
                                let i = (y * WIDTH + x) * 3;
                                px[i..i + 3].copy_from_slice(&FOREGROUND);
                            }
                        }
                    }
                }
            }
        }
    }
    px
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// An 8-bit RGB PNG of the given pixels (WIDTH x HEIGHT).
fn png(px: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(HEIGHT * (1 + WIDTH * 3));
    for row in px.chunks(WIDTH * 3) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&raw).expect("in-memory write");
    let idat = z.finish().expect("in-memory write");
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(WIDTH as u32).to_be_bytes());
    ihdr.extend_from_slice(&(HEIGHT as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, RGB, deflate, no filter set, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// The notice page image for page `index` (1-based in the client's numbering, shown as "n / PAGES").
pub fn page(index: u32) -> Vec<u8> {
    let mut lines: Vec<String> = TEXT.iter().map(|s| s.to_string()).collect();
    lines.push(String::new());
    lines.push(format!("PAGE {} / {}", index.clamp(1, PAGES), PAGES));
    png(&render(&lines))
}

#[cfg(test)]
mod tests {
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
}
