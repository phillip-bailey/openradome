//! RTL-SDR input (`--device-type rtlsdr`), through the pure-Rust `rtlsdr-nusb` driver.
//!
//! A reader thread owns the dongle and does nothing but move USB blocks into a bounded queue,
//! so the decoder falling behind for a moment never stalls the USB transfers. If the queue
//! does fill up, the block is dropped and counted (upstream reports the same as "samples dropped").
//!
//! The reader thread never blocks on the queue. When it stops (read error, or the dongle silent
//! for [`WATCHDOG`]) it leaves its error in a shared slot and exits; the decoder sees the queue
//! disconnect once it has used up what was already queued, and then gets the error.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rtlsdr_nusb::{Device, GainConfig, MaybeFuture, RawIq, RxStream};

/// USB transfers waiting for the decoder. At 256 KiB each this is about 3.5 s of samples at 2.4 MS/s.
const QUEUE_BLOCKS: usize = 32;
/// How long the dongle may deliver nothing, without reporting an error, before we give up on it
/// so that the service manager restarts us and the dongle is reopened.
pub const WATCHDOG: Duration = Duration::from_secs(30);
/// How long to wait for the reader thread to release the dongle on shutdown. A wedged dongle
/// must not keep the process from exiting.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);
const OPEN_WAIT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct SdrConfig {
    /// Device index, or a serial number if it does not parse as an index.
    pub device: String,
    pub freq_hz: u64,
    pub sample_rate: u32,
    /// `None` means the tuner's automatic gain.
    pub gain_db: Option<f32>,
    pub ppm: i32,
    pub bias_tee: bool,
}

#[derive(Debug, PartialEq)]
pub enum Chunk {
    /// Raw unsigned 8-bit I/Q pairs. `dropped_before` is how many samples were lost just
    /// before these (by the driver, or by us when the queue was full): the sample clock must
    /// skip them so that later timestamps stay true, which MLAT depends on.
    Data { bytes: Vec<u8>, dropped_before: u64 },
    /// The dongle failed or went silent; the reader has stopped.
    Failed(String),
}

/// Where the samples come from: the dongle, or a fake one in tests.
trait Source {
    /// The next block of raw I/Q bytes and the number of samples the driver lost before it,
    /// `Ok(None)` if nothing arrived within `timeout`.
    fn next(&mut self, timeout: Duration) -> Result<Option<(Vec<u8>, u64)>, String>;
    /// Stop streaming and release the device. May block on a wedged device.
    fn close(self);
}

struct Shared {
    stop: AtomicBool,
    error: Mutex<Option<String>>,
    /// Samples lost after the last block that was queued, reported at shutdown.
    final_drop: AtomicU64,
}

pub struct SdrReader {
    rx: Receiver<Chunk>,
    shared: Arc<Shared>,
    /// Disconnects when the reader thread has finished, including releasing the device.
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    pub description: String,
}

/// The tuner gain that the user asked for with `--gain`.
///
/// librtlsdr's table tops out at 49.6 dB, which is LNA and mixer both at maximum. The pure-Rust
/// driver models the same two stages in 2.3 dB and 1.2 dB steps up to 52.5 dB, so anything at or
/// above librtlsdr's maximum maps to that same fully-open setting.
pub fn tuner_gain(requested_db: f32) -> GainConfig {
    if requested_db >= 49.0 {
        GainConfig::Manual(52.5)
    } else {
        GainConfig::Manual(requested_db.max(0.0))
    }
}

/// librtlsdr's R820T gain steps in tenths of a dB. Upstream snaps `--gain` to the closest one
/// and reports that in `stats.json`.
const R820T_GAINS: [i64; 29] = [
    0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280, 297, 328, 338, 364, 372,
    386, 402, 421, 434, 439, 445, 480, 496,
];

/// The gain to report for `--gain`, as upstream does: the closest librtlsdr step, the maximum
/// when no gain was given, and nothing for automatic gain (`-10`).
pub fn reported_gain(requested_db: Option<f32>) -> Option<f64> {
    const MAX_STEP: i64 = R820T_GAINS[R820T_GAINS.len() - 1];
    let tenths = match requested_db {
        None => MAX_STEP,
        Some(-10.0) => return None,
        // clamp to the table's range first, so no input can overflow the distance below
        Some(g) => (f64::from(g) * 10.0).round().clamp(0.0, MAX_STEP as f64) as i64,
    };
    let step = R820T_GAINS
        .iter()
        .copied()
        .min_by_key(|s| (s - tenths).abs())
        .unwrap_or(MAX_STEP);
    Some(step as f64 / 10.0)
}

