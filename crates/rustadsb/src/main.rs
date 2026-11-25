mod apiserver;
mod cli;
mod dashboard;
mod engine;
mod globefiles;
mod interactive;
mod jsonout;
mod net;
mod sdr;
mod state;

use std::fs::File;
use std::io::{self, Read};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;
use cli::{Cli, DeviceType};
use engine::Engine;
use rustadsb_core::convert::InputFormat;
use rustadsb_core::demod::{Demodulator, SAMPLE_RATE};
use rustadsb_core::mode_s::Decoder;
use rustadsb_core::pipeline::IqPipeline;
use rustadsb_core::track::{Tracker, TrackerConfig};

/// Upstream's `viewadsb` mode: run under that name, the program is a terminal viewer that connects
/// to a decoder's Beast output (127.0.0.1:30005 unless told otherwise).
fn viewadsb_args(args: Vec<String>) -> Vec<String> {
    let name = args
        .first()
        .map(|a| a.rsplit(['/', '\\']).next().unwrap_or(a).to_string());
    if !name.is_some_and(|n| n.contains("viewadsb")) {
        return args;
    }
    let has = |flag: &str| {
        args.iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    };
    let mut out = vec![
        args[0].clone(),
        "--net".into(),
        "--net-only".into(),
        "--interactive".into(),
    ];
    if !has("--net-connector") {
        out.extend(["--net-connector".into(), "127.0.0.1,30005,beast_in".into()]);
    }
    if !has("--net-connector-delay") {
        out.extend(["--net-connector-delay".into(), "5".into()]);
    }
    out.extend(args.into_iter().skip(1));
    out
}

