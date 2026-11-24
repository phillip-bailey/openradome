//! Periodic JSON files (`--write-json <dir>`), written atomically.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use rustadsb_core::bincraft;
use rustadsb_core::json::{self, Cumulative, ReceiverInfo, StatsRecorder};
use rustadsb_core::pipeline::IqPipeline;
use rustadsb_core::track::Tracker;

pub const VERSION_STRING: &str = concat!("rustadsb ", env!("CARGO_PKG_VERSION"));

/// Which files `--write-json` produces besides the always-written `aircraft.json` family.
#[derive(Debug, Clone, Default)]
pub struct JsonOptions {
    /// `--write-json-gzip`: also `aircraft.json.gz`.
    pub gzip: bool,
    /// `--write-json-binCraft-only`: 1 = for globe files, 2 = `aircraft.json` is not written either.
    pub bin_only: u8,
    /// `--write-binCraft-old`: also the gzipped `aircraft.binCraft`.
    pub bin_gz: bool,
    /// `--json-location-accuracy 0`: leave the receiver position out of the binary header.
    pub hide_receiver: bool,
    /// `--tar1090-use-api`: receiver.json tells tar1090 to use the API.
    pub reapi: bool,
    /// Globe tiles and traces (`--write-json-globe-index`, `--write-globe-history`).
    pub globe: crate::globefiles::Options,
    /// Trace interval in seconds, shown in receiver.json when traces are written.
    pub trace_interval_s: f64,
    /// There is a local SDR or IQ file: `stats.json` gets the receiver block and the gain.
    pub with_local: bool,
    /// The tuner gain to report, if known (upstream writes 99999.9 otherwise).
    pub gain_db: Option<f64>,
}

pub struct JsonWriter {
    opts: JsonOptions,
    globe: Option<crate::globefiles::GlobeWriter>,
    /// Messages per second, from the change in the message counter between writes.
    rate_last: (i64, u64),
    message_rate: f64,
    dir: PathBuf,
    interval_ms: i64,
    next_aircraft: i64,
    recorder: StatsRecorder,
    nfix: u8,
    /// There is a local SDR or IQ file: `stats.json` gets the receiver block and the gain.
    with_local: bool,
    /// The tuner gain to report, if known (upstream writes 99999.9 otherwise).
    gain_db: Option<f64>,
    /// When `outline.json` is next due (every 15 s, as upstream).
    next_outline: i64,
    /// When the command file is next looked for.
    next_command: i64,
    /// The last command-file problem reported, so a lasting one is logged once.
    command_problem: Option<String>,
}

const OUTLINE_EVERY_MS: i64 = 15_000;

/// Upstream's command file in the JSON directory: a script writes one command into it, the
/// decoder reads and deletes it (`echo resetRangeOutline > /run/rustadsb/setGain`).
pub const COMMAND_FILE: &str = "setGain";
/// How often the command file is looked for.
const COMMAND_EVERY_MS: i64 = 1_000;

/// True when a timer set to fire at `next`, `period` ms apart, is due at `now`. A `next` more
/// than one period ahead means the clock was set back: fire now rather than wait out the step.
pub fn due(now: i64, next: i64, period: i64) -> bool {
    now >= next || next - now > period
}

/// What looking at the command file found.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Nothing to do: no file, or an empty one (`echo x > setGain` creates the file before it
    /// writes, so an empty file is left for the next look).
    Nothing,
    /// A command, and the file is gone, so it runs once.
    Run(String),
    /// Something to report; no command runs.
    Problem(String),
}

/// Look for a command in `dir/setGain`: read it (upstream reads at most 127 bytes) and delete
/// the file. A command is only returned once the file is deleted, so it cannot run twice; only
/// regular files are opened (a FIFO would block the decoder).
pub fn read_command(dir: &Path) -> Command {
    let path = dir.join(COMMAND_FILE);
    match fs::metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Command::Nothing,
        Err(e) => return Command::Problem(format!("{}: {e}", path.display())),
        Ok(m) if !m.is_file() => {
            return Command::Problem(format!("{}: not a regular file; ignored", path.display()));
        }
        Ok(_) => {}
    }
    let mut buf = Vec::new();
    if let Err(e) = fs::File::open(&path).and_then(|f| f.take(127).read_to_end(&mut buf)) {
        return match e.kind() {
            io::ErrorKind::NotFound => Command::Nothing, // removed since we looked
            _ => Command::Problem(format!("{}: {e}", path.display())),
        };
    }
    if buf.is_empty() {
        return Command::Nothing;
    }
    match fs::remove_file(&path) {
        Ok(()) => Command::Run(String::from_utf8_lossy(&buf).trim().to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Command::Nothing, // someone else took it
        Err(e) => Command::Problem(format!(
            "{}: unable to delete it, so its command is not run: {e}",
            path.display()
        )),
    }
}

