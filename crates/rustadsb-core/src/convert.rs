//! IQ sample to magnitude conversion. Port of the integer/float "no DC filter"
//! paths of upstream `convert.c` (UC8, SC16, SC16Q11).

use std::sync::OnceLock;

/// Raw IQ sample format of an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputFormat {
    /// Unsigned 8-bit I/Q pairs (RTL-SDR native).
    #[default]
    Uc8,
    /// Signed 16-bit little-endian I/Q, full scale.
    Sc16,
    /// Signed 16-bit little-endian I/Q, 12-bit (Q11) scale.
    Sc16Q11,
}

impl InputFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            InputFormat::Uc8 => 2,
            InputFormat::Sc16 | InputFormat::Sc16Q11 => 4,
        }
    }

    /// Parse the `--iformat` argument (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "uc8" => Some(Self::Uc8),
            "sc16" => Some(Self::Sc16),
            "sc16q11" => Some(Self::Sc16Q11),
            _ => None,
        }
    }
}

/// Mean level and power of a converted block, both normalised to 0..1.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BlockStats {
    pub mean_level: f64,
    pub mean_power: f64,
}

/// Bits of I and Q used by the SC16Q11 lookup table (upstream `SC16Q11_TABLE_BITS`).
const SC16Q11_TABLE_BITS: u32 = 8;
const SC16Q11_LOSE_BITS: u32 = 11 - SC16Q11_TABLE_BITS;

/// Upstream builds with `SC16Q11_TABLE_BITS=8` on ARM and aarch64 (the Pi and Apple
/// silicon) and uses the float path elsewhere. The table drops the 3 low bits of |I| and
/// |Q|, so the two paths differ slightly; follow upstream's choice per architecture so
/// results match what upstream gives on the same machine.
pub const SC16Q11_USES_TABLE: bool = cfg!(any(target_arch = "arm", target_arch = "aarch64"));

fn sc16q11_lookup() -> &'static [u16] {
    static T: OnceLock<Vec<u16>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = vec![0u16; 1 << (SC16Q11_TABLE_BITS * 2)];
        for i in (0..2048u32).step_by(1 << SC16Q11_LOSE_BITS) {
            for q in (0..2048u32).step_by(1 << SC16Q11_LOSE_BITS) {
                let fi = i as f32 / 2048.0;
                let fq = q as f32 / 2048.0;
                let magsq = (fi * fi + fq * fq).min(1.0);
                let index =
                    ((i >> SC16Q11_LOSE_BITS) << SC16Q11_TABLE_BITS) | (q >> SC16Q11_LOSE_BITS);
                t[index as usize] = (magsq.sqrt() * 65535.0 + 0.5) as u16;
            }
        }
        t
    })
}

fn uc8_lookup() -> &'static [u16] {
    static T: OnceLock<Vec<u16>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = vec![0u16; 256 * 256];
        for i in 0..256usize {
            for q in 0..256usize {
                let fi = (i as f32 - 127.5) / 127.5;
                let fq = (q as f32 - 127.5) / 127.5;
                let magsq = (fi * fi + fq * fq).min(1.0);
                // Indexed by the byte pair read as a little-endian u16 (I low, Q high).
                t[i + 256 * q] = (magsq.sqrt() * 65535.0 + 0.5) as u16;
            }
        }
        t
    })
}