fn main() -> ExitCode {
    let mut cli = Cli::parse_from(viewadsb_args(std::env::args().collect()));
    if cli.no_interactive {
        cli.interactive = false;
    }
    if cli.interactive {
        // the table owns the screen: no message output, and files play at capture speed
        cli.quiet = true;
        cli.throttle = true;
    }
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rustadsb: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let device = match (cli.device_type, &cli.ifile) {
        (Some(d), _) => d,
        (None, Some(_)) => DeviceType::Ifile,
        (None, None) => DeviceType::None,
    };
    if cli.json_location_accuracy != 2 {
        eprintln!("rustadsb: --json-location-accuracy other than 2 is not implemented; using 2");
    }
    if device == DeviceType::None && !cli.network_enabled() {
        return Err("no input: give --ifile <path>, or enable networking with --net".into());
    }

    // Tokio runs the network side and signal handling; the engine runs on this thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("can't start the async runtime: {e}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        runtime.spawn(async move {
            wait_for_signal().await;
            stop.store(true, Ordering::SeqCst);
        });
    }

    let mut decoder = Decoder::new(cli.nfix_crc());
    decoder.fix_df = !cli.no_fix_df;
    // Anything that shows messages on stdout decodes rejected ones too, like upstream.
    decoder.decode_all = !cli.quiet || cli.show_only.is_some();
    let mut demod = Demodulator::new(decoder);
    if let Some(t) = cli.preamble_threshold() {
        demod.preamble_threshold = t;
    }
    let format = cli.iformat.unwrap_or(InputFormat::Uc8);
    let block = (cli.sdr_buffer_size.max(1) * 1024) / 2;
    let mut pipe = IqPipeline::new(demod, format, block);
    pipe.mode_ac = cli.modeac;
    pipe.startup_time = net::now_ms();

    let mut track_cfg = TrackerConfig::new(cli.lat, cli.lon, cli.max_range);
    // upstream caps the beast-reduce interval at 15 s
    track_cfg.beast_reduce_interval =
        (cli.net_beast_reduce_interval.clamp(0.0, 15.0) * 1000.0) as i64;
    // the globe index and history keep traces; both default to a stricter position reliability
    let globe_index = cli.write_json_globe_index;
    track_cfg.globe_index = globe_index;
    track_cfg.write_traces = globe_index || cli.write_globe_history.is_some();
    track_cfg.message_rate_mode = globe_index || cli.api_shutdown_delay > 0.0;
    if let Some(i) = cli.json_trace_interval {
        track_cfg.trace.interval_ms = ((i * 1000.0) as i64).max(1);
    }
    if let Some(r) = cli.json_reliable {
        track_cfg.json_reliable = r.clamp(-1, 4);
    } else if track_cfg.write_traces {
        track_cfg.json_reliable = track_cfg.json_reliable.max(2);
    }
    track_cfg.position_persistence = track_cfg
        .position_persistence
        .max(track_cfg.json_reliable.max(0));
    let mut tracker = Tracker::new(track_cfg);
    // outline.json, as upstream: needs JSON output and a receiver location that is shown
    if cli.write_json.is_some()
        && tracker.cfg.user_location.is_some()
        && cli.json_location_accuracy != 0
    {
        let hours = cli.range_outline_hours.unwrap_or(24.0);
        tracker.outline = Some(upstream_outline(hours));
    }
    let state = open_state(cli, &mut tracker);

    // Network
    let mut hub = None;
    let mut net_rx = None;
    if cli.network_enabled() {
        let cfg = net_config(cli)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(65_536);
        let h = net::Hub::with_flush_size(cli.net_ro_size.unwrap_or(net::FLUSH_SIZE));
        net::spawn(runtime.handle(), &cfg, &h, &tx).map_err(|e| e.to_string())?;
        hub = Some(h);
        net_rx = Some(rx);
    }

    let net_only = device == DeviceType::None;
    let startup = pipe.startup_time;
    let mut engine = Engine::new(cli, pipe, tracker, hub, net_only);
    if cli.interactive {
        let mut ui = interactive::Interactive::new(
            (cli.interactive_ttl * 1000.0) as i64,
            cli.metric,
            cli.gnss,
            engine.tracker.cfg.user_location.is_some(),
        );
        ui.start();
        engine.interactive = Some(ui);
    }
    engine.net_rx = net_rx;
    engine.state = state;
    if let Some(port) = cli.net_api_port {
        let bind = cli
            .net_bind_address
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        engine.api = Some(
            apiserver::Api::start(
                runtime.handle(),
                bind,
                port,
                (cli.write_json_every.max(0.05) * 1000.0) as i64,
                startup,
                jsonout::VERSION_STRING,
            )
            .map_err(|e| format!("can't listen for the API on port {port}: {e}"))?,
        );
    }
    if cli.write_json.is_some() || cli.dashboard_port.is_some() {
        engine.recorder = Some(rustadsb_core::json::StatsRecorder::new(startup));
    }
    if let Some(port) = cli.dashboard_port {
        let bind = cli
            .net_bind_address
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        engine.dashboard = Some(
            dashboard::Dashboard::start(
                runtime.handle(),
                bind,
                port,
                startup,
                !net_only,
                jsonout::VERSION_STRING,
            )
            .map_err(|e| format!("can't listen for the dashboard on port {port}: {e}"))?,
        );
    }
    if let Some(dir) = &cli.write_json {
        engine.json = Some(
            jsonout::JsonWriter::new(
                dir,
                (cli.write_json_every.max(0.05) * 1000.0) as i64,
                startup,
                &engine.tracker,
                cli.nfix_crc(),
                jsonout::JsonOptions {
                    reapi: cli.tar1090_use_api,
                    trace_interval_s: engine.tracker.cfg.trace.interval_ms as f64 / 1000.0,
                    globe: globefiles::Options {
                        tiles: globe_index,
                        traces: engine.tracker.cfg.write_traces,
                        bin_gz: cli.write_bincraft_old,
                        hist_only: cli.json_trace_hist_only.unwrap_or(0),
                        history_dir: cli.write_globe_history.clone(),
                        full_trace_dir: cli.full_trace_dir.clone(),
                    },
                    with_local: !net_only,
                    gain_db: (device == DeviceType::Rtlsdr)
                        .then(|| sdr::reported_gain(cli.gain))
                        .flatten(),
                    gzip: cli.write_json_gzip,
                    bin_only: cli.write_json_bincraft_only.unwrap_or(0),
                    bin_gz: cli.write_bincraft_old,
                    hide_receiver: cli.json_location_accuracy == 0,
                },
            )
            .map_err(|e| format!("can't write json to {}: {e}", dir.display()))?,
        );
    }

    let deadline = cli
        .auto_exit
        .map(|s| Instant::now() + Duration::from_secs_f64(s));
    // An input that stops with an error (a failed dongle, say) still gets the normal wind-down:
    // final statistics and a bounded runtime shutdown. The error is returned afterwards.
    let input = match device {
        DeviceType::Ifile => run_ifile(cli, &mut engine, &stop, deadline),
        DeviceType::Rtlsdr => run_rtlsdr(cli, &mut engine, &stop, deadline),
        _ => run_net_only(&mut engine, &stop, deadline),
    };

    if cli.stats {
        print_stats(&engine, cli.nfix_crc());
    }
    if let Some(api) = &engine.api
        && cli.api_shutdown_delay > 0.0
    {
        // keep answering queries while a load balancer notices ?status is 503
        api.begin_shutdown();
        std::thread::sleep(Duration::from_secs_f64(cli.api_shutdown_delay));
    }
    drop(engine);
    runtime.shutdown_timeout(Duration::from_millis(200));
    input
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            return;
        };
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn net_config(cli: &Cli) -> Result<net::NetConfig, String> {
    let ports = |s: &String| net::parse_ports(s);
    let connectors = cli
        .net_connector
        .iter()
        .map(|c| net::Connector::parse(c))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(net::NetConfig {
        bind: cli
            .net_bind_address
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
        raw_out: ports(&cli.net_ro_port)?,
        beast_out: ports(&cli.net_bo_port)?,
        sbs_out: ports(&cli.net_sbs_port)?,
        raw_in: ports(&cli.net_ri_port)?,
        beast_in: ports(&cli.net_bi_port)?,
        beast_reduce_out: ports(&cli.net_beast_reduce_out_port)?,
        connectors,
        uuid: match (&cli.uuid, &cli.uuid_file) {
            (Some(u), _) => Some(u.clone()),
            // like upstream, a bad UUID file is reported but never stops the program
            (None, Some(path)) => match std::fs::read_to_string(path) {
                Ok(u) => Some(u.trim().to_string()),
                Err(e) => {
                    eprintln!(
                        "rustadsb: --uuid-file {} could not be read: {e}",
                        path.display()
                    );
                    None
                }
            },
            (None, None) => None,
        },
        heartbeat: Duration::from_secs_f64(cli.net_heartbeat.max(0.0)),
        connector_delay: Duration::from_secs_f64(cli.net_connector_delay.max(0.1)),
    })
}