/// Write `contents` to `dir/name` via a temporary file and a rename, so readers never
/// see a half-written file.
pub fn write_atomic(dir: &Path, name: &str, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, dir.join(name))
}

/// Gzip `data` (level 1, as upstream's quick path).
pub fn gzip(data: &[u8]) -> io::Result<Vec<u8>> {
    gzip_level(data, 1)
}

/// Gzip `data` at the given level.
pub fn gzip_level(data: &[u8], level: u32) -> io::Result<Vec<u8>> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
    e.write_all(data)?;
    e.finish()
}

/// Compress `data` into one zstd frame (pure Rust encoder, fastest level).
pub fn zstd(data: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
}

impl JsonWriter {
    pub fn new(
        dir: &Path,
        interval_ms: i64,
        now: i64,
        tracker: &Tracker,
        nfix: u8,
        opts: JsonOptions,
    ) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let receiver = json::receiver_json(&ReceiverInfo {
            refresh_ms: interval_ms as u32,
            location: tracker.cfg.user_location,
            version: VERSION_STRING.to_string(),
            outline: tracker.outline.is_some(),
            reapi: opts.reapi,
            trace_interval_s: (opts.globe.tiles || opts.globe.traces)
                .then_some(opts.trace_interval_s),
            globe: opts.globe.tiles,
        });
        write_atomic(dir, "receiver.json", &receiver)?;
        let globe = if opts.globe.tiles || opts.globe.traces {
            Some(crate::globefiles::GlobeWriter::new(
                dir,
                opts.globe.clone(),
                VERSION_STRING,
            )?)
        } else {
            None
        };
        let (with_local, gain_db) = (opts.with_local, opts.gain_db);
        Ok(Self {
            opts,
            globe,
            rate_last: (now, tracker.stats.messages_total),
            message_rate: 0.0,
            dir: dir.to_path_buf(),
            interval_ms,
            next_aircraft: now,
            recorder: StatsRecorder::new(now),
            nfix,
            with_local,
            gain_db,
            next_outline: now,
            next_command: now,
            command_problem: None,
        })
    }

    /// Call as time advances; writes what is due.
    pub fn tick(
        &mut self,
        now: i64,
        tracker: &mut Tracker,
        pipe: &mut IqPipeline,
    ) -> io::Result<()> {
        let (processed, lost, dropped) =
            (pipe.samples(), pipe.samples_lost(), pipe.samples_dropped());
        let closed = self.recorder.update(
            now,
            &mut pipe.demod.stats,
            &mut tracker.stats,
            processed,
            lost,
            dropped,
        );
        if closed {
            self.write_stats(now, tracker, pipe)?;
        }
        if due(now, self.next_aircraft, self.interval_ms) {
            self.next_aircraft = now + self.interval_ms;
            self.write_aircraft(now, tracker, false)?;
        }
        if let Some(o) = &tracker.outline
            && due(now, self.next_outline, OUTLINE_EVERY_MS)
        {
            self.next_outline = now + OUTLINE_EVERY_MS;
            write_atomic(&self.dir, "outline.json", json::outline_json(o))?;
        }
        Ok(())
    }

    /// Write `outline.json` at the next tick instead of waiting up to 15 s (after a reset).
    pub fn outline_due_now(&mut self) {
        self.next_outline = i64::MIN;
    }

    /// The command waiting in the command file, if any; looks at most once a second.
    /// Problems with the file are reported once, not every second.
    pub fn take_command(&mut self, now: i64) -> Option<String> {
        if !due(now, self.next_command, COMMAND_EVERY_MS) {
            return None;
        }
        self.next_command = now + COMMAND_EVERY_MS;
        match read_command(&self.dir) {
            Command::Run(cmd) => {
                self.command_problem = None;
                Some(cmd)
            }
            Command::Nothing => {
                self.command_problem = None;
                None
            }
            Command::Problem(p) => {
                if self.command_problem.as_ref() != Some(&p) {
                    eprintln!("rustadsb: {p}");
                    self.command_problem = Some(p);
                }
                None
            }
        }
    }

    /// Final write at exit.
    pub fn finish(&mut self, now: i64, tracker: &mut Tracker, pipe: &IqPipeline) -> io::Result<()> {
        self.write_aircraft(now, tracker, true)?;
        if let Some(o) = &tracker.outline {
            write_atomic(&self.dir, "outline.json", json::outline_json(o))?;
        }
        self.write_stats(now, tracker, pipe)
    }

    /// `aircraft.json` (and `.gz`) and `aircraft.binCraft` (`.zst` always, `.gz` on request).
    fn write_aircraft(
        &mut self,
        now: i64,
        tracker: &mut Tracker,
        final_write: bool,
    ) -> io::Result<()> {
        let (t0, m0) = self.rate_last;
        if now - t0 >= 1000 {
            let msgs = tracker.stats.messages_total;
            self.message_rate = msgs.saturating_sub(m0) as f64 * 1000.0 / (now - t0) as f64;
            self.rate_last = (now, msgs);
        }
        if self.opts.bin_only < 2 {
            let doc = json::aircraft_json(tracker, now);
            if self.opts.gzip {
                write_atomic(&self.dir, "aircraft.json.gz", gzip(doc.as_bytes())?)?;
            }
            write_atomic(&self.dir, "aircraft.json", doc)?;
        }
        let bin = bincraft::aircraft_bin(tracker, now, self.message_rate, self.opts.hide_receiver);
        if self.opts.bin_gz {
            write_atomic(&self.dir, "aircraft.binCraft", gzip(&bin)?)?;
        }
        write_atomic(&self.dir, "aircraft.binCraft.zst", zstd(&bin))?;
        if let Some(g) = self.globe.as_mut() {
            g.tick(now, tracker, self.message_rate, final_write)?;
        }
        Ok(())
    }

    fn write_stats(&self, now: i64, tracker: &Tracker, pipe: &IqPipeline) -> io::Result<()> {
        let cum = Cumulative {
            demod: &pipe.demod.stats,
            track: &tracker.stats,
            samples_processed: pipe.samples(),
            samples_lost: pipe.samples_lost(),
            samples_dropped: pipe.samples_dropped(),
            counts: tracker.counts(now),
            gain_db: self.gain_db,
        };
        let s = json::stats_json(&self.recorder, now, &cum, self.nfix, self.with_local);
        write_atomic(&self.dir, "stats.json", &s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rustadsb-cmd-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_command_is_read_once_and_the_file_removed() {
        let dir = temp_dir("once");
        assert_eq!(read_command(&dir), Command::Nothing, "no file, no command");

        fs::write(dir.join(COMMAND_FILE), "resetRangeOutline\n").unwrap();
        assert_eq!(read_command(&dir), Command::Run("resetRangeOutline".into()));
        assert!(!dir.join(COMMAND_FILE).exists(), "the file is consumed");
        assert_eq!(read_command(&dir), Command::Nothing, "and not run twice");

        // like upstream, only the first 127 bytes count
        fs::write(dir.join(COMMAND_FILE), "x".repeat(500)).unwrap();
        assert!(matches!(read_command(&dir), Command::Run(c) if c.len() == 127));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_file_waits_for_its_writer() {
        // `echo cmd > setGain` creates the file, then writes: a look in between finds it empty
        let dir = temp_dir("empty");
        fs::write(dir.join(COMMAND_FILE), "").unwrap();
        assert_eq!(read_command(&dir), Command::Nothing);
        assert!(dir.join(COMMAND_FILE).exists(), "left for the next look");
        fs::write(dir.join(COMMAND_FILE), "resetRangeOutline").unwrap();
        assert_eq!(read_command(&dir), Command::Run("resetRangeOutline".into()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_regular_file_is_opened() {
        let dir = temp_dir("special");
        fs::create_dir(dir.join(COMMAND_FILE)).unwrap();
        assert!(
            matches!(read_command(&dir), Command::Problem(p) if p.contains("not a regular file"))
        );
        fs::remove_dir(dir.join(COMMAND_FILE)).unwrap();

        // a FIFO with no writer would block open() for good
        let fifo = dir.join(COMMAND_FILE);
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            assert!(matches!(read_command(&dir), Command::Problem(_)));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_cannot_be_deleted_is_not_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("stuck");
        fs::write(dir.join(COMMAND_FILE), "resetRangeOutline").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let deletable = fs::remove_file(dir.join(COMMAND_FILE)).is_ok(); // true as root
        if !deletable {
            for _ in 0..3 {
                assert!(
                    matches!(read_command(&dir), Command::Problem(p) if p.contains("unable to delete")),
                    "a command that stays in place would run every second"
                );
            }
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn timers_survive_the_clock_going_back() {
        assert!(!due(1_000, 2_000, 1_000), "not yet");
        assert!(due(2_000, 2_000, 1_000), "on time");
        assert!(due(2_500, 2_000, 1_000), "late");
        // set back an hour: the next firing (an hour ahead) would wait an hour
        assert!(due(2_000 - 3_600_000, 2_000, 1_000));
        assert!(due(0, i64::MIN, 15_000), "forced");
    }
}
