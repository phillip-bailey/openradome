//! 2.4 MHz Mode S and Mode A/C demodulator. Port of upstream `demod_2400.c`.
//!
//! At 2.4 MHz there are exactly 6 samples per 5 symbols (each symbol is 500 ns, each
//! sample 416.7 ns). A phase offset in units of 1/5 sample (1/6 symbol, 83.3 ns) is
//! tracked while slicing; each symbol advances it by 6.
//!
//! Input is a block of magnitude samples (`u16`, full scale 65535). The block must be
//! followed by at least [`TRAILING_SAMPLES`] samples of look-ahead so a message that
//! starts near the end can be read to its last bit. Message starts are only searched
//! in the first `length` samples; the look-ahead is re-scanned at the start of the next
//! block.

use crate::bits::getbits;
use crate::mode_s::{Decoder, LONG_MSG_BYTES, ModesMessage, SHORT_MSG_BYTES, msg_len_by_type};

pub const SAMPLE_RATE: f64 = 2_400_000.0;
pub const PREAMBLE_US: usize = 8;
/// Extra samples after a block needed to demodulate a message that starts at its end.
pub const TRAILING_SAMPLES: usize = ((PREAMBLE_US + 112 + 16) as f64 * 1e-6 * SAMPLE_RATE) as usize;
/// Samples in the preamble.
pub const PREAMBLE_SAMPLES: usize = PREAMBLE_US * 2;

pub const PREAMBLE_THRESHOLD_MIN: u32 = 40;
pub const PREAMBLE_THRESHOLD_HOT: u32 = 42;
pub const PREAMBLE_THRESHOLD_DEFAULT: u32 = 58;
pub const PREAMBLE_THRESHOLD_PIZERO: u32 = 75;
pub const PREAMBLE_THRESHOLD_MAX: u32 = 400;

// Correlation of a 1-0 symbol pair starting at `m[0]`, for each of the five phase
// offsets. Positive means a 1 bit, negative a 0 bit. Coefficients are upstream's
// hand-tuned values ("20 units per sample, 24 per symbol"). `PHASE` is a constant so the
// match disappears when a byte's eight bits are unrolled.
#[inline(always)]
fn slice_phase<const PHASE: u8>(m: &[u16; 4]) -> i32 {
    let (a, b, c) = (m[0] as i32, m[1] as i32, m[2] as i32);
    match PHASE {
        0 => 18 * a - 15 * b - 3 * c,
        1 => 14 * a - 5 * b - 9 * c,
        2 => 16 * a + 5 * b - 20 * c, // slightly DC unbalanced but better results
        3 => 7 * a + 11 * b - 18 * c,
        _ => 4 * a + 15 * b - 20 * c + m[3] as i32,
    }
}

/// For each starting phase: the (slicer phase, sample offset) of each of the 8 bits.
const BYTE_PLAN: [[(u8, usize); 8]; 5] = [
    [
        (0, 0),
        (2, 2),
        (4, 4),
        (1, 7),
        (3, 9),
        (0, 12),
        (2, 14),
        (4, 16),
    ],
    [
        (1, 0),
        (3, 2),
        (0, 5),
        (2, 7),
        (4, 9),
        (1, 12),
        (3, 14),
        (0, 17),
    ],
    [
        (2, 0),
        (4, 2),
        (1, 5),
        (3, 7),
        (0, 10),
        (2, 12),
        (4, 14),
        (1, 17),
    ],
    [
        (3, 0),
        (0, 3),
        (2, 5),
        (4, 7),
        (1, 10),
        (3, 12),
        (0, 15),
        (2, 17),
    ],
    [
        (4, 0),
        (1, 3),
        (3, 5),
        (0, 8),
        (2, 10),
        (4, 12),
        (1, 15),
        (3, 17),
    ],
];
/// Samples consumed by one byte for each starting phase (the phase then advances by one).
const BYTE_ADVANCE: [usize; 5] = [19, 19, 19, 19, 20];

