//! Sample pipeline: raw IQ bytes in, decoded messages out. Port of the block
//! handling in upstream's `sdr_ifile.c` / reader loop.
//!
//! Samples are converted to magnitudes and demodulated in fixed-size blocks. Each block
//! is preceded by [`TRAILING_SAMPLES`] of overlap from the previous one, so a message
//! that straddles the boundary is still seen whole. Block boundaries matter: they
//! decide where the demodulator restarts after a message, so keeping upstream's block
//! size keeps the output comparable.

use crate::convert::{BlockStats, InputFormat, convert};
use crate::demod::{Demodulator, MagBlock, SAMPLE_RATE, TRAILING_SAMPLES};
use crate::mode_s::ModesMessage;

/// Samples per block upstream uses by default (128 KiB / 2).
pub const DEFAULT_BLOCK_SAMPLES: usize = 64 * 1024;

pub struct IqPipeline {
    pub demod: Demodulator,
    pub format: InputFormat,
    /// Also look for Mode A/C replies (`--modeac`).
    pub mode_ac: bool,
    block_samples: usize,
    /// `[overlap | new samples]`
    mag: Vec<u16>,
    filled: usize,
    pending: Vec<u8>,
    sample_counter: u64,
    /// System time (ms since the Unix epoch) corresponding to sample 0.
    pub startup_time: i64,
    samples_lost: u64,
    samples_dropped: u64,
    /// Sample count right after the last gap, to raise the preamble threshold for a while.
    last_drop_sample: Option<u64>,
    /// The block being run is cut short by a gap, not by the end of the input.
    gap_flush: bool,
    stats_sum: BlockStats,
}

/// After dropped samples the preamble threshold is raised for this long (upstream uses its
/// 15 minute statistics window), in samples at 2.4 MS/s.
const DROP_MEMORY_SAMPLES: u64 = 15 * 60 * SAMPLE_RATE as u64;

impl IqPipeline {
    pub fn new(demod: Demodulator, format: InputFormat, block_samples: usize) -> Self {
        assert!(block_samples > 0);
        Self {
            demod,
            format,
            mode_ac: false,
            block_samples,
            mag: vec![0; TRAILING_SAMPLES + block_samples],
            filled: 0,
            pending: Vec::new(),
            sample_counter: 0,
            startup_time: 0,
            samples_lost: 0,
            samples_dropped: 0,
            last_drop_sample: None,
            gap_flush: false,
            stats_sum: BlockStats::default(),
        }
    }

    /// Samples missing from short blocks (upstream counts the unfilled part of a short
    /// final block as lost).
    pub fn samples_lost(&self) -> u64 {
        self.samples_lost
    }

    /// Samples the SDR driver reported as dropped (live input only).
    pub fn samples_dropped(&self) -> u64 {
        self.samples_dropped
    }

    /// Record `n` samples the source lost just before the ones pushed next. What is already in
    /// the current block is demodulated first, because the samples after the gap belong to a
    /// later time. The sample clock then skips the gap, so the 12 MHz timestamps of everything
    /// after it stay true (MLAT depends on that), the overlap is cleared so no message is
    /// assembled across the gap, and the preamble threshold is raised for a while. This is what
    /// upstream does with a dropped USB block.
    pub fn add_dropped(&mut self, n: u64, on_block: &mut impl FnMut(Vec<ModesMessage>)) {
        if n == 0 {
            return;
        }
        if self.filled > 0 {
            self.gap_flush = true;
            self.run_block(on_block);
            self.gap_flush = false;
        }
        // An incomplete sample from before the gap must not be completed by bytes after it:
        // it belongs to a sample lost in the gap, so it is counted with it.
        let half = u64::from(!self.pending.is_empty());
        self.pending.clear();
        self.samples_dropped += n + half;
        self.sample_counter += n + half;
        self.last_drop_sample = Some(self.sample_counter);
        self.mag[..TRAILING_SAMPLES].fill(0);
    }

    /// The stream's clock: system time (ms) of the last sample consumed.
    pub fn now_ms(&self) -> i64 {
        let clock = self.sample_counter + self.filled as u64; // gaps included
        self.startup_time + (clock as f64 * 1000.0 / SAMPLE_RATE) as i64
    }

    /// Samples consumed so far, not counting samples dropped before they arrived.
    pub fn samples(&self) -> u64 {
        // samples demodulated (upstream's samples_processed); dropped ones only move the clock
        self.sample_counter + self.filled as u64 - self.samples_dropped
    }

