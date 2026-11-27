//! Periodic JSON files (`--write-json <dir>`), written atomically.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use openradome_core::bincraft;
use openradome_core::json::{self, Cumulative, ReceiverInfo, StatsRecorder};
use openradome_core::pipeline::IqPipeline;
use openradome_core::track::Tracker;

pub const VERSION_STRING: &str = concat!("openradome ", env!("CARGO_PKG_VERSION"));

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
/// decoder reads and deletes it (`echo resetRangeOutline > /run/openradome/setGain`).
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
///
/// The temporary file has a random name and is created only if nothing is there yet (`O_EXCL`):
/// with a fixed name, anyone who can write to `dir` could put a symlink there and have the decoder
/// overwrite the file it points to. Something already at a name just means another try.
pub fn write_atomic(dir: &Path, name: &str, contents: impl AsRef<[u8]>) -> io::Result<()> {
    use std::hash::{BuildHasher, Hasher};
    use std::io::Write;
    let mut last = None;
    for _ in 0..8 {
        // a fresh RandomState gives a different, unpredictable value each time
        let r = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        let tmp = dir.join(format!(".{name}.{r:016x}.tmp"));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp);
        let mut file = match file {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                last = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        let written = file.write_all(contents.as_ref()).and_then(|()| {
            drop(file);
            fs::rename(&tmp, dir.join(name))
        });
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return written;
    }
    Err(last.unwrap_or_else(|| io::Error::other("no temporary file name was free")))
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
            nfix,
            with_local,
            gain_db,
            next_outline: now,
            next_command: now,
            command_problem: None,
        })
    }

    /// Call as time advances; writes what is due. `stats_closed`: the recorder has just closed a
    /// 10 s bucket, so `stats.json` is due.
    pub fn tick(
        &mut self,
        now: i64,
        tracker: &mut Tracker,
        pipe: &IqPipeline,
        recorder: &StatsRecorder,
        stats_closed: bool,
    ) -> io::Result<()> {
        if stats_closed {
            self.write_stats(now, tracker, pipe, recorder)?;
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
                    eprintln!("openradome: {p}");
                    self.command_problem = Some(p);
                }
                None
            }
        }
    }

    /// Final write at exit.
    pub fn finish(
        &mut self,
        now: i64,
        tracker: &mut Tracker,
        pipe: &IqPipeline,
        recorder: &StatsRecorder,
    ) -> io::Result<()> {
        self.write_aircraft(now, tracker, true)?;
        if let Some(o) = &tracker.outline {
            write_atomic(&self.dir, "outline.json", json::outline_json(o))?;
        }
        self.write_stats(now, tracker, pipe, recorder)
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

    fn write_stats(
        &self,
        now: i64,
        tracker: &Tracker,
        pipe: &IqPipeline,
        recorder: &StatsRecorder,
    ) -> io::Result<()> {
        let cum = Cumulative {
            demod: &pipe.demod.stats,
            track: &tracker.stats,
            samples_processed: pipe.samples(),
            samples_lost: pipe.samples_lost(),
            samples_dropped: pipe.samples_dropped(),
            counts: tracker.counts(now),
            gain_db: self.gain_db,
        };
        let s = json::stats_json(recorder, now, &cum, self.nfix, self.with_local);
        write_atomic(&self.dir, "stats.json", &s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("openradome-cmd-{name}-{}", std::process::id()));
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

    fn tmp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn a_reader_only_ever_sees_whole_files() {
        let dir = temp_dir("atomic");
        let big = |c: u8| vec![c; 256 * 1024];
        write_atomic(&dir, "aircraft.json", big(b'a')).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let (dir, stop) = (dir.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut reads = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let got = fs::read(dir.join("aircraft.json")).unwrap();
                    assert_eq!(got.len(), 256 * 1024, "a partial file");
                    assert!(got.iter().all(|&c| c == got[0]), "two versions mixed");
                    reads += 1;
                }
                reads
            })
        };
        for i in 0..200 {
            write_atomic(
                &dir,
                "aircraft.json",
                big(if i % 2 == 0 { b'b' } else { b'a' }),
            )
            .unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
        assert!(
            tmp_files(&dir).is_empty(),
            "no temporary files left: {:?}",
            tmp_files(&dir)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_not_followed() {
        let dir = temp_dir("symlink");
        let victim = dir.join("history.sqlite");
        fs::write(&victim, "precious").unwrap();
        // where the old fixed-name temporary file went
        std::os::unix::fs::symlink(&victim, dir.join("aircraft.json.tmp")).unwrap();
        write_atomic(&dir, "aircraft.json", "{}").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        assert_eq!(fs::read_to_string(dir.join("aircraft.json")).unwrap(), "{}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_leaves_the_old_file_and_no_temporary_one() {
        let dir = temp_dir("fail");
        write_atomic(&dir, "stats.json", "old").unwrap();
        // the target is a directory: the rename fails after the temporary file is written
        fs::create_dir(dir.join("outline.json")).unwrap();
        assert!(write_atomic(&dir, "outline.json", "new").is_err());
        assert!(tmp_files(&dir).is_empty(), "{:?}", tmp_files(&dir));
        assert!(write_atomic(&dir.join("missing"), "x.json", "y").is_err());
        assert_eq!(fs::read_to_string(dir.join("stats.json")).unwrap(), "old");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The whole writer: what `--write-json` leaves in its directory as time goes on.
    #[test]
    fn the_writer_writes_every_file_when_due() {
        use openradome_core::convert::InputFormat;
        use openradome_core::demod::Demodulator;
        use openradome_core::mode_s::{Decoder, ModesMessage};
        use openradome_core::track::{RangeOutline, TrackerConfig};

        let dir = temp_dir("writer");
        const T0: i64 = 1_700_000_000_000;
        let mut tracker = Tracker::new(TrackerConfig::new(Some(52.0), Some(4.0), None));
        tracker.outline = Some(RangeOutline::new(24 * 3_600_000));
        let pipe = IqPipeline::new(Demodulator::new(Decoder::new(1)), InputFormat::Uc8, 1024);
        let mut recorder = StatsRecorder::new(T0);
        let opts = JsonOptions {
            gzip: true,
            bin_gz: true,
            with_local: true,
            gain_db: Some(29.7),
            ..JsonOptions::default()
        };
        let mut w = JsonWriter::new(&dir, 1_000, T0, &tracker, 1, opts).unwrap();
        let receiver: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("receiver.json")).unwrap()).unwrap();
        assert_eq!(receiver["lat"], 52.0);
        assert_eq!(receiver["outlineJson"], true);

        // an aircraft: a position pair and an identification, each heard twice
        let mut d = Decoder::new(1);
        let frames = [
            "8D40621D58C382D690C8AC2863A7",
            "8D40621D58C386435CC412692AD6",
            "8D4840D6202CC371C32CE0576098",
            "8D4840D6202CC371C32CE0576098",
        ];
        for (i, f) in frames.iter().enumerate() {
            let bytes: Vec<u8> = (0..f.len() / 2)
                .map(|j| u8::from_str_radix(&f[2 * j..2 * j + 2], 16).unwrap())
                .collect();
            let mut mm = ModesMessage::from_bytes(&bytes);
            mm.sys_timestamp = T0 + 100 * i as i64;
            mm.signal_level = 0.01;
            d.decode(&mut mm);
            tracker.update_from_message(&mut mm);
        }
        let now = T0 + 1_000;
        w.tick(now, &mut tracker, &pipe, &recorder, false).unwrap();
        let aircraft: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("aircraft.json")).unwrap()).unwrap();
        let list = aircraft["aircraft"].as_array().unwrap();
        assert!(
            list.iter()
                .any(|a| a["flight"].as_str().is_some_and(|f| f.trim() == "KLM1023"))
        );
        let mut unzipped = Vec::new();
        flate2::read::GzDecoder::new(fs::File::open(dir.join("aircraft.json.gz")).unwrap())
            .read_to_end(&mut unzipped)
            .unwrap();
        assert_eq!(
            unzipped,
            fs::read(dir.join("aircraft.json")).unwrap(),
            "the .gz is the same file"
        );
        let mut bin = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(
            fs::File::open(dir.join("aircraft.binCraft.zst")).unwrap(),
        )
        .unwrap()
        .read_to_end(&mut bin)
        .unwrap();
        assert_eq!(bin.len() % bincraft::RECORD_SIZE, 0);
        assert!(
            bin.len() / bincraft::RECORD_SIZE > list.len(),
            "a header and every aircraft"
        );
        assert!(
            dir.join("aircraft.binCraft").exists(),
            "--write-binCraft-old"
        );
        assert!(
            dir.join("outline.json").exists(),
            "the outline is due at once"
        );
        assert!(
            !dir.join("stats.json").exists(),
            "stats.json waits for a closed bucket"
        );

        // a closed 10 s bucket: stats.json
        let mut demod = pipe.demod.stats.clone();
        let mut track = tracker.stats.clone();
        assert!(recorder.update(T0 + 10_000, &mut demod, &mut track, 0, 0, 0));
        w.tick(T0 + 10_000, &mut tracker, &pipe, &recorder, true)
            .unwrap();
        let stats: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("stats.json")).unwrap()).unwrap();
        assert_eq!(stats["gain_db"], 29.7);
        assert!(stats["last1min"]["messages"].as_u64().is_some());

        // the command file: looked at once a second, run once
        fs::write(dir.join(COMMAND_FILE), "resetRangeOutline\n").unwrap();
        assert_eq!(
            w.take_command(T0 + 10_000).as_deref(),
            Some("resetRangeOutline")
        );
        fs::write(dir.join(COMMAND_FILE), "resetRangeOutline\n").unwrap();
        assert_eq!(
            w.take_command(T0 + 10_500),
            None,
            "not before a second has passed"
        );
        assert_eq!(
            w.take_command(T0 + 11_000).as_deref(),
            Some("resetRangeOutline")
        );
        assert_eq!(w.take_command(T0 + 12_000), None, "and only once");

        // at exit: everything once more
        fs::remove_file(dir.join("aircraft.json")).unwrap();
        w.finish(T0 + 12_000, &mut tracker, &pipe, &recorder)
            .unwrap();
        assert!(dir.join("aircraft.json").exists());
        assert!(tmp_files(&dir).is_empty(), "{:?}", tmp_files(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bincraft_only_leaves_out_aircraft_json() {
        use openradome_core::track::TrackerConfig;
        let dir = temp_dir("binonly");
        let mut tracker = Tracker::new(TrackerConfig::new(None, None, None));
        let pipe = IqPipeline::new(
            openradome_core::demod::Demodulator::new(openradome_core::mode_s::Decoder::new(1)),
            openradome_core::convert::InputFormat::Uc8,
            1024,
        );
        let recorder = StatsRecorder::new(0);
        let opts = JsonOptions {
            bin_only: 2,
            ..JsonOptions::default()
        };
        let mut w = JsonWriter::new(&dir, 1_000, 0, &tracker, 1, opts).unwrap();
        w.tick(1_000, &mut tracker, &pipe, &recorder, false)
            .unwrap();
        assert!(!dir.join("aircraft.json").exists());
        assert!(dir.join("aircraft.binCraft.zst").exists());
        assert!(
            !dir.join("outline.json").exists(),
            "no outline without a position"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
