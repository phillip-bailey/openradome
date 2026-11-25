//! The decoder engine: owns the sample pipeline, the tracker and the outputs, and applies
//! upstream's rules for what gets displayed and forwarded.

use std::io::{self, Write};
use std::sync::mpsc::Receiver;

use rustadsb_core::beast::message_from_wire;
use rustadsb_core::json::StatsRecorder;
use rustadsb_core::mode_s::{DFTYPE_MODEAC, DataSource, ModesMessage};
use rustadsb_core::output::{beast_frame, raw_line};
use rustadsb_core::pipeline::IqPipeline;
use rustadsb_core::sbs::{SbsOptions, sbs_line};
use rustadsb_core::track::Tracker;

use crate::apiserver::Api;
use crate::cli::Cli;
use crate::dashboard::Dashboard;
use crate::interactive::Interactive;
use crate::jsonout::{COMMAND_FILE, JsonWriter};
use crate::net::{Hub, WireItem, now_ms};
use crate::state::StateWriter;

/// Counters for messages that came from the network (the `remote_*` statistics).
#[derive(Debug, Default, Clone)]
pub struct RemoteStats {
    pub modeac: u64,
    pub modes: u64,
    pub rejected_bad: u64,
    pub rejected_unknown_icao: u64,
    pub accepted: [u64; 3],
}

pub struct Engine<'a> {
    pub cli: &'a Cli,
    pub pipe: IqPipeline,
    pub tracker: Tracker,
    pub hub: Option<Hub>,
    pub json: Option<JsonWriter>,
    /// `--net-api-port`: the HTTP API.
    pub api: Option<Api>,
    /// `--interactive`: the aircraft table on the terminal.
    pub interactive: Option<Interactive>,
    /// `--write-state`: the range outline, kept across restarts.
    pub state: Option<StateWriter>,
    /// `--dashboard-port`: the status web page.
    pub dashboard: Option<Dashboard>,
    /// The 10 s statistics buckets behind `stats.json` and the dashboard (when either is on).
    pub recorder: Option<StatsRecorder>,
    pub net_rx: Option<Receiver<WireItem>>,
    pub remote: RemoteStats,
    out: io::BufWriter<io::Stdout>,
    filter_df: Option<u32>,
    /// No local SDR or file: only network input.
    pub net_only: bool,
    pub write_error: Option<io::Error>,
}

impl<'a> Engine<'a> {
    pub fn new(
        cli: &'a Cli,
        pipe: IqPipeline,
        tracker: Tracker,
        hub: Option<Hub>,
        net_only: bool,
    ) -> Self {
        Self {
            cli,
            pipe,
            tracker,
            hub,
            json: None,
            interactive: None,
            api: None,
            state: None,
            dashboard: None,
            recorder: None,
            net_rx: None,
            remote: RemoteStats::default(),
            out: io::BufWriter::new(io::stdout()),
            filter_df: cli.filter_df_bits(),
            net_only,
            write_error: None,
        }
    }

    /// Process one batch of messages: track them all, then forward them all.
    pub fn process_batch(&mut self, mut batch: Vec<ModesMessage>) {
        if let Some(a) = self.cli.process_only {
            batch.retain(|m| m.addr == a || m.maybe_addr == a);
        }
        let mut owners = Vec::with_capacity(batch.len());
        for mm in &mut batch {
            let ac = self.tracker.update_from_message(mm);
            owners.push(ac);
            // stdout display happens as part of tracking in upstream
            if self.wanted(mm)
                && self.write_error.is_none()
                && let Err(e) = self.write_message(mm)
            {
                self.write_error = Some(e);
            }
        }
        if self.hub.is_some() {
            for (mm, ac) in batch.iter().zip(owners) {
                self.forward(mm, ac);
            }
        }
        if let Some(hub) = &self.hub {
            hub.flush();
        }
        // Flush so `--raw` consumers see data promptly.
        if self.write_error.is_none()
            && let Err(e) = self.out.flush()
        {
            self.write_error = Some(e);
        }
    }

    fn wanted(&self, mm: &ModesMessage) -> bool {
        if mm.msgtype == DFTYPE_MODEAC && !self.cli.modeac {
            return false;
        }
        let show_only_hit = self
            .cli
            .show_only
            .is_some_and(|a| mm.addr == a || mm.maybe_addr == a);
        if self.cli.show_only.is_some() && !show_only_hit {
            return false;
        }
        if (self.cli.quiet || self.cli.stats) && !show_only_hit {
            return false;
        }
        if let Some(bits) = self.filter_df
            && bits & (1u32 << (mm.msgtype & 31)) == 0
        {
            return false;
        }
        true
    }