    /// Feed raw IQ bytes in any chunking. Complete blocks are demodulated as they fill; the
    /// messages of each block are handed over together via `on_block`, because upstream
    /// processes (tracks, then forwards) a whole block's messages at once.
    pub fn push(&mut self, mut bytes: &[u8], on_block: &mut impl FnMut(Vec<ModesMessage>)) {
        let bps = self.format.bytes_per_sample();
        // `pending` holds less than one sample: the tail of the previous call.
        if !self.pending.is_empty() {
            let need = bps - self.pending.len();
            if bytes.len() < need {
                self.pending.extend_from_slice(bytes);
                return;
            }
            self.pending.extend_from_slice(&bytes[..need]);
            bytes = &bytes[need..];
            let sample = std::mem::take(&mut self.pending);
            self.convert_samples(&sample, on_block);
        }
        // Convert straight from the caller's buffer; only an incomplete last sample is kept.
        let usable = bytes.len() / bps * bps;
        self.convert_samples(&bytes[..usable], on_block);
        self.pending.extend_from_slice(&bytes[usable..]);
    }

    /// Convert whole samples into the current block, running each block as it fills.
    fn convert_samples(&mut self, data: &[u8], on_block: &mut impl FnMut(Vec<ModesMessage>)) {
        let bps = self.format.bytes_per_sample();
        let mut offset = 0;
        while offset < data.len() {
            let room = self.block_samples - self.filled;
            let want = ((data.len() - offset) / bps).min(room);
            let start = TRAILING_SAMPLES + self.filled;
            let (n, st) = convert(
                self.format,
                &data[offset..offset + want * bps],
                &mut self.mag[start..start + want],
            );
            // Accumulate sample-weighted statistics for the block.
            let (f0, f1) = (self.filled as f64, n as f64);
            let tot = f0 + f1;
            self.stats_sum.mean_level = (self.stats_sum.mean_level * f0 + st.mean_level * f1) / tot;
            self.stats_sum.mean_power = (self.stats_sum.mean_power * f0 + st.mean_power * f1) / tot;
            self.filled += n;
            offset += n * bps;
            if self.filled == self.block_samples {
                self.run_block(on_block);
            }
        }
    }

    /// Demodulate whatever is left in a partial final block.
    pub fn finish(&mut self, on_block: &mut impl FnMut(Vec<ModesMessage>)) {
        if self.filled > 0 {
            self.run_block(on_block);
        }
    }