impl SdrReader {
    /// Open the dongle and start streaming. Fails (with the driver's message) if it can't.
    pub fn start(cfg: &SdrConfig) -> Result<Self, String> {
        let cfg = cfg.clone();
        Self::spawn(move || RtlSource::open(&cfg), WATCHDOG, OPEN_WAIT)
    }

    fn spawn<S, F>(open: F, watchdog: Duration, open_wait: Duration) -> Result<Self, String>
    where
        S: Source,
        F: FnOnce() -> Result<(S, String), String> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            error: Mutex::new(None),
            final_drop: AtomicU64::new(0),
        });
        let (tx, rx) = sync_channel::<Chunk>(QUEUE_BLOCKS);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<String, String>>();
        let (done_tx, done) = mpsc::channel::<()>();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("rtlsdr".into())
            .spawn(move || {
                let _done = done_tx; // dropped, and so disconnected, when this thread ends
                let (source, description) = match open() {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(description));
                reader(source, &thread_shared, &tx, watchdog);
            })
            .map_err(|e| format!("can't start the RTL-SDR thread: {e}"))?;
        match ready_rx.recv_timeout(open_wait) {
            Ok(Ok(description)) => Ok(Self {
                rx,
                shared,
                done,
                thread: Some(thread),
                description,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                // The open is stuck; leave the thread behind rather than hang with it.
                shared.stop.store(true, Ordering::SeqCst);
                Err("rtlsdr: timed out opening the device".into())
            }
        }
    }

    /// Wait up to `timeout` for the next chunk.
    pub fn recv(&self, timeout: Duration) -> Option<Chunk> {
        match self.rx.recv_timeout(timeout) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Chunk::Failed(
                self.shared
                    .error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .unwrap_or_else(|| "rtlsdr: the reader thread ended".into()),
            )),
        }
    }
}

impl SdrReader {
    /// Stop streaming and release the dongle. Returns the samples that will not be decoded:
    /// blocks still queued, and losses after the last queued block, so the final statistics
    /// count them as dropped.
    pub fn shutdown(mut self) -> u64 {
        self.stop_reader();
        let mut lost = self.shared.final_drop.load(Ordering::SeqCst);
        while let Ok(chunk) = self.rx.try_recv() {
            if let Chunk::Data {
                bytes,
                dropped_before,
            } = chunk
            {
                lost += dropped_before + (bytes.len() / 2) as u64;
            }
        }
        lost
    }

    fn stop_reader(&mut self) {
        if self.thread.is_none() {
            return;
        }
        self.shared.stop.store(true, Ordering::SeqCst);
        // The reader never blocks on the queue, so it only fails to finish if the device does.
        match self.done.recv_timeout(SHUTDOWN_WAIT) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(t) = self.thread.take() {
                    let _ = t.join();
                }
            }
            _ => {
                self.thread = None;
                eprintln!("rtlsdr: the dongle did not shut down in time; leaving it");
            }
        }
    }
}

impl Drop for SdrReader {
    fn drop(&mut self) {
        self.stop_reader();
    }
}

fn reader(mut source: impl Source, shared: &Shared, tx: &SyncSender<Chunk>, watchdog: Duration) {
    // Poll often enough that both stopping and the watchdog react promptly.
    let poll = watchdog.min(Duration::from_millis(500) * 4) / 4;
    let mut last_data = Instant::now();
    // samples lost before the next block we manage to queue
    let mut pending_drop: u64 = 0;
    let error = loop {
        if shared.stop.load(Ordering::Relaxed) {
            break None;
        }
        match source.next(poll) {
            Ok(Some((bytes, lost))) => {
                last_data = Instant::now();
                pending_drop += lost;
                let samples = (bytes.len() / 2) as u64;
                let chunk = Chunk::Data {
                    bytes,
                    dropped_before: pending_drop,
                };
                if tx.try_send(chunk).is_err() {
                    // The decoder is behind and the queue is full (or gone): drop the block
                    // and account for it before the next one that gets through.
                    pending_drop += samples;
                } else {
                    pending_drop = 0;
                }
                // kept current, so shutdown sees it even if this thread never finishes
                shared.final_drop.store(pending_drop, Ordering::SeqCst);
            }
            Ok(None) => {
                if last_data.elapsed() >= watchdog {
                    break Some(format!(
                        "rtlsdr: no samples for {} s, giving up",
                        watchdog.as_secs_f64()
                    ));
                }
            }
            Err(e) => break Some(e),
        }
    };
    // Report before releasing the device, which may hang on a wedged dongle.
    *shared.error.lock().unwrap_or_else(|e| e.into_inner()) = error;
    source.close();
}

