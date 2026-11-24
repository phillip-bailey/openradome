//! Test signal generator: turns Mode S frames into 2.4 MHz magnitude samples.
//!
//! Used by the unit tests and by the `synth_iq` example to build IQ captures with
//! known content, so decoder output can be checked against ground truth.

use crate::demod::TRAILING_SAMPLES;

/// Synthesize magnitude samples at 2.4 MHz for a Mode S frame.
///
/// The ideal 1 Mbps PPM waveform (8 us preamble, then one bit per microsecond, a 1 being
/// high in the first half) is built at 12 MHz (6 ticks per 0.5 us symbol), delayed by
/// `lead_samples` whole samples plus `offset_ticks` (0..5, the sub-sample phase), then
/// box-averaged 5:1 down to 2.4 MHz. `amp` is the full-scale pulse height (max 65535).
/// The output ends with `TRAILING_SAMPLES + 50` samples of silence.
pub fn modulate(msg: &[u8], lead_samples: usize, offset_ticks: usize, amp: f64) -> Vec<u16> {
    let mut ticks = vec![0u8; lead_samples * 5 + offset_ticks];
    let push_pulse = |ticks: &mut Vec<u8>, start: usize, on: bool| {
        if ticks.len() < start + 6 {
            ticks.resize(start + 6, 0);
        }
        if on {
            ticks[start..start + 6].fill(1);
        }
    };
    let base = ticks.len();
    // preamble: pulses at 0, 1.0, 3.5 and 4.5 us (in half-microsecond symbols: 0, 2, 7, 9)
    for sym in [0usize, 2, 7, 9] {
        push_pulse(&mut ticks, base + sym * 6, true);
    }
    for i in 0..msg.len() * 8 {
        let bit = (msg[i / 8] >> (7 - i % 8)) & 1;
        let start = base + 96 + i * 12;
        push_pulse(&mut ticks, start, bit == 1);
        push_pulse(&mut ticks, start + 6, bit == 0);
    }
    ticks.resize(ticks.len() + (TRAILING_SAMPLES + 50) * 5, 0);
    (0..ticks.len() / 5)
        .map(|s| {
            let sum: u32 = ticks[s * 5..s * 5 + 5].iter().map(|&t| t as u32).sum();
            (amp * sum as f64 / 5.0) as u16
        })
        .collect()
}

/// Magnitudes to UC8 IQ bytes (I carries the magnitude, Q is centred), the RTL-SDR format.
pub fn to_uc8(mags: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(mags.len() * 2);
    for &m in mags {
        out.push((127.5 + 127.5 * (m as f64 / 65535.0)).round() as u8);
        out.push(128);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_and_first_bit_shape() {
        let m = modulate(&[0x80, 0, 0, 0, 0, 0, 0], 10, 0, 1000.0);
        // lead 10, preamble pulse 0 is 0.5 us = 1.2 samples: sample 10 full, sample 11 partly
        assert_eq!(m[9], 0);
        assert_eq!(m[10], 1000);
        assert!(m[11] > 0 && m[11] < 1000);
        // length: lead + 8us preamble + 56 bits + trailing silence
        assert!(m.len() >= 10 + (8 + 56) * 12 / 5 + TRAILING_SAMPLES);
    }

    #[test]
    fn uc8_round_trip_is_close() {
        let mags = [0u16, 1000, 30000, 65535];
        let back = {
            let iq = to_uc8(&mags);
            let mut out = [0u16; 4];
            crate::convert::convert(crate::convert::InputFormat::Uc8, &iq, &mut out);
            out
        };
        // the Q channel is offset by one half-step, so allow a small floor
        for (a, b) in mags.iter().zip(back) {
            assert!((*a as i32 - b as i32).abs() < 700, "{a} vs {b}");
        }
    }
}