/// Slice one byte for starting phase `START`, beginning at `pos`.
#[inline(always)]
fn slice_byte_at<const START: usize>(data: &[u16], pos: usize) -> u8 {
    let mut byte = 0u8;
    // Unrolled at compile time: the slicer phase and offset of every bit are constants.
    macro_rules! bit {
        ($i:expr) => {{
            let (ph, off) = BYTE_PLAN[START][$i];
            let w = data[pos + off..]
                .first_chunk::<4>()
                .expect("trailing samples");
            let v = match ph {
                0 => slice_phase::<0>(w),
                1 => slice_phase::<1>(w),
                2 => slice_phase::<2>(w),
                3 => slice_phase::<3>(w),
                _ => slice_phase::<4>(w),
            };
            if v > 0 {
                byte |= 0x80 >> $i;
            }
        }};
    }
    bit!(0);
    bit!(1);
    bit!(2);
    bit!(3);
    bit!(4);
    bit!(5);
    bit!(6);
    bit!(7);
    byte
}

/// Slice one byte starting at `*pos` with phase `*phase`; advance both.
#[inline(always)]
fn slice_byte(data: &[u16], pos: &mut usize, phase: &mut usize) -> u8 {
    let byte = match *phase {
        0 => slice_byte_at::<0>(data, *pos),
        1 => slice_byte_at::<1>(data, *pos),
        2 => slice_byte_at::<2>(data, *pos),
        3 => slice_byte_at::<3>(data, *pos),
        _ => slice_byte_at::<4>(data, *pos),
    };
    *pos += BYTE_ADVANCE[*phase];
    *phase = (*phase + 1) % 5;
    byte
}

/// All DF values within `damage_bits` bit flips of `df`, as a bit set.
fn generate_damage_set(df: u8, damage_bits: u32) -> u32 {
    let mut result = 1u32 << df;
    if damage_bits == 0 {
        return result;
    }
    for bit in 0..5 {
        result |= generate_damage_set(df ^ (1 << bit), damage_bits - 1);
    }
    result
}

/// Demodulator counters (subset of upstream `struct stats`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DemodStats {
    pub demod_preambles: u64,
    pub demod_rejected_bad: u64,
    pub demod_rejected_unknown_icao: u64,
    /// Messages accepted, indexed by number of corrected bits.
    pub demod_accepted: [u64; 3],
    pub demod_modeac: u64,
    pub demod_preamble_phase: [u64; 5],
    pub demod_best_phase: [u64; 5],
    pub signal_power_sum: f64,
    pub signal_power_count: u64,
    pub noise_power_sum: f64,
    pub noise_power_count: u64,
    pub peak_signal_power: f64,
    /// Messages with signal power above -3 dBFS.
    pub strong_signal_count: u64,
}

/// A block of magnitude samples to demodulate.
#[derive(Debug, Clone, Copy)]
pub struct MagBlock<'a> {
    /// Samples; at least `length + TRAILING_SAMPLES` for full coverage.
    pub data: &'a [u16],
    /// Samples in which message starts are searched.
    pub length: usize,
    /// 12 MHz clock timestamp of `data[0]`.
    pub sample_timestamp: i64,
    /// Estimated system time (ms since the Unix epoch) at the start of the block.
    pub sys_timestamp: i64,
    /// Mean of the normalised (0..1) level of the block.
    pub mean_level: f64,
    /// Mean of the normalised (0..1) power of the block.
    pub mean_power: f64,
}

#[derive(Debug, Clone)]
pub struct Demodulator {
    pub decoder: Decoder,
    /// Preamble threshold, 40..=400; lower = more CPU, more sensitive.
    pub preamble_threshold: u32,
    /// Set when samples were dropped recently: the threshold is raised to cut CPU load.
    pub samples_dropped_recently: bool,
    pub stats: DemodStats,
}

struct Best {
    msg: [u8; LONG_MSG_BYTES],
    score: i32,
    phase: usize,
}

impl Demodulator {
    pub fn new(decoder: Decoder) -> Self {
        Self {
            decoder,
            preamble_threshold: PREAMBLE_THRESHOLD_DEFAULT,
            samples_dropped_recently: false,
            stats: DemodStats::default(),
        }
    }