/// `--write-state`: restore the range outline from the last run and keep saving it. Like
/// upstream, a state directory that can't be used is reported and the decoder runs without it.
fn open_state(cli: &Cli, tracker: &mut Tracker) -> Option<state::StateWriter> {
    let dir = cli.write_state.as_ref()?;
    let Some(outline) = tracker.outline.as_mut() else {
        eprintln!(
            "rustadsb: --write-state: nothing to keep; the range outline needs --write-json and \
             a receiver location (--lat/--lon)"
        );
        return None;
    };
    let now = net::now_ms();
    match state::StateWriter::new(dir, cli.write_state_only_on_exit, now) {
        Ok(w) => {
            w.load(outline, now);
            Some(w)
        }
        Err(e) => {
            eprintln!(
                "rustadsb: --write-state: unable to create state directory ({}): {e}",
                dir.display()
            );
            None
        }
    }
}

/// The range outline for `--range-outline-hours` (upstream's minimum is one minute).
fn upstream_outline(hours: f64) -> rustadsb_core::track::RangeOutline {
    if hours * 60.0 < 1.0 {
        eprintln!("rustadsb: --range-outline-hours too short, using one minute");
    }
    rustadsb_core::track::RangeOutline::new((hours * 3_600_000.0) as i64)
}

fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

fn run_ifile(
    cli: &Cli,
    engine: &mut Engine<'_>,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let path = cli
        .ifile
        .as_deref()
        .ok_or("SDR type 'ifile' requires an --ifile argument")?;
    let mut input: Box<dyn Read> = if path == "-" {
        Box::new(io::stdin().lock())
    } else {
        Box::new(File::open(path).map_err(|e| format!("ifile: could not open {path}: {e}"))?)
    };

    let started = Instant::now();
    let mut buf = vec![0u8; 64 * 1024];
    while !stop.load(Ordering::Relaxed) && !expired(deadline) {
        let n = input
            .read(&mut buf)
            .map_err(|e| format!("ifile: error reading input file: {e}"))?;
        if n == 0 {
            break;
        }
        let mut batches = Vec::new();
        engine.pipe.push(&buf[..n], &mut |b| batches.push(b));
        for b in batches {
            engine.process_batch(b);
        }
        engine.drain_net();
        let now = engine.pipe.now_ms();
        engine.tick(now)?;
        if let Some(e) = engine.write_error.take() {
            // A closed pipe (e.g. `| head`) is a normal way to stop.
            return if e.kind() == io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(e.to_string())
            };
        }
        if cli.throttle {
            // process samples at the original capture speed
            let due = Duration::from_secs_f64(engine.pipe.samples() as f64 / SAMPLE_RATE);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
    }
    let mut batches = Vec::new();
    engine.pipe.finish(&mut |b| batches.push(b));
    for b in batches {
        engine.process_batch(b);
    }
    engine.drain_net();
    let now = engine.pipe.now_ms();
    engine.finish(now)
}