struct RtlSource {
    device: Device<RawIq>,
    rx: RxStream<RawIq>,
}

impl RtlSource {
    fn open(cfg: &SdrConfig) -> Result<(Self, String), String> {
        let mut builder = Device::builder()
            .frequency_hz(cfg.freq_hz)
            .sample_rate_hz(cfg.sample_rate)
            .gain(cfg.gain_db.map_or(GainConfig::Auto, tuner_gain))
            .correction_ppm(cfg.ppm)
            .bias_tee(cfg.bias_tee);
        builder = match cfg.device.parse::<usize>() {
            Ok(i) => builder.index(i),
            Err(_) => builder.serial(cfg.device.clone()),
        };
        let device = builder
            .raw_iq()
            .open()
            .wait()
            .map_err(|e| format!("rtlsdr: can't open device {}: {e}", cfg.device))?;
        let mut rx = device
            .rx_stream()
            .map_err(|e| format!("rtlsdr: can't create the sample stream: {e}"))?;
        rx.start()
            .wait()
            .map_err(|e| format!("rtlsdr: can't start streaming: {e}"))?;
        let description = format!(
            "{:?}, tuned to {} Hz at {} samples/s",
            device.info(),
            device.actual_frequency_hz(),
            device.actual_sample_rate_hz()
        );
        Ok((Self { device, rx }, description))
    }
}