    /// Valid (short, long) DF sets for early rejection, given the decoder's error-fixing config.
    fn df_sets(&self) -> (u32, u32) {
        let short = (1 << 0) | (1 << 4) | (1 << 5) | (1 << 11);
        let mut long = (1 << 16) | (1 << 17) | (1 << 18) | (1 << 20) | (1 << 21);
        // If we can also repair DF damage, include those corrections.
        if self.decoder.fix_df && self.decoder.nfix_crc() != 0 {
            long |= generate_damage_set(17, 1);
        }
        (short, long)
    }

    fn score_phase(
        &mut self,
        sets: (u32, u32),
        try_phase: usize,
        data: &[u16],
        pa: usize,
        best: &mut Best,
    ) {
        self.stats.demod_preamble_phase[try_phase - 4] += 1;
        let mut pos = pa + 19 + try_phase / 5;
        let mut phase = try_phase % 5;
        let mut msg = [0u8; LONG_MSG_BYTES];
        msg[0] = slice_byte(data, &mut pos, &mut phase);

        // Inspect the DF field early: only continue with messages where it looks valid.
        let df = (msg[0] >> 3) as u32;
        let bytelen = if sets.1 & (1 << df) != 0 {
            LONG_MSG_BYTES
        } else if sets.0 & (1 << df) != 0 {
            SHORT_MSG_BYTES
        } else {
            // only for preamble stats
            best.score = best.score.max(-2);
            return;
        };
        for b in msg.iter_mut().take(bytelen).skip(1) {
            *b = slice_byte(data, &mut pos, &mut phase);
        }

        let score = self.decoder.score(&msg, bytelen * 8);
        if score > best.score {
            *best = Best {
                msg,
                score,
                phase: try_phase,
            };
        }
    }