/// Live input from an RTL-SDR dongle.
fn run_rtlsdr(
    cli: &Cli,
    engine: &mut Engine<'_>,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let cfg = sdr::SdrConfig {
        device: cli.device.clone(),
        freq_hz: cli.freq,
        sample_rate: SAMPLE_RATE as u32,
        // -10 is upstream's spelling of "automatic gain"
        gain_db: cli.gain.filter(|g| *g != -10.0),
        ppm: cli.ppm,
        bias_tee: cli.enable_biastee,
    };
    if cli.iformat.is_some_and(|f| f != InputFormat::Uc8) {
        return Err("rtlsdr delivers UC8 samples; --iformat does not apply".into());
    }
    let reader = sdr::SdrReader::start(&cfg)?;
    if !cli.quiet {
        eprintln!("rtlsdr: {}", reader.description);
    }
    // Opening the dongle takes a moment; the sample clock starts now.
    engine.pipe.startup_time = net::now_ms();
    let mut last_tick = 0;
    let mut result = Ok(());
    while !stop.load(Ordering::Relaxed) && !expired(deadline) {
        let chunk = reader.recv(Duration::from_millis(100));
        let mut batches = Vec::new();
        match chunk {
            Some(sdr::Chunk::Data {
                bytes,
                dropped_before,
            }) => {
                if dropped_before > 0 {
                    engine
                        .pipe
                        .add_dropped(dropped_before, &mut |b| batches.push(b));
                }
                engine.pipe.push(&bytes, &mut |b| batches.push(b));
                // Keep the sample clock on wall time. Dropped samples or a stalled decoder
                // would otherwise make it fall behind for good.
                let skew = net::now_ms() - engine.pipe.now_ms();
                if skew.abs() > 500 {
                    engine.pipe.startup_time += skew;
                }
            }
            Some(sdr::Chunk::Failed(e)) => {
                result = Err(e);
                break;
            }
            None => {}
        }
        for b in batches {
            engine.process_batch(b);
        }
        engine.drain_net();
        let now = net::now_ms();
        if now - last_tick >= 100 {
            last_tick = now;
            if let Err(e) = engine.tick(now) {
                result = Err(e);
                break;
            }
        }
        if let Some(e) = engine.write_error.take() {
            result = if e.kind() == io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(e.to_string())
            };
            break;
        }
    }
    // stop streaming and release the dongle (or give up on a wedged one)
    let lost_at_end = reader.shutdown();
    let mut batches = Vec::new();
    // what was never decoded is a gap at the end of the stream
    engine
        .pipe
        .add_dropped(lost_at_end, &mut |b| batches.push(b));
    engine.pipe.finish(&mut |b| batches.push(b));
    for b in batches {
        engine.process_batch(b);
    }
    engine.drain_net();
    let finished = engine.finish(net::now_ms());
    match (result, finished) {
        (Err(e), Err(f)) => {
            // keep the input error, which says why we stopped; still mention the other
            eprintln!("rustadsb: {f}");
            Err(e)
        }
        (Err(e), _) | (Ok(()), Err(e)) => Err(e),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn run_net_only(
    engine: &mut Engine<'_>,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let mut last_tick = 0;
    while !stop.load(Ordering::Relaxed) && !expired(deadline) {
        std::thread::sleep(Duration::from_millis(20));
        engine.drain_net();
        let now = net::now_ms();
        if now - last_tick >= 100 {
            last_tick = now;
            engine.tick(now)?;
        }
        if let Some(e) = engine.write_error.take() {
            return if e.kind() == io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(e.to_string())
            };
        }
    }
    engine.drain_net();
    engine.finish(net::now_ms())
}

