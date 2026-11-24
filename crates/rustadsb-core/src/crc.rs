//! Mode S CRC-24, syndrome tables and 1/2-bit error correction.
//! Port of upstream `crc.c`.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Mode S generator polynomial (x^24 + ... , top bit implied).
pub const GENERATOR_POLY: u32 = 0xfff409;
pub const SHORT_MSG_BITS: usize = 56;
pub const LONG_MSG_BITS: usize = 112;
/// Most bit errors the tables can correct.
pub const MAX_BITERRORS: usize = 2;

fn crc_table() -> &'static [u32; 256] {
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = (i as u32) << 16;
            for _ in 0..8 {
                c = if c & 0x80_0000 != 0 {
                    (c << 1) ^ GENERATOR_POLY
                } else {
                    c << 1
                };
            }
            *e = c & 0xff_ffff;
        }
        t
    })
}

/// CRC remainder over `msg` (whole bytes, at least 3). For a valid frame
/// with the parity field XORed in (DF11/17/18) the result is 0.
pub fn checksum(msg: &[u8]) -> u32 {
    let n = msg.len();
    assert!(n >= 3, "message too short for checksum");
    let table = crc_table();
    let mut rem = 0u32;
    for &b in &msg[..n - 3] {
        rem = ((rem << 8) ^ table[(b as u32 ^ ((rem & 0xff_0000) >> 16)) as usize]) & 0xff_ffff;
    }
    rem ^ ((msg[n - 3] as u32) << 16) ^ ((msg[n - 2] as u32) << 8) ^ (msg[n - 1] as u32)
}

fn single_bit_syndromes() -> &'static [u32; LONG_MSG_BITS] {
    static T: OnceLock<[u32; LONG_MSG_BITS]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; LONG_MSG_BITS];
        let mut msg = [0u8; LONG_MSG_BITS / 8];
        for (i, e) in t.iter_mut().enumerate() {
            msg[i / 8] ^= 1 << (7 - (i & 7));
            *e = checksum(&msg);
            msg[i / 8] ^= 1 << (7 - (i & 7));
        }
        t
    })
}

/// Syndrome of a single flipped bit `bit` (0 = first transmitted bit) in a 112-bit message.
pub fn bit_syndrome(bit: usize) -> u32 {
    single_bit_syndromes()[bit]
}

/// How to repair a message: which bits to flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorInfo {
    pub syndrome: u32,
    pub errors: u8,
    /// Bit positions to flip, valid for the first `errors` entries.
    pub bit: [i8; MAX_BITERRORS],
}

impl ErrorInfo {
    pub const NO_ERRORS: ErrorInfo = ErrorInfo {
        syndrome: 0,
        errors: 0,
        bit: [-1; MAX_BITERRORS],
    };

    /// Flip the identified bits in `msg`.
    pub fn apply(&self, msg: &mut [u8]) {
        for &b in &self.bit[..self.errors as usize] {
            let b = b as usize;
            msg[b >> 3] ^= 1 << (7 - (b & 7));
        }
    }
}

type Table = HashMap<u32, ErrorInfo>;

/// Visit every combination of `1..=max` distinct bits in `[start, bits)` with
/// the running syndrome. `f(syndrome, bits_so_far)`.
fn for_each_combo(
    bits: usize,
    offset: usize,
    start: usize,
    max: usize,
    syndrome: u32,
    chosen: &mut Vec<usize>,
    f: &mut impl FnMut(u32, &[usize]),
) {
    if chosen.len() >= max {
        return;
    }
    for i in start..bits {
        let s = syndrome ^ bit_syndrome(i + offset);
        chosen.push(i);
        f(s, chosen);
        for_each_combo(bits, offset, i + 1, max, s, chosen, f);
        chosen.pop();
    }
}

/// Build the correction table for messages of `bits` bits. Syndromes reachable by
/// more than one error pattern of up to `max_correct` bits are dropped, and so are
/// those that collide with a `max_correct+1..=max_detect` bit pattern.
fn prepare_table(bits: usize, max_correct: usize, max_detect: usize) -> Table {
    assert!(bits <= LONG_MSG_BITS && max_correct <= MAX_BITERRORS && max_detect >= max_correct);
    if max_correct == 0 {
        return Table::new();
    }
    let offset = LONG_MSG_BITS - bits;
    // The first 5 bits (the DF field) are never corrected.
    let mut seen: HashMap<u32, (ErrorInfo, u32)> = HashMap::new();
    for_each_combo(
        bits,
        offset,
        5,
        max_correct,
        0,
        &mut Vec::new(),
        &mut |s, c| {
            let mut ei = ErrorInfo {
                syndrome: s,
                errors: c.len() as u8,
                bit: [-1; MAX_BITERRORS],
            };
            for (k, &b) in c.iter().enumerate() {
                ei.bit[k] = b as i8;
            }
            seen.entry(s).and_modify(|e| e.1 += 1).or_insert((ei, 1));
        },
    );
    let mut table: Table = seen
        .into_iter()
        .filter(|(_, (_, n))| *n == 1)
        .map(|(s, (ei, _))| (s, ei))
        .collect();

    if max_detect > max_correct {
        let mut flagged: HashSet<u32> = HashSet::new();
        for_each_combo(
            bits,
            offset,
            5,
            max_detect,
            0,
            &mut Vec::new(),
            &mut |s, c| {
                if c.len() > max_correct && table.contains_key(&s) {
                    flagged.insert(s);
                }
            },
        );
        for s in flagged {
            table.remove(&s);
        }
    }
    table
}