    /// Demodulate Mode S messages from a block, passing each accepted message to `sink`.
    pub fn demodulate(&mut self, block: MagBlock<'_>, mut sink: impl FnMut(ModesMessage)) {
        let m = block.data;
        // Never start a message where we could not read its full length.
        let stop = block
            .length
            .min(m.len().saturating_sub(TRAILING_SAMPLES.min(m.len())));
        let sets = self.df_sets();
        let mut sum_scaled_signal_power: u64 = 0;
        let mut pa = 0usize;

        while pa < stop {
            // Quick pre-check for the preamble shape.
            //
            // sample#: 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0
            // phase 3: 2/4\0/5\1 0 0 0 0/5\1/3 3\0 0 0 0 0 0 X4
            // phase 4: 1/5\0/4\2 0 0 0 0/4\2 2/4\0 0 0 0 0 0 0 X0
            // phase 5: 0/5\1/3 3\0 0 0 0/3 3\1/5\0 0 0 0 0 0 0 X1
            // phase 6: 0/4\2 2/4\0 0 0 0 2/4\0/5\1 0 0 0 0 0 0 X2
            // phase 7: 0/3 3\1/5\0 0 0 0 1/5\0/4\2 0 0 0 0 0 0 X3
            let Some(p) = m[pa..].first_chunk::<20>() else {
                break;
            };
            if !(p[1] > p[7] && p[12] > p[14] && p[12] > p[15]) {
                pa += 1;
                continue;
            }

            // 5 noise samples
            let base_noise =
                (p[5] as i32) + p[8] as i32 + p[16] as i32 + p[17] as i32 + p[18] as i32;
            let threshold = if self.samples_dropped_recently {
                self.preamble_threshold.max(PREAMBLE_THRESHOLD_PIZERO)
            } else {
                self.preamble_threshold
            };
            let ref_level = (base_noise * threshold as i32) >> 5;

            let mut best = Best {
                msg: [0; LONG_MSG_BYTES],
                score: -42,
                phase: 0,
            };

            let diff_2_3 = p[2] as i32 - p[3] as i32;
            let sum_1_4 = p[1] as i32 + p[4] as i32;
            let diff_10_11 = p[10] as i32 - p[11] as i32;
            let common3456 = sum_1_4 - diff_2_3 + p[9] as i32 + p[12] as i32;

            // phases 3 and 4
            if common3456 - diff_10_11 >= ref_level {
                self.score_phase(sets, 4, m, pa, &mut best);
                self.score_phase(sets, 5, m, pa, &mut best);
            }
            // phases 5 and 6
            if common3456 + diff_10_11 >= ref_level {
                self.score_phase(sets, 6, m, pa, &mut best);
                self.score_phase(sets, 7, m, pa, &mut best);
            }
            // phase 7
            if sum_1_4 + 2 * diff_2_3 + diff_10_11 + p[12] as i32 >= ref_level {
                self.score_phase(sets, 8, m, pa, &mut best);
            }

            // no preamble detected
            if best.score == -42 {
                pa += 1;
                continue;
            }

            // We had at least one phase above the preamble threshold.
            self.stats.demod_preambles += 1;

            if best.score < 0 {
                if best.score == -1 {
                    self.stats.demod_rejected_unknown_icao += 1;
                } else {
                    self.stats.demod_rejected_bad += 1;
                }
                pa += 1;
                continue;
            }

            let msglen = msg_len_by_type(getbits(&best.msg, 1, 5) as u8);

            let mut mm = ModesMessage::from_bytes(&best.msg);
            // As the Beast / Radarcape do, report the timestamp at the end of bit 56,
            // even for a 112-bit frame.
            mm.timestamp =
                block.sample_timestamp + (pa as i64) * 5 + (8 + 56) * 12 + best.phase as i64;
            mm.sys_timestamp =
                block.sys_timestamp + (mm.timestamp - block.sample_timestamp) / 12000;
            mm.score = best.score;

            let result = self.decoder.decode(&mut mm);
            if result < 0 {
                if result == -1 {
                    self.stats.demod_rejected_unknown_icao += 1;
                } else {
                    self.stats.demod_rejected_bad += 1;
                }
                pa += 1;
                continue;
            }
            self.stats.demod_accepted[(mm.corrected_bits as usize).min(2)] += 1;
            self.stats.demod_best_phase[best.phase - 4] += 1;

            // Measure signal power.
            let signal_len = msglen * 12 / 5;
            let mut scaled: u64 = 0;
            for &s in &m[pa + 19..pa + 19 + signal_len] {
                scaled += (s as u64) * (s as u64);
            }
            let signal_power = scaled as f64 / 65535.0 / 65535.0;
            mm.signal_level = signal_power / signal_len as f64;
            self.stats.signal_power_sum += signal_power;
            self.stats.signal_power_count += signal_len as u64;
            sum_scaled_signal_power += scaled;
            if mm.signal_level > self.stats.peak_signal_power {
                self.stats.peak_signal_power = mm.signal_level;
            }
            if mm.signal_level > 0.50119 {
                self.stats.strong_signal_count += 1; // above -3 dBFS
            }

            // Only jump part of the message and let the preamble detection handle the
            // rest, then step on by one as the scan loop would.
            pa += msglen * 8 / 4 + 1;

            sink(mm);
        }

        // Update noise power.
        let sum_signal_power = sum_scaled_signal_power as f64 / 65535.0 / 65535.0;
        self.stats.noise_power_sum += block.mean_power * block.length as f64 - sum_signal_power;
        self.stats.noise_power_count += block.length as u64;
    }

