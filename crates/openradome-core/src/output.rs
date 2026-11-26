//! Wire-format encoders for decoded messages. Port of the raw and Beast parts of
//! upstream `net_io.c` (`modesSendRawOutput`, `modesSendBeastOutput`).

use crate::mode_s::{DFTYPE_MODEAC, ModesMessage};

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Beast frame escape character.
pub const BEAST_ESCAPE: u8 = 0x1a;

fn push_hex(out: &mut String, bytes: &[u8]) {
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
}

/// The message bytes to send: the original on-air bytes with `verbatim`, else as corrected.
fn payload(mm: &ModesMessage, verbatim: bool) -> &[u8] {
    let src = if verbatim { &mm.verbatim } else { &mm.msg };
    &src[..mm.msgbits / 8]
}

/// Raw output line: `*8D4840D6...;\n`, or `@<12 hex digit timestamp>8D48...;\n` when
/// `with_timestamp` (upstream's MLAT mode) and the message has a timestamp.
pub fn raw_line(mm: &ModesMessage, with_timestamp: bool, verbatim: bool) -> String {
    let bytes = payload(mm, verbatim);
    let mut s = String::with_capacity(bytes.len() * 2 + 15);
    if with_timestamp && mm.timestamp != 0 {
        s.push_str(&format!("@{:012X}", mm.timestamp));
    } else {
        s.push('*');
    }
    push_hex(&mut s, bytes);
    s.push_str(";\n");
    s
}

/// Beast binary frame: `0x1a`, type (`'1'` Mode A/C, `'2'` short, `'3'` long), 6 byte
/// big-endian 12 MHz timestamp, signal byte, message. `0x1a` in the body is doubled.
/// Returns `None` for a message with an unsupported length.
pub fn beast_frame(mm: &ModesMessage, verbatim: bool) -> Option<Vec<u8>> {
    let bytes = payload(mm, verbatim);
    let kind = match bytes.len() {
        7 => b'2',
        14 => b'3',
        2 => b'1',
        _ => return None,
    };
    let mut raw = Vec::with_capacity(7 + bytes.len());
    let ts = mm.timestamp as u64;
    for i in (0..6).rev() {
        raw.push((ts >> (8 * i)) as u8);
    }
    let mut sig = (mm.signal_level.sqrt() * 255.0).round_ties_even() as i64;
    if mm.signal_level > 0.0 && sig < 1 {
        sig = 1;
    }
    raw.push(sig.clamp(0, 255) as u8);
    raw.extend_from_slice(bytes);

    let mut out = Vec::with_capacity(2 + raw.len() * 2);
    out.push(BEAST_ESCAPE);
    out.push(kind);
    for b in raw {
        out.push(b);
        if b == BEAST_ESCAPE {
            out.push(b);
        }
    }
    Some(out)
}

/// Is this a Mode A/C pseudo-message?
pub fn is_mode_ac(mm: &ModesMessage) -> bool {
    mm.msgtype == DFTYPE_MODEAC
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode_s::Decoder;

    fn msg() -> ModesMessage {
        let mut d = Decoder::new(1);
        let mut mm = ModesMessage::from_bytes(&[
            0x8D, 0x48, 0x40, 0xD6, 0x20, 0x2C, 0xC3, 0x71, 0xC3, 0x2C, 0xE0, 0x57, 0x60, 0x98,
        ]);
        d.decode(&mut mm);
        mm
    }

    #[test]
    fn raw_formats() {
        let mut mm = msg();
        assert_eq!(
            raw_line(&mm, false, false),
            "*8D4840D6202CC371C32CE0576098;\n"
        );
        mm.timestamp = 0x1234_5678_9ABC;
        assert_eq!(
            raw_line(&mm, true, false),
            "@123456789ABC8D4840D6202CC371C32CE0576098;\n"
        );
        mm.timestamp = 0;
        assert!(raw_line(&mm, true, false).starts_with('*'));
    }

    #[test]
    fn beast_long_frame() {
        let mut mm = msg();
        mm.timestamp = 0x0102_0304_0506;
        mm.signal_level = 0.25; // sqrt = 0.5 -> 127.5 -> rounds to even 128
        let f = beast_frame(&mm, false).unwrap();
        assert_eq!(&f[..9], &[0x1a, b'3', 1, 2, 3, 4, 5, 6, 128]);
        assert_eq!(&f[9..], &mm.msg[..14]);
    }

    #[test]
    fn beast_escapes_0x1a_and_clamps_signal() {
        let mut mm = msg();
        mm.timestamp = 0x1a00_0000_001a;
        mm.signal_level = 1e-9; // tiny but non zero => 1
        let f = beast_frame(&mm, false).unwrap();
        assert_eq!(&f[..4], &[0x1a, b'3', 0x1a, 0x1a]);
        assert_eq!(f[f.len() - 14 - 1], 1);
        assert_eq!(f.iter().filter(|&&b| b == 0x1a).count(), 1 + 2 * 2);
        mm.timestamp = 1;
        mm.signal_level = 4.0;
        assert_eq!(beast_frame(&mm, false).unwrap()[8], 255);
    }

    #[test]
    fn beast_short_and_mode_ac() {
        let mm = ModesMessage::from_mode_a(0x1200);
        assert!(is_mode_ac(&mm));
        let f = beast_frame(&mm, false).unwrap();
        assert_eq!(f[1], b'1');
        let mut short = ModesMessage::from_bytes(&[0x5D, 0x48, 0x40, 0xD6, 0, 0, 0]);
        short.msgbits = 56;
        assert_eq!(beast_frame(&short, false).unwrap()[1], b'2');
        short.msgbits = 24;
        assert!(beast_frame(&short, false).is_none());
    }
}