/// Error-correction tables for short (56-bit) and long (112-bit) messages.
#[derive(Debug, Clone, Default)]
pub struct Fixer {
    short: Table,
    long: Table,
}

impl Fixer {
    /// `fix_bits`: 0 = no correction, 1 = single bit, 2 or more = up to two bits
    /// (detecting out to four, like upstream).
    pub fn new(fix_bits: u8) -> Self {
        match fix_bits {
            0 => Self::default(),
            1 => Self {
                short: prepare_table(SHORT_MSG_BITS, 1, 1),
                long: prepare_table(LONG_MSG_BITS, 1, 1),
            },
            _ => Self {
                short: prepare_table(SHORT_MSG_BITS, 2, 4),
                long: prepare_table(LONG_MSG_BITS, 2, 4),
            },
        }
    }

    /// Look up how to repair a message with this syndrome. `None` if it cannot be
    /// corrected (or correction is disabled). Syndrome 0 means no errors.
    pub fn diagnose(&self, syndrome: u32, bitlen: usize) -> Option<ErrorInfo> {
        if syndrome == 0 {
            return Some(ErrorInfo::NO_ERRORS);
        }
        assert!(bitlen == SHORT_MSG_BITS || bitlen == LONG_MSG_BITS);
        let t = if bitlen == SHORT_MSG_BITS {
            &self.short
        } else {
            &self.long
        };
        t.get(&syndrome).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.short.is_empty() && self.long.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    // Well-known DF17 frames (ADS-B identification / position from the Mode S book).
    const GOOD: [&str; 3] = [
        "8D4840D6202CC371C32CE0576098",
        "8D40621D58C382D690C8AC2863A7",
        "8D40621D58C386435CC412692AD6",
    ];

    #[test]
    fn valid_frames_have_zero_syndrome() {
        for g in GOOD {
            assert_eq!(checksum(&hex(g)), 0, "{g}");
        }
    }

    #[test]
    fn single_bit_syndrome_matches_flip() {
        let mut m = hex(GOOD[0]);
        m[3] ^= 0x10; // bit index 27
        assert_eq!(checksum(&m), bit_syndrome(27));
    }

    fn flip(m: &mut [u8], bit: usize) {
        m[bit >> 3] ^= 1 << (7 - (bit & 7));
    }

    #[test]
    fn corrects_every_tabulated_error() {
        let fixer = Fixer::new(2);
        let table = prepare_table(112, 2, 4);
        for g in GOOD {
            let good = hex(g);
            for ei in table.values() {
                let mut m = good.clone();
                for &b in &ei.bit[..ei.errors as usize] {
                    flip(&mut m, b as usize);
                }
                let found = fixer.diagnose(checksum(&m), 112).expect("tabulated error");
                found.apply(&mut m);
                assert_eq!(m, good);
            }
        }
    }

    #[test]
    fn every_single_bit_error_is_corrected() {
        let fixer = Fixer::new(1);
        let good = hex(GOOD[1]);
        for bit in 5..112 {
            let mut m = good.clone();
            flip(&mut m, bit);
            let ei = fixer.diagnose(checksum(&m), 112).unwrap();
            assert_eq!(ei.errors, 1);
            ei.apply(&mut m);
            assert_eq!(m, good);
        }
    }

    #[test]
    fn short_messages_use_the_short_table() {
        // DF11 all-call, 56 bits: check a flipped bit is repaired using the 56-bit table.
        let mut good = hex("5D4840D6000000");
        let pi = checksum(&good); // PI field = remainder over the zero tail
        good[4] = (pi >> 16) as u8;
        good[5] = (pi >> 8) as u8;
        good[6] = pi as u8;
        assert_eq!(checksum(&good), 0);
        let mut m = good.clone();
        flip(&mut m, 20);
        let ei = Fixer::new(1).diagnose(checksum(&m), 56).unwrap();
        ei.apply(&mut m);
        assert_eq!(m, good);
    }

    #[test]
    fn no_correction_when_disabled_or_df_bits_hit() {
        let mut m = hex(GOOD[0]);
        m[0] ^= 0x80; // bit 0 is in the DF field, which is never corrected
        assert!(Fixer::new(2).diagnose(checksum(&m), 112).is_none());
        let mut m = hex(GOOD[0]);
        m[6] ^= 1;
        assert!(Fixer::new(0).diagnose(checksum(&m), 112).is_none());
        assert_eq!(Fixer::new(0).diagnose(0, 112), Some(ErrorInfo::NO_ERRORS));
    }

    #[test]
    fn two_bit_table_has_partial_coverage() {
        // Upstream notes ~65% 2-bit coverage when detecting out to 4 bits.
        let t = prepare_table(112, 2, 4);
        let ones = t.values().filter(|e| e.errors == 1).count();
        let twos = t.values().filter(|e| e.errors == 2).count();
        assert_eq!(ones, 107);
        let possible = 107 * 106 / 2;
        let pct = twos * 100 / possible;
        assert!((55..=75).contains(&pct), "2-bit coverage {pct}%");
    }
}