    fn write_message(&mut self, mm: &ModesMessage) -> io::Result<()> {
        if self.cli.onlyaddr {
            return writeln!(self.out, "{:06x}", mm.addr);
        }
        // upstream prints the corrected message in lower case hex on stdout
        let line = raw_line(mm, false, self.cli.net_verbatim).to_ascii_lowercase();
        let line = if self.cli.mlat && mm.timestamp != 0 {
            raw_line(mm, true, self.cli.net_verbatim)
        } else {
            line
        };
        self.out.write_all(line.as_bytes())?;
        if self.cli.raw {
            return Ok(());
        }
        writeln!(
            self.out,
            "hex: {}{:06x}  CRC: {:06x} fixed bits: {} decode: {}",
            if mm.addr & rustadsb_core::mode_s::NON_ICAO_ADDRESS != 0 {
                "~"
            } else {
                " "
            },
            mm.addr & 0xff_ffff,
            mm.crc,
            mm.corrected_bits,
            match mm.decode_result {
                r if r >= 0 => "ok",
                -1 => "reject unknown hex",
                _ => "reject bad",
            }
        )?;
        if mm.signal_level > 0.0 {
            writeln!(
                self.out,
                "RSSI: {:8.1} dBFS",
                10.0 * mm.signal_level.log10()
            )?;
        }
        if let Some(cs) = &mm.callsign {
            writeln!(self.out, "  Ident: {cs}")?;
        }
        if let Some(a) = mm.baro_alt {
            writeln!(self.out, "  Altitude: {a} ft barometric")?;
        }
        if let Some(sq) = mm.squawk {
            writeln!(self.out, "  Squawk: {:04x}", sq.hex)?;
        }
        writeln!(self.out)
    }

    /// Upstream's `outputMessage`: decide what goes to the network.
    fn forward(&self, mm: &ModesMessage, ac: Option<u32>) {
        let Some(hub) = &self.hub else { return };
        if let Some(bits) = self.filter_df
            && bits & (1u32 << (mm.msgtype & 31)) == 0
        {
            return;
        }
        if mm.decode_result < 0 {
            return;
        }
        let messages = ac.and_then(|a| self.tracker.get(a)).map(|a| a.messages);
        // Suppress the first message from an aircraft when using an SDR, unless its CRC was
        // explicit and nothing had to be fixed.
        let ok = self.net_only
            || self.cli.net_verbatim
            || messages.is_some_and(|m| m > 1)
            || (mm.crc == 0 && mm.corrected_bits == 0)
            || mm.msgtype == DFTYPE_MODEAC;
        if !ok {
            return;
        }
        let is_mlat = mm.source == DataSource::Mlat;
        let verbatim = self.cli.net_verbatim;

        if let (Some(addr), true) = (ac, hub.sbs_clients() > 0)
            && (!is_mlat || self.cli.forward_mlat_sbs)
        {
            let delta = self
                .tracker
                .get(addr)
                .filter(|a| a.geom_delta_valid.valid())
                .map(|a| a.geom_delta);
            let opts = SbsOptions {
                use_gnss: self.cli.gnss,
                extra_fields: false,
            };
            if let Some(line) = sbs_line(mm, mm.addr, delta, now_ms(), opts) {
                hub.send_sbs(line.into_bytes());
            }
        }
        if !is_mlat && (verbatim || mm.corrected_bits < 2) && hub.raw_clients() > 0 {
            hub.send_raw(raw_line(mm, self.cli.mlat, verbatim).into_bytes());
        }
        if (!is_mlat || self.cli.forward_mlat) && (mm.corrected_bits < 2 || verbatim) {
            let want_beast = hub.beast_clients() > 0;
            // beast_reduce carries only the messages the tracker flagged as news
            let want_reduce = mm.reduce_forward && hub.beast_reduce_clients() > 0;
            if (want_beast || want_reduce)
                && let Some(frame) = beast_frame(mm, verbatim)
            {
                if want_beast {
                    hub.send_beast(frame.clone());
                }
                if want_reduce {
                    hub.send_beast_reduce(frame);
                }
            }
        }
    }

