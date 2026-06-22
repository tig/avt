//! Kitty graphics protocol decoding.
//!
//! Kitty graphics arrive in an APC string of the form
//! `ESC _ G <key=val,...> ; <base64 payload> ESC \`. The parser strips the
//! envelope and hands the terminal everything after the `G` introducer; this
//! module parses the control keys ([`parse_command`]) and decodes raw RGBA/RGB
//! pixel payloads ([`decode_raster`]) into the same packed [`Sixel`] raster the
//! sixel decoder produces, so both image sources share one placement path.
//!
//! Only the subset emitted by the target apps is handled: direct transmission
//! (`a=T`) of raw 32-bit RGBA (`f=32`, or 24-bit RGB `f=24` as opaque),
//! optionally split across continuation chunks (`m=1`/`m=0`), plus delete by id
//! (`a=d,d=i`). PNG (`f=100`) and other formats are rejected.

use crate::sixel::Sixel;
use rgb::RGBA8;

// Upper bound on a decoded image's pixel count, mirroring the sixel decoder: a
// header can declare an enormous canvas while sending almost no data, so cap the
// area before allocating. 16M px (~64 MiB of RGBA8) is far larger than any
// legitimate terminal image.
const MAX_PIXELS: usize = 16_000_000;

/// The `a=` action of a Kitty graphics command. An omitted `a` key defaults to
/// transmit-only, matching the Kitty spec; continuation chunks (also keyless)
/// inherit this but still route through accumulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Action {
    /// `a=T`: transmit and display.
    TransmitAndDisplay,
    /// `a=t`: transmit only (store without displaying). The default when `a` is
    /// omitted.
    #[default]
    Transmit,
    /// `a=p`: place a previously transmitted image.
    Put,
    /// `a=d`: delete images.
    Delete,
    /// `a=q`: query support.
    Query,
    /// Any other action.
    Other,
}

/// The parsed control keys of a Kitty graphics command plus its (verbatim,
/// still base64) payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    pub action: Action,
    /// `f`: pixel format (32 = RGBA, 24 = RGB, 100 = PNG). Defaults to 32.
    pub format: u32,
    /// `s`: source pixel width.
    pub width: usize,
    /// `v`: source pixel height.
    pub height: usize,
    /// `c`: display width in cells.
    pub cols: usize,
    /// `r`: display height in cells.
    pub rows: usize,
    /// `i`: image id.
    pub id: Option<u32>,
    /// `m`: whether more chunks follow.
    pub more: bool,
    /// `d`: delete target (the character, e.g. `i`/`I`).
    pub delete_target: Option<char>,
    /// The base64 payload, kept verbatim.
    pub payload: String,
}

impl Default for Command {
    fn default() -> Self {
        Command {
            action: Action::default(),
            format: 32,
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            id: None,
            more: false,
            delete_target: None,
            payload: String::new(),
        }
    }
}

/// Parse the part of a Kitty graphics APC after the `G` introducer:
/// `<keys>;<payload>`. Unknown keys and unparsable values are ignored; the
/// payload is kept verbatim.
pub(crate) fn parse_command(rest: &str) -> Command {
    let (keys, payload) = rest.split_once(';').unwrap_or((rest, ""));

    let mut cmd = Command {
        payload: payload.to_string(),
        ..Default::default()
    };

    for kv in keys.split(',') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };

        match k {
            "a" => {
                cmd.action = match v {
                    "T" => Action::TransmitAndDisplay,
                    "t" => Action::Transmit,
                    "p" => Action::Put,
                    "d" => Action::Delete,
                    "q" => Action::Query,
                    _ => Action::Other,
                };
            }
            "f" => {
                if let Ok(n) = v.parse() {
                    cmd.format = n;
                }
            }
            "s" => {
                if let Ok(n) = v.parse() {
                    cmd.width = n;
                }
            }
            "v" => {
                if let Ok(n) = v.parse() {
                    cmd.height = n;
                }
            }
            "c" => {
                if let Ok(n) = v.parse() {
                    cmd.cols = n;
                }
            }
            "r" => {
                if let Ok(n) = v.parse() {
                    cmd.rows = n;
                }
            }
            "i" => {
                if let Ok(n) = v.parse() {
                    cmd.id = Some(n);
                }
            }
            "m" => {
                cmd.more = v == "1";
            }
            "d" => {
                cmd.delete_target = v.chars().next();
            }
            _ => {}
        }
    }

    cmd
}