    fn run_block(&mut self, on_block: &mut impl FnMut(Vec<ModesMessage>)) {
        let length = self.filled;
        if !self.gap_flush {
            self.samples_lost += (self.block_samples - length) as u64;
        }
        self.demod.samples_dropped_recently = self
            .last_drop_sample
            .is_some_and(|s| self.sample_counter - s < DROP_MEMORY_SAMPLES);
        // Samples past `length` are stale or zero: clear them like a fresh block would be.
        let end = TRAILING_SAMPLES + length;
        if end < self.mag.len() {
            let tail_end = (end + TRAILING_SAMPLES).min(self.mag.len());
            self.mag[end..tail_end].fill(0);
        }
        let sample_timestamp = (self.sample_counter as f64 * 12e6 / SAMPLE_RATE) as i64;
        let block = MagBlock {
            data: &self.mag
                [..TRAILING_SAMPLES + length + TRAILING_SAMPLES.min(self.mag.len() - end)],
            length,
            sample_timestamp,
            sys_timestamp: sample_timestamp / 12000 + self.startup_time,
            mean_level: self.stats_sum.mean_level,
            mean_power: self.stats_sum.mean_power,
        };
        let mut batch = Vec::new();
        self.demod.demodulate(block, |mm| batch.push(mm));
        if self.mode_ac {
            self.demod.demodulate_mode_ac(block, |mm| batch.push(mm));
        }

        self.sample_counter += length as u64;
        // Carry the last TRAILING_SAMPLES into the next block, or reset if too short.
        if length >= TRAILING_SAMPLES {
            self.mag.copy_within(length..length + TRAILING_SAMPLES, 0);
        } else {
            self.mag[..TRAILING_SAMPLES].fill(0);
        }
        self.filled = 0;
        self.stats_sum = BlockStats::default();
        on_block(batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode_s::Decoder;
    use crate::synth::modulate;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    /// Magnitudes back to UC8 IQ bytes (I = mag, Q = 0, centred on 127.5).
    fn to_uc8(mags: &[u16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(mags.len() * 2);
        for &m in mags {
            let i = (127.5 + 127.5 * (m as f64 / 65535.0)).round() as u8;
            out.push(i);
            out.push(128);
        }
        out
    }

    fn run_all(chunk: usize, block: usize, bytes: &[u8]) -> Vec<ModesMessage> {
        let mut p = IqPipeline::new(Demodulator::new(Decoder::new(1)), InputFormat::Uc8, block);
        let mut out = Vec::new();
        let mut sink = |batch: Vec<ModesMessage>| out.extend(batch);
        for c in bytes.chunks(chunk) {
            p.push(c, &mut sink);
        }
        p.finish(&mut sink);
        out
    }

    #[test]
    fn decodes_from_uc8_iq_in_any_chunking() {
        let frame = hex("8D4840D6202CC371C32CE0576098");
        let mags = modulate(&frame, 100, 2, 30000.0);
        let bytes = to_uc8(&mags);
        for chunk in [1usize, 7, 4096, bytes.len()] {
            let msgs = run_all(chunk, DEFAULT_BLOCK_SAMPLES, &bytes);
            assert_eq!(msgs.len(), 1, "chunk {chunk}");
            assert_eq!(&msgs[0].msg[..14], &frame[..]);
        }
    }

    #[test]
    fn message_straddling_a_block_boundary_is_not_lost() {
        let frame = hex("8D4840D6202CC371C32CE0576098");
        let mags = modulate(&frame, 100, 0, 30000.0);
        let bytes = to_uc8(&mags);
        // A tiny block size puts block boundaries inside the message's samples.
        for block in [400usize, 500, 523] {
            let msgs = run_all(1000, block, &bytes);
            assert!(!msgs.is_empty(), "block {block}");
            assert_eq!(&msgs[0].msg[..14], &frame[..]);
        }
    }

    #[test]
    fn a_gap_shifts_later_timestamps_and_raises_the_threshold() {
        let frame = hex("8D4840D6202CC371C32CE0576098");
        let bytes = to_uc8(&modulate(&frame, 100, 0, 30000.0));
        let run = |gap: u64| {
            let mut p = IqPipeline::new(
                Demodulator::new(Decoder::new(1)),
                InputFormat::Uc8,
                DEFAULT_BLOCK_SAMPLES,
            );
            let mut out = Vec::new();
            let mut sink = |batch: Vec<ModesMessage>| out.extend(batch);
            p.push(&[128u8; 2 * 1000], &mut sink); // 1000 samples of silence
            p.add_dropped(gap, &mut sink); // then the source loses `gap` samples
            p.push(&bytes, &mut sink);
            p.finish(&mut sink);
            (
                out,
                p.samples_dropped(),
                p.samples_lost(),
                p.demod.samples_dropped_recently,
            )
        };
        let (plain, d0, _, flag0) = run(0);
        let (gapped, d1, lost1, flag1) = run(5000);
        assert_eq!(plain.len(), 1);
        assert_eq!(gapped.len(), 1, "the frame after the gap still decodes");
        // 5000 samples at 2.4 MS/s are 25000 ticks of the 12 MHz clock
        assert_eq!(gapped[0].timestamp - plain[0].timestamp, 5000 * 5);
        assert_eq!((d0, d1), (0, 5000));
        assert_eq!(
            lost1,
            DEFAULT_BLOCK_SAMPLES as u64 - bytes.len() as u64 / 2,
            "the gap flush is not counted as lost"
        );
        assert!(
            !flag0 && flag1,
            "dropped samples raise the preamble threshold"
        );
    }

    #[test]
    fn a_half_sample_does_not_span_a_gap() {
        let frame = hex("8D4840D6202CC371C32CE0576098");
        let bytes = to_uc8(&modulate(&frame, 100, 0, 30000.0));
        let mut p = IqPipeline::new(
            Demodulator::new(Decoder::new(1)),
            InputFormat::Uc8,
            DEFAULT_BLOCK_SAMPLES,
        );
        let mut out = Vec::new();
        let mut sink = |batch: Vec<ModesMessage>| out.extend(batch);
        p.push(&[128u8; 2001], &mut sink); // ends on half a sample
        p.add_dropped(1000, &mut sink);
        p.push(&bytes, &mut sink); // must start on an I byte again
        p.finish(&mut sink);
        assert_eq!(out.len(), 1, "the frame after the gap decodes");
        assert_eq!(&out[0].msg[..14], &frame[..]);
        assert_eq!(
            p.samples_dropped(),
            1001,
            "the gap and the half sample before it"
        );
        assert_eq!(
            p.samples(),
            1000 + bytes.len() as u64 / 2,
            "processed excludes the gap"
        );
    }

    #[test]
    fn counts_samples_and_handles_empty_input() {
        let mut p = IqPipeline::new(Demodulator::new(Decoder::new(1)), InputFormat::Uc8, 1000);
        let mut sink = |_b: Vec<ModesMessage>| {};
        p.push(&[], &mut sink);
        p.push(&[128; 2001], &mut sink); // odd byte stays pending
        assert_eq!(p.samples(), 1000);
        p.finish(&mut sink);
        assert_eq!(p.samples(), 1000);
    }
}
