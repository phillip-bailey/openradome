//! State kept across restarts (`--write-state <dir>`), as upstream: the range outline in
//! `<dir>/internal_state/rangeDirs.gz`, in upstream's format, so openradome and upstream can each
//! pick up the other's file.
//!
//! Upstream also saves its aircraft there (`blob_*.zstl`, dumps of its C structures, used for
//! its trace history). openradome keeps no traces and rebuilds its aircraft table from live
//! messages within seconds, so it neither reads nor writes those.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use openradome_core::track::RangeOutline;

const RANGE_DIRS: &str = "rangeDirs.gz";
/// How often the outline is saved: upstream's 5 minutes, or 6 hours with
/// `--write-state-only-on-exit` (it is saved at exit either way).
const EVERY_MS: i64 = 5 * 60_000;
const ONLY_ON_EXIT_EVERY_MS: i64 = 6 * 3_600_000;

pub struct StateWriter {
    dir: PathBuf,
    every_ms: i64,
    next_write: i64,
}

impl StateWriter {
    /// `parent` is the `--write-state` directory; state goes in its `internal_state`.
    pub fn new(parent: &Path, only_on_exit: bool, now: i64) -> io::Result<Self> {
        let dir = parent.join("internal_state");
        fs::create_dir_all(&dir)?;
        let every_ms = if only_on_exit {
            ONLY_ON_EXIT_EVERY_MS
        } else {
            EVERY_MS
        };
        Ok(Self {
            dir,
            every_ms,
            next_write: now + every_ms,
        })
    }

    /// Restore the outline from a previous run, if there is one. A missing file is normal
    /// (first start); a broken one is reported and ignored.
    pub fn load(&self, outline: &mut RangeOutline, now: i64) {
        let path = self.dir.join(RANGE_DIRS);
        let saved_at = match fs::metadata(&path) {
            Ok(m) => m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(now, |d| d.as_millis() as i64),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => {
                eprintln!("openradome: state: {}: {e}", path.display());
                return;
            }
        };
        let mut bytes = Vec::with_capacity(RangeOutline::STATE_LEN);
        let read = fs::File::open(&path).and_then(|f| {
            GzDecoder::new(f)
                .take(RangeOutline::STATE_LEN as u64 + 1)
                .read_to_end(&mut bytes)
        });
        match read.map_err(|e| e.to_string()).and_then(|_| {
            outline
                .load_state_bytes(&bytes, saved_at, now)
                .map_err(str::to_string)
        }) {
            Ok(()) => eprintln!("actual range outline, read bytes: {}", bytes.len()),
            Err(e) => eprintln!("openradome: state: {} not used: {e}", path.display()),
        }
    }

    /// Call as time advances; saves when due. A failed save is reported, not fatal: the
    /// outline is not worth stopping the feed for.
    pub fn tick(&mut self, now: i64, outline: &RangeOutline) {
        if !crate::jsonout::due(now, self.next_write, self.every_ms) {
            return;
        }
        self.next_write = now + self.every_ms;
        self.save_or_report(outline);
    }

    /// Save at the next tick (after a reset), not inline where the caller is.
    pub fn save_due_now(&mut self) {
        self.next_write = i64::MIN;
    }

    /// Save now (at exit), reporting a failure.
    pub fn save_or_report(&self, outline: &RangeOutline) {
        if let Err(e) = self.save(outline) {
            eprintln!("openradome: state: saving the range outline failed: {e}");
        }
    }

    /// Save now.
    pub fn save(&self, outline: &RangeOutline) -> io::Result<()> {
        // via a temporary file and a rename, so a crash mid-write keeps the previous state
        let tmp = self.dir.join(format!("{RANGE_DIRS}.tmp"));
        let mut gz = GzEncoder::new(fs::File::create(&tmp)?, Compression::default());
        gz.write_all(&outline.to_state_bytes())?;
        gz.finish()?.sync_all()?;
        fs::rename(&tmp, self.dir.join(RANGE_DIRS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openradome_core::track::RANGEDIRS_IVALS;

    fn temp_dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("openradome-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn saves_and_restores_the_outline() {
        let day = 24 * 3_600_000;
        // the real clock: loading dates the file by its modification time
        let real_now = || {
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
        };
        let now = real_now();
        let parent = temp_dir("roundtrip");
        let mut w = StateWriter::new(&parent, false, now).unwrap();

        // nothing saved yet: the outline stays empty, no error
        let mut o = RangeOutline::new(day);
        w.load(&mut o, now);
        assert!(o.points().is_empty());

        // fill it through its own state format (no aircraft needed here)
        let mut bytes = RangeOutline::new(day).to_state_bytes();
        let slice = ((now * (RANGEDIRS_IVALS as i64 - 1) / day) % RANGEDIRS_IVALS as i64) as usize;
        bytes[..4].copy_from_slice(&(slice as i32).to_le_bytes());
        let at = 4 + (slice * 360 + 45) * 16;
        bytes[at..at + 4].copy_from_slice(&100_000f32.to_le_bytes());
        bytes[at + 4..at + 8].copy_from_slice(&51.9f32.to_le_bytes());
        bytes[at + 8..at + 12].copy_from_slice(&1.6f32.to_le_bytes());
        bytes[at + 12..at + 16].copy_from_slice(&37000i32.to_le_bytes());
        o.load_state_bytes(&bytes, now, now).unwrap();

        // not due yet, then due after 5 minutes
        w.tick(now + 1_000, &o);
        assert!(!parent.join("internal_state/rangeDirs.gz").exists());
        w.tick(now + EVERY_MS, &o);
        let path = parent.join("internal_state/rangeDirs.gz");
        assert!(path.exists());
        assert!(!parent.join("internal_state/rangeDirs.gz.tmp").exists());

        // a new run picks it up
        let mut fresh = RangeOutline::new(day);
        w.load(&mut fresh, real_now());
        let p = fresh.points();
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].alt, 37000);
        assert!((p[0].lat - 51.9).abs() < 1e-5);

        // the file is plain gzip of upstream's layout
        let mut raw = Vec::new();
        GzDecoder::new(fs::File::open(&path).unwrap())
            .read_to_end(&mut raw)
            .unwrap();
        assert_eq!(raw.len(), RangeOutline::STATE_LEN);

        // a broken file is ignored and leaves the outline empty
        fs::write(&path, b"not gzip").unwrap();
        let mut broken = RangeOutline::new(day);
        w.load(&mut broken, real_now());
        assert!(broken.points().is_empty());

        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn only_on_exit_saves_every_six_hours() {
        let parent = temp_dir("onexit");
        let now = 1_000_000;
        let mut w = StateWriter::new(&parent, true, now).unwrap();
        let o = RangeOutline::new(24 * 3_600_000);
        w.tick(now + EVERY_MS, &o);
        assert!(!parent.join("internal_state/rangeDirs.gz").exists());
        w.tick(now + ONLY_ON_EXIT_EVERY_MS, &o);
        assert!(parent.join("internal_state/rangeDirs.gz").exists());
        let _ = fs::remove_dir_all(&parent);
    }
}