    /// Demodulate Mode A/C replies from a block (`--modeac`). Each reply is reported as a
    /// message made by [`ModesMessage::from_mode_a`].
    ///
    /// Mode A/C replies are 20.3 us long, 15 pulse slots of 1.45 us. At 2.4 MHz a slot is
    /// 87 cycles of a 60 MHz clock and a sample 25 cycles; framing pulses F1 and F2 bracket
    /// the 13 data bits.
    pub fn demodulate_mode_ac(&mut self, block: MagBlock<'_>, mut sink: impl FnMut(ModesMessage)) {
        let m = block.data;
        let mlen = block
            .length
            .min(m.len().saturating_sub(TRAILING_SAMPLES.min(m.len())));

        let noise_stddev = (block.mean_power - block.mean_level * block.mean_level).sqrt();
        let noise_level = ((block.mean_power + noise_stddev) * 65535.0 + 0.5) as u32;

        let mut f1_sample = 1usize;
        while f1_sample < mlen {
            let this = f1_sample;
            f1_sample += 1;

            if m[this - 1] >= m[this] {
                continue; // not a rising edge
            }
            if m[this + 2] > m[this] || m[this + 2] > m[this + 1] {
                continue; // quiet part of bit wasn't sufficiently quiet
            }
            let f1_level = (m[this] as u32 + m[this + 1] as u32) / 2;
            if noise_level * 2 > f1_level {
                continue; // require 6 dB above noise
            }

            // Estimate the initial clock phase from the power in the second sample.
            let f1a = (m[this] as f32) * m[this] as f32;
            let f1b = (m[this + 1] as f32) * m[this + 1] as f32;
            let fraction = f1b / (f1a + f1b);
            let f1_clock = (25.0 * (this as f32 + fraction * fraction) + 0.5) as u32;

            // Same for F2, 20.3 us / 14 bit periods after F1.
            let f2_clock = f1_clock + 87 * 14;
            let f2_sample = (f2_clock / 25) as usize;
            if f2_sample + 2 >= m.len() {
                continue;
            }
            if m[f2_sample - 1] >= m[f2_sample] {
                continue;
            }
            if m[f2_sample + 2] > m[f2_sample] || m[f2_sample + 2] > m[f2_sample + 1] {
                continue;
            }
            let f2_level = (m[f2_sample] as u32 + m[f2_sample + 1] as u32) / 2;
            if noise_level * 2 > f2_level {
                continue;
            }

            let f1f2_level = f1_level.max(f2_level);
            let midpoint = ((noise_level as f32) * f1f2_level as f32).sqrt(); // geometric mean
            let signal_threshold = (midpoint * std::f32::consts::SQRT_2 + 0.5) as u32; // +3 dB
            let noise_threshold = (midpoint / std::f32::consts::SQRT_2 + 0.5) as u32; // -3 dB

            // Looks like a real signal. Demodulate all the bits.
            let (mut bits, mut noisy_bits, mut uncertain_bits) = (0u32, 0u32, 0u32);
            let mut clock = f1_clock;
            for _ in 0..20 {
                let s = (clock / 25) as usize;
                bits <<= 1;
                noisy_bits <<= 1;
                uncertain_bits <<= 1;

                // excessive noise in the quiet period?
                if m[s + 2] as u32 >= signal_threshold {
                    noisy_bits |= 1;
                }
                // is this bit on or off?
                if m[s] as u32 >= signal_threshold || m[s + 1] as u32 >= signal_threshold {
                    bits |= 1;
                } else if m[s] as u32 > noise_threshold && m[s + 1] as u32 > noise_threshold {
                    uncertain_bits |= 1; // not certain about this bit
                }
                clock += 87;
            }

            if bits & 0x80020 != 0x80020 {
                continue; // framing bits must be on
            }
            if bits & 0x0101B != 0 {
                continue; // quiet bits must be off
            }
            if noisy_bits != 0 || uncertain_bits != 0 {
                continue;
            }

            // Convert to the form used elsewhere: 00 A4 A2 A1  00 B4 B2 B1  SPI C4 C2 C1  00 D4 D2 D1
            let map: [(u32, u32); 13] = [
                (0x40000, 0x0010), // C1
                (0x20000, 0x1000), // A1
                (0x10000, 0x0020), // C2
                (0x08000, 0x2000), // A2
                (0x04000, 0x0040), // C4
                (0x02000, 0x4000), // A4
                (0x00800, 0x0100), // B1
                (0x00400, 0x0001), // D1
                (0x00200, 0x0200), // B2
                (0x00100, 0x0002), // D2
                (0x00080, 0x0400), // B4
                (0x00040, 0x0004), // D4
                (0x00004, 0x0080), // SPI
            ];
            let modeac = map
                .iter()
                .filter(|(src, _)| bits & src != 0)
                .fold(0u32, |acc, (_, dst)| acc | dst);

            let mut mm = ModesMessage::from_mode_a(modeac);
            // Report the timestamp at the second framing pulse (F2), as the Beast does.
            mm.timestamp = block.sample_timestamp + (f2_clock / 5) as i64; // 60 MHz -> 12 MHz
            mm.sys_timestamp =
                block.sys_timestamp + (mm.timestamp - block.sample_timestamp) / 12000;
            sink(mm);

            f1_sample = this + 20 * 87 / 25 + 1;
            self.stats.demod_modeac += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::modulate;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn run(demod: &mut Demodulator, data: &[u16]) -> Vec<ModesMessage> {
        let mut out = Vec::new();
        let block = MagBlock {
            data,
            length: data.len() - TRAILING_SAMPLES,
            sample_timestamp: 0,
            sys_timestamp: 0,
            mean_level: 0.05,
            mean_power: 0.01,
        };
        demod.demodulate(block, |m| out.push(m));
        out
    }

    #[test]
    fn trailing_samples_matches_upstream() {
        assert_eq!(TRAILING_SAMPLES, 326);
    }

    #[test]
    fn damage_sets() {
        assert_eq!(generate_damage_set(17, 0), 1 << 17);
        let s = generate_damage_set(17, 1);
        for df in [17, 16, 19, 21, 25, 1] {
            assert!(s & (1 << df) != 0, "{df}");
        }
        assert_eq!(s.count_ones(), 6);
    }

    #[test]
    fn decodes_df17_at_every_subsample_phase() {
        let frame = hex("8D4840D6202CC371C32CE0576098");
        for offset in 0..5 {
            for lead in [20usize, 21, 22] {
                let data = modulate(&frame, lead, offset, 30000.0);
                let mut d = Demodulator::new(Decoder::new(1));
                let msgs = run(&mut d, &data);
                assert_eq!(msgs.len(), 1, "offset {offset} lead {lead}: {msgs:?}");
                assert_eq!(
                    &msgs[0].msg[..14],
                    &frame[..],
                    "offset {offset} lead {lead}"
                );
                assert_eq!(msgs[0].addr, 0x4840D6);
                assert_eq!(msgs[0].callsign.as_deref(), Some("KLM1023 "));
                assert_eq!(d.stats.demod_accepted[0], 1);
            }
        }
    }

    #[test]
    fn decodes_short_all_call_for_known_aircraft() {
        // DF11 all-call reply, II=0: address 4840D6, PI = CRC over the 32 bit payload.
        let mut f = hex("5D4840D6000000");
        let crc = crate::crc::checksum(&f);
        f[4] = (crc >> 16) as u8;
        f[5] = (crc >> 8) as u8;
        f[6] = crc as u8;
        let data = modulate(&f, 30, 2, 20000.0);
        let mut d = Demodulator::new(Decoder::new(1));
        // unknown address: DF11 with II=0 is accepted (score 750)
        let msgs = run(&mut d, &data);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].msgtype, 11);
        assert_eq!(msgs[0].addr, 0x4840D6);
        assert_eq!(msgs[0].msgbits, 56);
        assert!(d.decoder.icao.test(0x4840D6));
    }

