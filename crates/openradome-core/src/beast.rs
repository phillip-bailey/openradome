//! Parsers for the network input formats: Beast binary and raw hex lines.
//! Port of the relevant parts of upstream `net_io.c` (`decodeBinMessage`, `processHexMessage`).

use crate::mode_s::{Decoder, ModesMessage};

/// Timestamp mlat-client uses to mark MLAT results in the Beast stream.
pub const MAGIC_MLAT_TIMESTAMP: i64 = 0xFF00_4D4C_4154;
const ESC: u8 = 0x1a;

/// One frame of a Beast stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeastFrame {
    /// Mode A/C (2 bytes), short (7) or long (14) message with its 12 MHz timestamp and signal byte.
    Message {
        timestamp: i64,
        signal: u8,
        data: Vec<u8>,
    },
    /// `0x1a 0xe3`: the id of the receiver that follows messages are from.
    ReceiverId(u64),
}

/// Incremental Beast stream parser. Feed arbitrary chunks, get complete frames.
#[derive(Debug, Default)]
pub struct BeastParser {
    buf: Vec<u8>,
    /// Bytes skipped because they were not part of a valid frame.
    pub garbage: u64,
}

impl BeastParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<BeastFrame> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut pos = 0;
        let buf = std::mem::take(&mut self.buf);
        loop {
            // find the next escape
            let Some(rel) = buf[pos..].iter().position(|&b| b == ESC) else {
                self.garbage += (buf.len() - pos) as u64;
                pos = buf.len();
                break;
            };
            self.garbage += rel as u64;
            pos += rel;
            let Some(&kind) = buf.get(pos + 1) else { break };
            let body_len = match kind {
                b'1' => 6 + 1 + 2,
                b'2' => 6 + 1 + 7,
                b'3' => 6 + 1 + 14,
                0xe3 => 8,
                _ => {
                    // not a frame we know: skip this escape (a doubled escape is data of
                    // another frame we missed the start of)
                    self.garbage += 1;
                    pos += 1;
                    continue;
                }
            };
            // read body_len bytes, removing escape doubling
            let mut body = Vec::with_capacity(body_len);
            let mut p = pos + 2;
            let mut bad = false;
            let mut incomplete = false;
            while body.len() < body_len {
                let Some(&b) = buf.get(p) else {
                    incomplete = true;
                    break;
                };
                if b == ESC {
                    match buf.get(p + 1) {
                        None => {
                            incomplete = true;
                            break;
                        }
                        Some(&ESC) => {
                            body.push(ESC);
                            p += 2;
                        }
                        Some(_) => {
                            bad = true; // unescaped escape: resynchronise at it
                            break;
                        }
                    }
                } else {
                    body.push(b);
                    p += 1;
                }
            }
            if incomplete {
                break;
            }
            if bad {
                self.garbage += (p - pos) as u64;
                pos = p;
                continue;
            }
            if kind == 0xe3 {
                out.push(BeastFrame::ReceiverId(
                    body.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64),
                ));
            } else {
                let timestamp = body[..6].iter().fold(0i64, |acc, &b| (acc << 8) | b as i64);
                out.push(BeastFrame::Message {
                    timestamp,
                    signal: body[6],
                    data: body[7..].to_vec(),
                });
            }
            pos = p;
        }
        self.buf = buf[pos..].to_vec();
        out
    }
}

/// A message read from a raw (`*hex;` / `@timestamp hex;`) line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLine {
    pub timestamp: Option<i64>,
    pub data: Vec<u8>,
}

/// Parse one line of the raw format. Returns `None` for blank, comment or malformed lines.
pub fn parse_raw_line(line: &str) -> Option<RawLine> {
    let line = line.trim();
    // hex digits only below: slicing by byte position is then safe, and `from_str_radix` can't
    // take a sign
    if !line.is_ascii() {
        return None;
    }
    let (timestamp, rest) = match line.chars().next()? {
        '*' => (None, &line[1..]),
        '@' if line.len() > 13 && line[1..13].bytes().all(|b| b.is_ascii_hexdigit()) => (
            Some(i64::from_str_radix(&line[1..13], 16).ok()?),
            &line[13..],
        ),
        _ => return None,
    };
    let hex = rest.strip_suffix(';')?;
    if !matches!(hex.len(), 4 | 14 | 28) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let data = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some(RawLine { timestamp, data })
}