impl Source for RtlSource {
    fn next(&mut self, timeout: Duration) -> Result<Option<(Vec<u8>, u64)>, String> {
        match self.rx.next_block(Some(timeout)).wait() {
            // One copy per USB transfer, nothing else: the pipeline accepts any chunking.
            Ok(Some(b)) => Ok(Some((b.raw_bytes().to_vec(), b.dropped_samples()))),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("rtlsdr: read error: {e}")),
        }
    }

    fn close(mut self) {
        let _ = self.rx.close().wait();
        let _ = self.device.shutdown().wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn librtlsdr_max_gain_maps_to_fully_open() {
        assert_eq!(tuner_gain(49.6), GainConfig::Manual(52.5));
        assert_eq!(tuner_gain(58.0), GainConfig::Manual(52.5));
        assert_eq!(tuner_gain(30.0), GainConfig::Manual(30.0));
        assert_eq!(tuner_gain(-3.0), GainConfig::Manual(0.0));
    }

    #[test]
    fn reported_gain_snaps_to_librtlsdr_steps() {
        assert_eq!(reported_gain(Some(49.6)), Some(49.6));
        assert_eq!(reported_gain(None), Some(49.6));
        assert_eq!(reported_gain(Some(58.0)), Some(49.6));
        assert_eq!(reported_gain(Some(40.0)), Some(40.2));
        assert_eq!(reported_gain(Some(0.3)), Some(0.0));
        assert_eq!(reported_gain(Some(-10.0)), None);
        // absurd values clamp instead of overflowing
        assert_eq!(reported_gain(Some(-1e18)), Some(0.0));
        assert_eq!(reported_gain(Some(1e18)), Some(49.6));
    }

    /// A scripted dongle. `None` steps time out; after the script it keeps timing out, or
    /// keeps producing data if `endless` is set.
    struct Fake {
        script: VecDeque<Option<Result<Vec<u8>, String>>>,
        endless: bool,
        close_takes: Duration,
    }

    impl Source for Fake {
        fn next(&mut self, timeout: Duration) -> Result<Option<(Vec<u8>, u64)>, String> {
            match self.script.pop_front() {
                Some(Some(Ok(b))) => Ok(Some((b, 0))),
                Some(Some(Err(e))) => Err(e),
                Some(None) => {
                    std::thread::sleep(timeout);
                    Ok(None)
                }
                None if self.endless => Ok(Some((vec![0; 4], 0))),
                None => {
                    std::thread::sleep(timeout);
                    Ok(None)
                }
            }
        }
        fn close(self) {
            std::thread::sleep(self.close_takes);
        }
    }

    fn data(bytes: Vec<u8>) -> Chunk {
        Chunk::Data {
            bytes,
            dropped_before: 0,
        }
    }

    fn fake(script: Vec<Option<Result<Vec<u8>, String>>>) -> Fake {
        Fake {
            script: script.into(),
            endless: false,
            close_takes: Duration::ZERO,
        }
    }

    fn start(f: Fake, watchdog: Duration) -> SdrReader {
        SdrReader::spawn(move || Ok((f, "fake".into())), watchdog, OPEN_WAIT).unwrap()
    }

    /// Next chunk, waiting at most `limit`.
    fn next(r: &SdrReader, limit: Duration) -> Option<Chunk> {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            if let Some(c) = r.recv(Duration::from_millis(20)) {
                return Some(c);
            }
        }
        None
    }

    #[test]
    fn silent_dongle_trips_the_watchdog() {
        let r = start(fake(vec![Some(Ok(vec![1, 2]))]), Duration::from_millis(200));
        assert_eq!(next(&r, Duration::from_secs(2)), Some(data(vec![1, 2])));
        match next(&r, Duration::from_secs(2)) {
            Some(Chunk::Failed(e)) => assert!(e.contains("no samples"), "{e}"),
            other => panic!("expected the watchdog, got {other:?}"),
        }
    }

    #[test]
    fn a_slow_decoder_does_not_trip_the_watchdog() {
        // Data keeps coming; the consumer just isn't reading for longer than the watchdog.
        let mut f = fake(vec![]);
        f.endless = true;
        let r = start(f, Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(400));
        let mut dropped = 0;
        for _ in 0..QUEUE_BLOCKS + 2 {
            match r.recv(Duration::from_secs(1)) {
                Some(Chunk::Data { dropped_before, .. }) => dropped += dropped_before,
                other => panic!("expected data, got {other:?}"),
            }
        }
        assert!(
            dropped > 0,
            "blocks that did not fit are reported on the next one"
        );
    }

    #[test]
    fn drops_not_yet_reported_are_returned_at_shutdown() {
        // data keeps coming and nobody reads: the queue fills and later blocks are dropped
        let mut f = fake(vec![]);
        f.endless = true;
        let r = start(f, Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            r.shutdown() > 0,
            "drops after the last queued block are reported"
        );
    }

    #[test]
    fn queued_data_is_delivered_before_the_error() {
        let r = start(
            fake(vec![
                Some(Ok(vec![1, 1])),
                Some(Ok(vec![2, 2])),
                Some(Err("rtlsdr: read error: gone".into())),
            ]),
            Duration::from_secs(10),
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(next(&r, Duration::from_secs(1)), Some(data(vec![1, 1])));
        assert_eq!(next(&r, Duration::from_secs(1)), Some(data(vec![2, 2])));
        assert_eq!(
            next(&r, Duration::from_secs(1)),
            Some(Chunk::Failed("rtlsdr: read error: gone".into()))
        );
    }

    #[test]
    fn shutdown_does_not_hang_on_a_full_queue_or_a_wedged_dongle() {
        let mut script: Vec<_> = (0..QUEUE_BLOCKS + 5)
            .map(|_| Some(Ok(vec![0; 2])))
            .collect();
        script.push(Some(Err("rtlsdr: read error: gone".into())));
        let mut f = fake(script);
        f.close_takes = Duration::from_secs(60);
        let r = start(f, Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(200)); // queue full, error reported, close stuck
        let t = Instant::now();
        drop(r);
        assert!(t.elapsed() < SHUTDOWN_WAIT + Duration::from_secs(1));
    }

    #[test]
    fn open_failure_is_reported() {
        let e = SdrReader::spawn(
            || Err::<(Fake, String), _>("rtlsdr: can't open device 0: no device".into()),
            WATCHDOG,
            OPEN_WAIT,
        )
        .err()
        .unwrap();
        assert!(e.contains("no device"));
    }
}