/// Decode a raw raster payload (the accumulated base64) into a packed RGBA
/// [`Sixel`]. Supports `f=32` (RGBA) and `f=24` (RGB, made opaque). Returns
/// `None` for unsupported formats (e.g. PNG `100`), zero/oversized dimensions,
/// invalid base64, or a payload too short for `width * height` pixels.
pub(crate) fn decode_raster(
    format: u32,
    width: usize,
    height: usize,
    payload: &str,
) -> Option<Sixel> {
    let channels = match format {
        32 => 4,
        24 => 3,
        _ => return None,
    };

    if width == 0 || height == 0 {
        return None;
    }

    let area = width.checked_mul(height).filter(|&a| a <= MAX_PIXELS)?;
    let needed = area.checked_mul(channels)?;
    let bytes = decode_base64(payload)?;

    if bytes.len() < needed {
        return None;
    }

    let mut pixels = Vec::with_capacity(area);

    for i in 0..area {
        let off = i * channels;

        let pixel = if channels == 4 {
            RGBA8::new(bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3])
        } else {
            RGBA8::new(bytes[off], bytes[off + 1], bytes[off + 2], 255)
        };

        pixels.push(pixel);
    }

    Some(Sixel {
        width,
        height,
        pixels,
    })
}

/// Decode standard base64 (RFC 4648) into bytes. Whitespace (`\r`/`\n`) is
/// skipped, padding (`=`) ends the stream, and any other non-alphabet byte makes
/// the decode fail.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;

    for &b in s.as_bytes() {
        let value = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'\r' | b'\n' => continue,
            b'=' => break,
            _ => return None,
        };

        buf = (buf << 6) | (value as u32);
        bits += 6;

        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_header_keys() {
        let cmd = parse_command("a=T,f=32,i=7,s=2,v=1,c=2,r=1,z=-1,C=1,q=2,m=0;Zm9v");

        assert_eq!(cmd.action, Action::TransmitAndDisplay);
        assert_eq!(cmd.format, 32);
        assert_eq!((cmd.width, cmd.height), (2, 1));
        assert_eq!((cmd.cols, cmd.rows), (2, 1));
        assert_eq!(cmd.id, Some(7));
        assert!(!cmd.more);
        assert_eq!(cmd.payload, "Zm9v");
    }

    #[test]
    fn continuation_chunk_has_no_header_keys() {
        // A continuation chunk carries only `m` and the next slice of payload;
        // the absent action defaults to transmit-only (the terminal still
        // accumulates it onto the in-flight transfer).
        let cmd = parse_command("m=1;AAAA");

        assert_eq!(cmd.action, Action::Transmit);
        assert!(cmd.more);
        assert_eq!(cmd.payload, "AAAA");
    }

    #[test]
    fn parses_delete_by_id() {
        let cmd = parse_command("a=d,d=i,i=42;");

        assert_eq!(cmd.action, Action::Delete);
        assert_eq!(cmd.delete_target, Some('i'));
        assert_eq!(cmd.id, Some(42));
        assert!(cmd.payload.is_empty());
    }

    #[test]
    fn decodes_rgba() {
        // Two pixels: red then blue, fully opaque.
        let raster = decode_raster(32, 2, 1, "/wAA/wAA//8=").unwrap();

        assert_eq!((raster.width, raster.height), (2, 1));
        assert_eq!(raster.pixels[0], RGBA8::new(255, 0, 0, 255));
        assert_eq!(raster.pixels[1], RGBA8::new(0, 0, 255, 255));
    }

    #[test]
    fn decodes_rgb_as_opaque() {
        // One RGB pixel (255, 0, 0) -> opaque red. "/wAA" = 0xff,0x00,0x00.
        let raster = decode_raster(24, 1, 1, "/wAA").unwrap();

        assert_eq!((raster.width, raster.height), (1, 1));
        assert_eq!(raster.pixels[0], RGBA8::new(255, 0, 0, 255));
    }

    #[test]
    fn rejects_png_format() {
        assert!(decode_raster(100, 1, 1, "iVBORw0KGgo=").is_none());
    }

    #[test]
    fn rejects_short_payload() {
        // 2x1 RGBA needs 8 bytes; only 4 supplied.
        assert!(decode_raster(32, 2, 1, "/wAA").is_none());
    }

    #[test]
    fn rejects_zero_dimensions() {
        assert!(decode_raster(32, 0, 1, "AAAA").is_none());
        assert!(decode_raster(32, 1, 0, "AAAA").is_none());
    }

    #[test]
    fn rejects_invalid_base64() {
        assert!(decode_raster(32, 1, 1, "@@@@@@@@").is_none());
    }
}