/// Build and decode a message from a Beast frame or raw line.
///
/// `now` is the receive time in ms. Mode A/C frames are returned only if `want_mode_ac`.
/// The result has `remote = true`. Check `decode_result` before using it.
pub fn message_from_wire(
    decoder: &mut Decoder,
    data: &[u8],
    timestamp: i64,
    signal: Option<u8>,
    now: i64,
    receiver_id: u64,
    want_mode_ac: bool,
) -> Option<ModesMessage> {
    let mut mm = match data.len() {
        2 => {
            if !want_mode_ac {
                return None;
            }
            ModesMessage::from_mode_a(((data[0] as u32) << 8) | data[1] as u32)
        }
        7 | 14 => {
            let mut mm = ModesMessage::from_bytes(data);
            decoder.decode(&mut mm);
            mm
        }
        _ => return None,
    };
    mm.remote = true;
    mm.timestamp = timestamp;
    mm.sys_timestamp = now;
    mm.receiver_id = receiver_id;
    if let Some(s) = signal {
        let level = s as f64 / 255.0;
        mm.signal_level = level * level;
    }
    // MLAT results are marked by a magic timestamp and override every other source.
    if timestamp == MAGIC_MLAT_TIMESTAMP && mm.decode_result >= 0 {
        mm.source = crate::mode_s::DataSource::Mlat;
        mm.addrtype = crate::mode_s::AddrType::Mlat;
    }
    Some(mm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::beast_frame;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn sample() -> ModesMessage {
        let mut d = Decoder::new(1);
        let mut mm = ModesMessage::from_bytes(&hex("8D4840D6202CC371C32CE0576098"));
        d.decode(&mut mm);
        mm.timestamp = 0x0102_0304_0506;
        mm.signal_level = 0.25;
        mm
    }

    #[test]
    fn round_trips_with_the_encoder_in_any_chunking() {
        let mm = sample();
        let frame = beast_frame(&mm, false).unwrap();
        let mut stream = frame.clone();
        stream.extend(&frame);
        for chunk in [1usize, 3, 5, stream.len()] {
            let mut p = BeastParser::new();
            let mut got = Vec::new();
            for c in stream.chunks(chunk) {
                got.extend(p.feed(c));
            }
            assert_eq!(got.len(), 2, "chunk {chunk}");
            let BeastFrame::Message {
                timestamp,
                signal,
                data,
            } = &got[0]
            else {
                panic!()
            };
            assert_eq!(*timestamp, 0x0102_0304_0506);
            assert_eq!(*signal, 128);
            assert_eq!(&data[..], &mm.msg[..14]);
            assert_eq!(p.garbage, 0);
        }
    }

    #[test]
    fn handles_escaped_bytes_and_garbage() {
        let mut mm = sample();
        mm.timestamp = 0x1a1a_1a1a_1a1a; // every timestamp byte needs escaping
        let frame = beast_frame(&mm, false).unwrap();
        let mut stream = vec![0x00, 0x42, 0x1a, 0x99]; // junk and a bad type
        stream.extend(&frame);
        let mut p = BeastParser::new();
        let got = p.feed(&stream);
        assert_eq!(got.len(), 1);
        let BeastFrame::Message { timestamp, .. } = &got[0] else {
            panic!()
        };
        assert_eq!(*timestamp, 0x1a1a_1a1a_1a1a);
        assert!(p.garbage >= 4);
    }

    #[test]
    fn recovers_from_a_truncated_frame() {
        let mm = sample();
        let frame = beast_frame(&mm, false).unwrap();
        let mut stream = frame[..10].to_vec(); // cut off
        stream.extend(&frame);
        let got = BeastParser::new().feed(&stream);
        assert_eq!(got.len(), 1, "the intact frame still decodes");
    }

    #[test]
    fn mode_ac_and_receiver_id_frames() {
        let ac = ModesMessage::from_mode_a(0x1200);
        let mut stream = vec![0x1a, 0xe3, 0, 0, 0, 0, 0, 0, 0x12, 0x34];
        stream.extend(beast_frame(&ac, false).unwrap());
        let got = BeastParser::new().feed(&stream);
        assert_eq!(got[0], BeastFrame::ReceiverId(0x1234));
        let BeastFrame::Message { data, .. } = &got[1] else {
            panic!()
        };
        assert_eq!(data.len(), 2);
    }

    #[test]
    fn raw_lines() {
        let l = parse_raw_line("*8d4840d6202cc371c32ce0576098;\n").unwrap();
        assert_eq!(l.timestamp, None);
        assert_eq!(l.data.len(), 14);
        let l = parse_raw_line("@0123456789AB5D4840D6000000;").unwrap_or_else(|| panic!());
        assert_eq!(l.timestamp, Some(0x0123_4567_89AB));
        assert_eq!(l.data.len(), 7);
        assert!(parse_raw_line("*8D4840;").is_none(), "bad length");
        assert!(parse_raw_line("*zz4840d6202cc371c32ce0576098;").is_none());
        assert!(parse_raw_line("hello").is_none());
        assert!(parse_raw_line("").is_none());
    }

    #[test]
    fn raw_lines_that_are_not_ascii_or_hex_are_refused_without_a_panic() {
        // each of these used to slice through a multi-byte character, or parse a sign
        assert!(parse_raw_line("*0\u{FFFD};").is_none());
        assert!(parse_raw_line("*\u{e9}840d6202cc371c32ce0576098;").is_none());
        assert!(parse_raw_line("@0123456789A\u{FFFD}5D4840D6000000;").is_none());
        assert!(parse_raw_line("@+123456789AB5D4840D6000000;").is_none());
        assert!(parse_raw_line("*+d4840d6202cc371c32ce0576098;").is_none());
        assert!(parse_raw_line("*+d48;").is_none());
    }

    #[test]
    fn wire_messages_decode_and_mlat_is_marked() {
        let mut d = Decoder::new(1);
        let data = hex("8D4840D6202CC371C32CE0576098");
        let mm =
            message_from_wire(&mut d, &data, 1234, Some(255), 1_700_000_000_000, 7, false).unwrap();
        assert_eq!(mm.decode_result, 0);
        assert!(mm.remote);
        assert_eq!(mm.signal_level, 1.0);
        assert_eq!(mm.sys_timestamp, 1_700_000_000_000);
        assert_eq!(mm.receiver_id, 7);
        let mm = message_from_wire(&mut d, &data, MAGIC_MLAT_TIMESTAMP, None, 1, 0, false).unwrap();
        assert_eq!(mm.source, crate::mode_s::DataSource::Mlat);
        assert_eq!(mm.addrtype, crate::mode_s::AddrType::Mlat);
        assert!(message_from_wire(&mut d, &[0x12, 0x00], 0, None, 1, 0, false).is_none());
        assert!(message_from_wire(&mut d, &[0x12, 0x00], 0, None, 1, 0, true).is_some());
        assert!(message_from_wire(&mut d, &[1, 2, 3], 0, None, 1, 0, true).is_none());
    }
}