/// Print the statistics block in upstream's format (`--stats`).
fn print_stats(engine: &Engine<'_>, nfix_crc: u8) {
    let (pipe, tracker) = (&engine.pipe, &engine.tracker);
    let s = &pipe.demod.stats;
    if !engine.net_only {
        println!("Local receiver:");
        println!("  {} samples processed", pipe.samples());
        println!("  {} samples dropped", pipe.samples_dropped());
        println!("  {} samples lost", pipe.samples_lost());
        println!("  {} Mode A/C messages received", s.demod_modeac);
        println!("  {} Mode-S message preambles received", s.demod_preambles);
        println!(
            "    {} with bad message format or invalid CRC",
            s.demod_rejected_bad
        );
        println!(
            "    {} with unrecognized ICAO address",
            s.demod_rejected_unknown_icao
        );
        println!("    {} accepted with correct CRC", s.demod_accepted[0]);
        for j in 1..=(nfix_crc as usize) {
            println!(
                "    {} accepted with {j}-bit error repaired",
                s.demod_accepted[j]
            );
        }
        if s.noise_power_sum > 0.0 && s.noise_power_count > 0 {
            println!(
                "  {:.1} dBFS noise power",
                10.0 * (s.noise_power_sum / s.noise_power_count as f64).log10()
            );
        }
        if s.signal_power_sum > 0.0 && s.signal_power_count > 0 {
            println!(
                "  {:.1} dBFS mean signal power",
                10.0 * (s.signal_power_sum / s.signal_power_count as f64).log10()
            );
        }
        if s.peak_signal_power > 0.0 {
            println!(
                "  {:.1} dBFS peak signal power",
                10.0 * s.peak_signal_power.log10()
            );
        }
        println!(
            "  {} messages with signal power above -3dBFS",
            s.strong_signal_count
        );

        println!("\n Phase stats");
        print!("\n ");
        for i in 0..5 {
            print!(" {:8}", i + 3);
        }
        print!("\n ");
        for v in s.demod_preamble_phase {
            print!(" {v:8}");
        }
        print!("\n ");
        for i in 0..5 {
            print!(" {:8}", i + 4);
        }
        print!("\n ");
        for v in s.demod_best_phase {
            print!(" {v:8}");
        }
        println!("\n");
    }

    if engine.hub.is_some() {
        let r = &engine.remote;
        println!("Network:");
        println!("Messages from network clients:");
        println!("  {} Mode A/C messages received", r.modeac);
        println!("  {} Mode S messages received", r.modes);
        println!(
            "    {} with bad message format or invalid CRC",
            r.rejected_bad
        );
        println!(
            "    {} with unrecognized ICAO address",
            r.rejected_unknown_icao
        );
        println!("    {} accepted with correct CRC", r.accepted[0]);
        for j in 1..=(nfix_crc as usize) {
            println!("    {} accepted with {j}-bit error repaired", r.accepted[j]);
        }
        // Only when something was lost: upstream has no such lines, and the comparison
        // scripts diff this block against it.
        if net::input_dropped() > 0 {
            println!(
                "  {} messages dropped because the decoder's input queue was full",
                net::input_dropped()
            );
        }
        if net::output_skipped() > 0 {
            println!(
                "  {} output chunks skipped for clients that could not keep up",
                net::output_skipped()
            );
        }
    }

    let t = &tracker.stats;
    println!("{} total usable messages", t.messages_total);
    println!("{} surface position messages received", t.cpr_surface);
    println!("{} airborne position messages received", t.cpr_airborne);
    println!(
        "{} global CPR attempts with valid positions",
        t.cpr_global_ok
    );
    println!("{} global CPR attempts with bad data", t.cpr_global_bad);
    println!(
        "  {} global CPR attempts that failed the range check",
        t.cpr_global_range_checks
    );
    println!(
        "  {} global CPR attempts that failed the speed check",
        t.cpr_global_speed_checks
    );
    println!(
        "{} global CPR attempts with insufficient data",
        t.cpr_global_skipped
    );
    println!("{} local CPR attempts with valid positions", t.cpr_local_ok);
    println!(
        "  {} aircraft-relative positions",
        t.cpr_local_aircraft_relative
    );
    println!(
        "  {} receiver-relative positions",
        t.cpr_local_receiver_relative
    );
    println!(
        "{} local CPR attempts that did not produce useful positions",
        t.cpr_local_skipped
    );
    println!(
        "  {} local CPR attempts that failed the range check",
        t.cpr_local_range_checks
    );
    println!(
        "  {} local CPR attempts that failed the speed check",
        t.cpr_local_speed_checks
    );
    println!(
        "{} CPR messages that look like transponder failures filtered",
        t.cpr_filtered
    );
    println!(
        "{} non-ES altitude messages from ES-equipped aircraft ignored",
        t.suppressed_altitude_messages
    );
    println!("{} unique aircraft tracks", t.unique_aircraft);
    println!(
        "{} aircraft tracks where only one message was seen",
        t.single_message_aircraft
    );
}
