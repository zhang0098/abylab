//! Kitty-graphics helpers: chat image thumbnails, the composer attachment
//! previews, and the theme background layer. The composer pet sprite was
//! removed; these placement primitives are what remains.

use std::io::{self, Write};
use std::sync::OnceLock;

use ratatui::layout::Rect;

/// Kitty graphics chunk payload limit (base64 chars per escape).
const CHUNK: usize = 4096;

/// Emitted by `restore_terminal` so panics also clear any placement.
pub const KITTY_DELETE_ALL: &str = "\x1b_Ga=d,d=A,q=2\x1b\\";

/// Does the terminal speak the kitty graphics protocol? Env-based sniff:
/// querying would race the crossterm input thread for the reply bytes.
pub fn kitty_supported() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        let term = std::env::var("TERM").unwrap_or_default();
        let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
        term.contains("kitty")
            || term.contains("ghostty")
            || prog.eq_ignore_ascii_case("ghostty")
            || prog.eq_ignore_ascii_case("wezterm")
    })
}

/// Parse pixel dimensions from a PNG (the only format thumbnailed without a
/// decoder — clipboard screenshots are PNG; other formats fall back to text).
pub fn image_dims(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    Some((w, h))
}

/// One visible image thumbnail the terminal should currently show.
pub struct ThumbShot<'a> {
    pub id: u32,
    pub rect: Rect,
    pub data: &'a [u8],
}

/// Keeps kitty-graphics thumbnail placements in sync with the visible chat
/// viewport: emit when new/moved, delete when scrolled away.
#[derive(Default)]
pub struct Thumbnails {
    shown: std::collections::HashMap<u32, Rect>,
}

impl Thumbnails {
    pub fn new() -> Self {
        Thumbnails::default()
    }

    pub fn sync(&mut self, out: &mut impl Write, visible: &[ThumbShot]) -> io::Result<()> {
        use std::collections::HashSet;
        let mut ids = HashSet::new();
        for shot in visible {
            ids.insert(shot.id);
            if self.shown.get(&shot.id) != Some(&shot.rect) {
                if self.shown.remove(&shot.id).is_some() {
                    delete_kitty(out, shot.id)?;
                }
                emit_kitty(out, shot.id, shot.data, shot.rect, 0)?;
                self.shown.insert(shot.id, shot.rect);
            }
        }
        let gone: Vec<u32> = self
            .shown
            .keys()
            .copied()
            .filter(|id| !ids.contains(id))
            .collect();
        for id in gone {
            delete_kitty(out, id)?;
            self.shown.remove(&id);
        }
        out.flush()
    }
}

fn emit_kitty(
    out: &mut impl Write,
    id: u32,
    data: &[u8],
    cell: Rect,
    z_index: i32,
) -> io::Result<()> {
    write!(out, "\x1b7\x1b[{};{}H", cell.y + 1, cell.x + 1)?;
    let b64 = base64(data);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(CHUNK).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 != chunks.len());
        if i == 0 {
            write!(
                out,
                "\x1b_Ga=T,f=100,i={id},c={},r={},C=1,z={z_index},q=2,m={more};",
                cell.width, cell.height,
            )?;
        } else {
            write!(out, "\x1b_Gm={more};")?;
        }
        out.write_all(chunk)?;
        write!(out, "\x1b\\")?;
    }
    write!(out, "\x1b8")
}

fn delete_kitty(out: &mut impl Write, id: u32) -> io::Result<()> {
    write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

/// Decode standard base64 (RFC 4648, with padding). Whitespace is ignored.
/// Standard base64 (RFC 4648, with padding) — small enough to not need a dep.
pub(crate) fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = u32::from_be_bytes([
            0,
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ]);
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real 192×208 RGBA PNG (same geometry as the former pet sprite)
    /// for the backdrop placement tests.

    #[test]
    fn base64_rfc4648_vectors() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(raw.as_bytes()), enc);
        }
    }

    #[test]
    fn image_dims_reads_png_ihdr() {
        let mut png = vec![0u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&320u32.to_be_bytes());
        png[20..24].copy_from_slice(&240u32.to_be_bytes());
        assert_eq!(image_dims(&png), Some((320, 240)));
        assert_eq!(image_dims(b"not a png"), None);
        assert_eq!(image_dims(&png[..20]), None, "truncated header");
    }
}