    /// Decode and process everything waiting from the network.
    pub fn drain_net(&mut self) {
        let Some(rx) = self.net_rx.take() else { return };
        let mut batch = Vec::new();
        while let Ok(item) = rx.try_recv() {
            let Some(mm) = message_from_wire(
                &mut self.pipe.demod.decoder,
                &item.data,
                item.timestamp,
                item.signal,
                item.now,
                item.receiver_id,
                self.cli.modeac,
            ) else {
                continue;
            };
            if mm.msgtype == DFTYPE_MODEAC {
                self.remote.modeac += 1;
            } else {
                self.remote.modes += 1;
                match mm.decode_result {
                    -1 => self.remote.rejected_unknown_icao += 1,
                    r if r < 0 => self.remote.rejected_bad += 1,
                    _ => self.remote.accepted[(mm.corrected_bits as usize).min(2)] += 1,
                }
            }
            batch.push(mm);
            if batch.len() >= 256 {
                self.process_batch(std::mem::take(&mut batch));
            }
        }
        if !batch.is_empty() {
            self.process_batch(batch);
        }
        self.net_rx = Some(rx);
    }

    /// Time-driven housekeeping: expiry, the command file, JSON files and saved state.
    pub fn tick(&mut self, now: i64) -> Result<(), String> {
        self.tracker.periodic(now);
        if let Some(cmd) = self.json.as_mut().and_then(|w| w.take_command(now)) {
            self.run_command(&cmd);
        }
        let mut closed = false;
        if let Some(r) = self.recorder.as_mut() {
            let (processed, lost, dropped) = (
                self.pipe.samples(),
                self.pipe.samples_lost(),
                self.pipe.samples_dropped(),
            );
            closed = r.update(
                now,
                &mut self.pipe.demod.stats,
                &mut self.tracker.stats,
                processed,
                lost,
                dropped,
            );
        }
        if let (Some(w), Some(r)) = (self.json.as_mut(), self.recorder.as_ref()) {
            w.tick(now, &mut self.tracker, &self.pipe, r, closed)
                .map_err(|e| format!("json: {e}"))?;
        }
        if let Some(d) = self.dashboard.as_mut() {
            let bucket = if closed {
                self.recorder.as_ref().and_then(|r| r.last())
            } else {
                None
            };
            d.tick(now, &self.tracker, bucket);
        }
        if let (Some(s), Some(o)) = (self.state.as_mut(), self.tracker.outline.as_ref()) {
            s.tick(now, o);
        }
        if let Some(api) = self.api.as_mut() {
            api.tick(now, &self.tracker);
        }
        if let Some(ui) = self.interactive.as_mut() {
            ui.show(now, &self.tracker);
        }
        Ok(())
    }

    /// Upstream's command file: of its commands only `resetRangeOutline` is supported (gain
    /// and position changes need a restart with new options).
    fn run_command(&mut self, cmd: &str) {
        if !cmd.to_ascii_lowercase().contains("resetrangeoutline") {
            eprintln!(
                "rustadsb: {COMMAND_FILE}: only resetRangeOutline is supported; ignored {cmd:?}"
            );
            return;
        }
        let Some(outline) = self.tracker.outline.as_mut() else {
            eprintln!("rustadsb: resetRangeOutline: there is no range outline (needs --lat/--lon)");
            return;
        };
        eprintln!("resetting range outline");
        outline.reset();
        if let Some(w) = self.json.as_mut() {
            w.outline_due_now();
        }
        // saved at once (this same tick, just after), so a restart cannot bring the old one back
        if let Some(s) = self.state.as_mut() {
            s.save_due_now();
        }
    }

    pub fn finish(&mut self, now: i64) -> Result<(), String> {
        if let (Some(w), Some(r)) = (self.json.as_mut(), self.recorder.as_ref()) {
            w.finish(now, &mut self.tracker, &self.pipe, r)
                .map_err(|e| format!("json: {e}"))?;
        }
        if let (Some(s), Some(o)) = (self.state.as_ref(), self.tracker.outline.as_ref()) {
            s.save_or_report(o);
        }
        if let Err(e) = self.out.flush()
            && e.kind() != io::ErrorKind::BrokenPipe
        {
            return Err(e.to_string());
        }
        Ok(())
    }
}