    #[test]
    fn two_bit_error_in_signal_is_repaired() {
        let good = hex("8D4840D6202CC371C32CE0576098");
        let mut bad = good.clone();
        bad[8] ^= 0x20; // one flipped bit on the air
        let data = modulate(&bad, 25, 0, 30000.0);
        let mut d = Demodulator::new(Decoder::new(1));
        // Learn the aircraft first so a corrected frame is accepted.
        d.decoder.icao.add(0x4840D6);
        let msgs = run(&mut d, &data);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].corrected_bits, 1);
        assert_eq!(&msgs[0].msg[..14], &good[..]);
        assert_eq!(d.stats.demod_accepted[1], 1);
    }

    #[test]
    fn noise_and_silence_give_nothing() {
        let mut d = Demodulator::new(Decoder::new(1));
        assert!(run(&mut d, &vec![0u16; 2000]).is_empty());
        // pseudo-random low-level noise (xorshift)
        let mut x = 0x1234_5678u32;
        let noise: Vec<u16> = (0..20000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x % 600) as u16
            })
            .collect();
        assert!(run(&mut d, &noise).is_empty());
    }

    #[test]
    fn two_messages_in_one_block() {
        let a = hex("8D4840D6202CC371C32CE0576098");
        let b = hex("8D40621D58C382D690C8AC2863A7");
        let mut data = modulate(&a, 20, 1, 28000.0);
        let second = modulate(&b, 30, 3, 15000.0);
        data.truncate(data.len() - TRAILING_SAMPLES - 50);
        data.extend_from_slice(&second);
        let mut d = Demodulator::new(Decoder::new(1));
        let msgs = run(&mut d, &data);
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[0].addr, 0x4840D6);
        assert_eq!(msgs[1].addr, 0x40621D);
        assert!(msgs[1].timestamp > msgs[0].timestamp);
        assert!(msgs[0].signal_level > msgs[1].signal_level);
    }

    #[test]
    fn timestamp_is_12mhz_clock_at_end_of_bit_56() {
        let frame = hex("8D4840D6202CC371C32CE0576098");
        let data = modulate(&frame, 20, 0, 30000.0);
        let mut d = Demodulator::new(Decoder::new(1));
        let msgs = run(&mut d, &data);
        // preamble starts at sample 20 (+ the detector may lock a sample or two off)
        let ts = msgs[0].timestamp;
        let expected = 20 * 5 + (8 + 56) * 12;
        assert!((ts - expected).abs() <= 12, "ts {ts} expected ~{expected}");
    }

    #[test]
    fn mode_ac_reply_is_demodulated() {
        // Build F1 .. F2 with code 0x1200 (A=1, B=2? whatever) at 2.4 MHz, 87-cycle slots.
        let mode_a = 0x1200u32;
        // slot bits (MSB first of 20): F1 C1 A1 C2 A2 C4 A4 X B1 D1 B2 D2 B4 D4 F2 X X SPI X X
        let src: [(u32, u32); 13] = [
            (0x40000, 0x0010),
            (0x20000, 0x1000),
            (0x10000, 0x0020),
            (0x08000, 0x2000),
            (0x04000, 0x0040),
            (0x02000, 0x4000),
            (0x00800, 0x0100),
            (0x00400, 0x0001),
            (0x00200, 0x0200),
            (0x00100, 0x0002),
            (0x00080, 0x0400),
            (0x00040, 0x0004),
            (0x00004, 0x0080),
        ];
        let mut slots = 0x80020u32; // F1 and F2
        for (slot, code) in src {
            if mode_a & code != 0 {
                slots |= slot;
            }
        }
        // 60 MHz ticks: each slot starts at 87*i, pulse width 25*... use 1 sample (25 ticks) ~ 0.45 us = 27 ticks
        let lead = 40usize;
        let total = (lead + 20 * 4 + TRAILING_SAMPLES + 60) * 25;
        let mut ticks = vec![0u8; total];
        for i in 0..20u32 {
            if slots & (1 << (19 - i)) != 0 {
                let start = lead * 25 + (i * 87) as usize;
                ticks[start..start + 27].fill(1);
            }
        }
        let n = ticks.len() / 25;
        let data: Vec<u16> = (0..n)
            .map(|s| {
                (ticks[s * 25..s * 25 + 25]
                    .iter()
                    .map(|&t| t as u32)
                    .sum::<u32>()
                    * 1000) as u16
            })
            .collect();
        let mut d = Demodulator::new(Decoder::new(1));
        let mut out = Vec::new();
        let block = MagBlock {
            data: &data,
            length: data.len() - TRAILING_SAMPLES,
            sample_timestamp: 0,
            sys_timestamp: 0,
            mean_level: 0.001,
            mean_power: 0.00001,
        };
        d.demodulate_mode_ac(block, |m| out.push(m));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].squawk.map(|s| s.hex), Some(mode_a));
        assert_eq!(d.stats.demod_modeac, 1);
    }
}