/// Convert `iq` (raw bytes in `format`) to magnitudes in `out`. Converts as many
/// whole samples as both slices hold and returns how many, plus block statistics.
pub fn convert(format: InputFormat, iq: &[u8], out: &mut [u16]) -> (usize, BlockStats) {
    let n = (iq.len() / format.bytes_per_sample()).min(out.len());
    if n == 0 {
        return (0, BlockStats::default());
    }
    let stats = match format {
        InputFormat::Uc8 => {
            let table: &[u16; 65536] = uc8_lookup().try_into().expect("65536 entries");
            let (mut sum_level, mut sum_power) = (0u64, 0u64);
            for (o, pair) in out[..n].iter_mut().zip(iq.as_chunks::<2>().0) {
                // the byte pair read as a little-endian u16 is the table index, so no
                // bounds check is needed
                let mag = table[u16::from_le_bytes(*pair) as usize];
                *o = mag;
                sum_level += u64::from(mag);
                // 65535 squared fits in 32 bits
                sum_power += u64::from(u32::from(mag) * u32::from(mag));
            }
            BlockStats {
                mean_level: sum_level as f64 / 65536.0 / n as f64,
                mean_power: sum_power as f64 / 65535.0 / 65535.0 / n as f64,
            }
        }
        InputFormat::Sc16Q11 if SC16Q11_USES_TABLE => {
            let table = sc16q11_lookup();
            let (mut sum_level, mut sum_power) = (0u64, 0u64);
            for (o, s) in out[..n].iter_mut().zip(iq.as_chunks::<4>().0) {
                let i = (i16::from_le_bytes([s[0], s[1]]) as i32).unsigned_abs() & 2047;
                let q = (i16::from_le_bytes([s[2], s[3]]) as i32).unsigned_abs() & 2047;
                let mag = table[(((i >> SC16Q11_LOSE_BITS) << SC16Q11_TABLE_BITS)
                    | (q >> SC16Q11_LOSE_BITS)) as usize];
                *o = mag;
                sum_level += mag as u64;
                sum_power += mag as u64 * mag as u64;
            }
            BlockStats {
                mean_level: sum_level as f64 / 65536.0 / n as f64,
                mean_power: sum_power as f64 / 65535.0 / 65535.0 / n as f64,
            }
        }
        InputFormat::Sc16 | InputFormat::Sc16Q11 => {
            let scale = if format == InputFormat::Sc16 {
                32768.0f32
            } else {
                2048.0
            };
            let (mut sum_level, mut sum_power) = (0f32, 0f32);
            for (o, s) in out[..n].iter_mut().zip(iq.as_chunks::<4>().0) {
                let i = i16::from_le_bytes([s[0], s[1]]) as f32 / scale;
                let q = i16::from_le_bytes([s[2], s[3]]) as f32 / scale;
                let magsq = (i * i + q * q).min(1.0);
                let mag = magsq.sqrt();
                sum_power += magsq;
                sum_level += mag;
                *o = (mag * 65535.0 + 0.5) as u16;
            }
            BlockStats {
                mean_level: (sum_level / n as f32) as f64,
                mean_power: (sum_power / n as f32) as f64,
            }
        }
    };
    (n, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uc8_known_points() {
        let mut out = [0u16; 3];
        // centre (127/128 is ~zero), full-scale I, and a diagonal that clips to 1.0
        let (n, _) = convert(InputFormat::Uc8, &[128, 127, 255, 128, 255, 255], &mut out);
        assert_eq!(n, 3);
        assert!(out[0] < 500, "near-zero magnitude, got {}", out[0]);
        assert!((out[1] as i32 - 65535).abs() <= 2);
        assert_eq!(out[2], 65535);
    }

    #[test]
    fn uc8_is_symmetric_in_i_and_q() {
        let mut a = [0u16; 1];
        let mut b = [0u16; 1];
        convert(InputFormat::Uc8, &[200, 90], &mut a);
        convert(InputFormat::Uc8, &[90, 200], &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn sc16_scales() {
        let mut out = [0u16; 2];
        let iq = [0x00, 0x40, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00]; // I=16384 ; I=1024
        convert(InputFormat::Sc16, &iq[..4], &mut out[..1]);
        assert_eq!(out[0], 32768); // 0.5 full scale
        convert(InputFormat::Sc16Q11, &iq[4..], &mut out[..1]);
        assert_eq!(out[0], 32768); // 1024/2048 (a multiple of 8, so the table is exact)
    }

    #[test]
    fn sc16q11_table_drops_three_low_bits() {
        if !SC16Q11_USES_TABLE {
            return;
        }
        let mut a = [0u16; 1];
        let mut b = [0u16; 1];
        // 1024 and 1031 share the same table cell; 1032 does not. Negative values mirror.
        convert(InputFormat::Sc16Q11, &[0x00, 0x04, 0, 0], &mut a);
        convert(InputFormat::Sc16Q11, &[0x07, 0x04, 0, 0], &mut b);
        assert_eq!(a, b);
        convert(InputFormat::Sc16Q11, &[0x08, 0x04, 0, 0], &mut b);
        assert_ne!(a, b);
        convert(InputFormat::Sc16Q11, &[0x00, 0xFC, 0, 0], &mut b); // -1024
        assert_eq!(a, b);
        // -32768 has |x| & 2047 == 0
        convert(InputFormat::Sc16Q11, &[0x00, 0x80, 0, 0], &mut b);
        assert_eq!(b[0], 0);
    }

    #[test]
    fn stats_and_partial_blocks() {
        let mut out = [0u16; 4];
        let (n, st) = convert(InputFormat::Uc8, &[255, 255, 255, 255, 1], &mut out);
        assert_eq!(n, 2, "trailing odd byte ignored");
        assert!((st.mean_level - 65535.0 / 65536.0).abs() < 1e-9);
        let (n, _) = convert(InputFormat::Uc8, &[], &mut out);
        assert_eq!(n, 0);
        assert_eq!(InputFormat::parse("SC16Q11"), Some(InputFormat::Sc16Q11));
        assert_eq!(InputFormat::parse("bogus"), None);
    }
}
