//! Command line, with upstream's long option names.

use clap::{ArgAction, Parser, ValueEnum};
use openradome_core::convert::InputFormat;
use openradome_core::demod::{PREAMBLE_THRESHOLD_MAX, PREAMBLE_THRESHOLD_MIN};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum DeviceType {
    Ifile,
    Rtlsdr,
    None,
}

/// ADS-B and Mode S decoder.
#[derive(Debug, Parser)]
// Like upstream, a single-value option given twice takes the last value (feeder installers do
// that, e.g. --uuid-file).
#[command(
    name = "openradome",
    version,
    about,
    disable_help_subcommand = true,
    args_override_self = true
)]
pub struct Cli {
    // ---- General options ----
    /// Read options from this TOML file: one key per long option (`net-bo-port = 30005`, `fix = true`,
    /// lists for repeatable ones); options given on the command line override it
    #[arg(long, value_name = "file.toml", help_heading = "General options")]
    pub config: Option<std::path::PathBuf>,
    /// Print the options given (and those from --config) as a TOML config file, then exit
    #[arg(long, help_heading = "General options")]
    pub print_config: bool,
    /// Reference/receiver surface latitude
    #[arg(
        long,
        value_name = "lat",
        allow_negative_numbers = true,
        help_heading = "General options"
    )]
    pub lat: Option<f64>,
    /// Reference/receiver surface longitude
    #[arg(
        long,
        value_name = "lon",
        allow_negative_numbers = true,
        help_heading = "General options"
    )]
    pub lon: Option<f64>,
    /// Absolute maximum range for position decoding (in nmi, default: 450)
    #[arg(long, value_name = "dist", help_heading = "General options")]
    pub max_range: Option<f64>,
    /// Enable decoding of SSR Modes 3/A & 3/C
    #[arg(long, help_heading = "General options")]
    pub modeac: bool,
    /// Enable CRC single-bit error correction (default)
    #[arg(long, help_heading = "General options")]
    pub fix: bool,
    /// Disable CRC single-bit error correction
    #[arg(long, help_heading = "General options")]
    pub no_fix: bool,
    /// Disable CRC single-bit error correction on the DF type to produce more DF17 messages
    #[arg(long, help_heading = "General options")]
    pub no_fix_df: bool,
    /// Enable two-bit CRC error correction
    #[arg(long, hide = true, help_heading = "General options")]
    pub aggressive: bool,
    /// Use metric units
    #[arg(long, help_heading = "General options")]
    pub metric: bool,
    /// Interactive mode refreshing data on screen. Implies --throttle
    #[arg(long, help_heading = "General options")]
    pub interactive: bool,
    /// Disable interactive mode, print to stdout
    #[arg(long, help_heading = "General options")]
    pub no_interactive: bool,
    /// Remove from list if idle for <sec> (default: 60)
    #[arg(
        long,
        value_name = "sec",
        default_value_t = 60.0,
        help_heading = "General options"
    )]
    pub interactive_ttl: f64,
    /// Show only messages by given ICAO on stdout
    #[arg(long, value_name = "addr", value_parser = parse_hex24, help_heading = "General options")]
    pub show_only: Option<u32>,
    /// Process only messages by given ICAO
    #[arg(long, value_name = "addr", value_parser = parse_hex24, help_heading = "General options")]
    pub process_only: Option<u32>,
    /// When displaying decoded Mode S messages on stdout only show this DF type (comma separated)
    #[arg(
        long = "filter-DF",
        value_name = "type",
        value_delimiter = ',',
        help_heading = "General options"
    )]
    pub filter_df: Vec<u8>,
    /// Select SDR type
    #[arg(
        long,
        value_name = "type",
        value_enum,
        help_heading = "General options"
    )]
    pub device_type: Option<DeviceType>,
    /// Show only messages hex values
    #[arg(long, help_heading = "General options")]
    pub raw: bool,
    /// Lower threshold --> more CPU usage (default: 58, pi zero / pi 1: 75, hot CPU 42)
    #[arg(long, value_name = "40-400", help_heading = "General options")]
    pub preamble_threshold: Option<i64>,
    /// Print stats at exit. No other output
    #[arg(long, help_heading = "General options")]
    pub stats: bool,
    /// Periodically write json output to <dir>
    #[arg(long, value_name = "dir", help_heading = "General options")]
    pub write_json: Option<std::path::PathBuf>,
    /// Write json output every <sec> seconds (default 1)
    #[arg(
        long,
        value_name = "sec",
        default_value_t = 1.0,
        help_heading = "General options"
    )]
    pub write_json_every: f64,
    /// Also write aircraft.json as aircraft.json.gz
    #[arg(long, help_heading = "General options")]
    pub write_json_gzip: bool,
    /// Write only the binary aircraft.binCraft: for globe files (1), aircraft.json as well (2)
    #[arg(
        long = "write-json-binCraft-only",
        value_name = "n",
        help_heading = "General options"
    )]
    pub write_json_bincraft_only: Option<u8>,
    /// Also write aircraft.binCraft gzipped (the old format; .zst is always written)
    #[arg(long = "write-binCraft-old", help_heading = "General options")]
    pub write_bincraft_old: bool,
    /// http API listen port (a single port); update frequency follows --write-json-every
    #[arg(long, value_name = "port", help_heading = "Network options")]
    pub net_api_port: Option<u16>,
    /// Keep serving API queries this many seconds after shutdown starts (?status answers 503)
    #[arg(
        long,
        value_name = "seconds",
        default_value_t = 0.0,
        help_heading = "Network options"
    )]
    pub api_shutdown_delay: f64,
    /// Tell tar1090 (in receiver.json) to use the API: needs /tar1090/re-api proxied to --net-api-port
    #[arg(long, help_heading = "Network options")]
    pub tar1090_use_api: bool,
    /// Serve a status web page on this port: live figures, 24 h graphs and the open connections
    /// (openradome only; off unless given; listens on --dashboard-bind-address)
    #[arg(long, value_name = "port", help_heading = "Network options")]
    pub dashboard_port: Option<u16>,
    /// Address the dashboard listens on (default: --net-bind-address, else any). Lets the page be
    /// reached from the LAN while the data ports stay on 127.0.0.1
    #[arg(long, value_name = "ip", help_heading = "Network options")]
    pub dashboard_bind_address: Option<std::net::IpAddr>,
    /// Serve a Model Context Protocol (MCP) server on this port, at /mcp: read-only tools an AI
    /// assistant can ask about the station (openradome only; off unless given; needs --mcp-token-file)
    #[arg(long, value_name = "port", help_heading = "Network options")]
    pub mcp_port: Option<u16>,
    /// Address the MCP server listens on (default: --net-bind-address, else any)
    #[arg(long, value_name = "ip", help_heading = "Network options")]
    pub mcp_bind_address: Option<std::net::IpAddr>,
    /// File holding the token MCP clients must send (Authorization: Bearer ...), at least 32 characters
    #[arg(long, value_name = "file", help_heading = "Network options")]
    pub mcp_token_file: Option<std::path::PathBuf>,
    /// Serve MCP over HTTPS with this certificate (PEM, leaf first; reloaded when it changes)
    #[arg(
        long = "mcp-tls-cert",
        value_name = "file.pem",
        help_heading = "Network options"
    )]
    pub mcp_tls_cert: Option<std::path::PathBuf>,
    /// The private key for --mcp-tls-cert (PEM)
    #[arg(
        long = "mcp-tls-key",
        value_name = "file.pem",
        help_heading = "Network options"
    )]
    pub mcp_tls_key: Option<std::path::PathBuf>,
    /// Write specially indexed globe_xxxx.binCraft files and traces (for tar1090)
    #[arg(long, help_heading = "General options")]
    pub write_json_globe_index: bool,
    /// Write traces to this directory, one gz compressed json per day and airframe
    #[arg(long, value_name = "dir", help_heading = "General options")]
    pub write_globe_history: Option<std::path::PathBuf>,
    /// Interval after which a new position is guaranteed to be written to the trace (default: 20)
    #[arg(long, value_name = "seconds", help_heading = "General options")]
    pub json_trace_interval: Option<f64>,
    /// Don't write recent (1), full (2), either (3) traces to the json dir, only archive them
    #[arg(long, value_name = "1,2,3,8", help_heading = "General options")]
    pub json_trace_hist_only: Option<u8>,
    /// Write full traces to this directory instead of the json dir (links them there)
    #[arg(long, value_name = "dir", help_heading = "General options")]
    pub full_trace_dir: Option<std::path::PathBuf>,
    /// Minimum position reliability to put it into json (default 1, 2 with the globe index; -1 to 4)
    #[arg(long, value_name = "n", help_heading = "General options")]
    pub json_reliable: Option<i32>,
    /// Write state to disk (the range outline) so it survives a restart
    #[arg(long, value_name = "dir", help_heading = "General options")]
    pub write_state: Option<std::path::PathBuf>,
    /// Accepted for compatibility: upstream's interval for its aircraft state, which openradome
    /// does not keep (the outline is saved every 5 minutes)
    #[arg(long, value_name = "seconds", help_heading = "General options")]
    pub write_state_every: Option<f64>,
    /// Save state only at exit (and every 6 hours), not every 5 minutes
    #[arg(long, help_heading = "General options")]
    pub write_state_only_on_exit: bool,
    /// Show only ICAO addresses
    #[arg(long, help_heading = "General options")]
    pub onlyaddr: bool,
    /// Disable output
    #[arg(long, help_heading = "General options")]
    pub quiet: bool,
    /// Display raw messages in Beast ASCII mode (timestamped)
    #[arg(long, hide = true, help_heading = "General options")]
    pub mlat: bool,
    /// Forward messages unchanged (as received, before correction)
    #[arg(long, help_heading = "General options")]
    pub net_verbatim: bool,
    /// Show altitudes as GNSS when available
    #[arg(long, help_heading = "General options")]
    pub gnss: bool,
    /// Run for X seconds, then exit (default: run indefinitely)
    #[arg(long, value_name = "sec", help_heading = "General options")]
    pub auto_exit: Option<f64>,
    /// SDR buffer / USB transfer size in kibibytes
    #[arg(
        long,
        value_name = "KiB",
        default_value_t = 128,
        help_heading = "General options"
    )]
    pub sdr_buffer_size: usize,

    // ---- Network ----
    /// Enable networking
    #[arg(long, help_heading = "Network options")]
    pub net: bool,
    /// Legacy option, enable networking, use --net instead
    #[arg(long, hide = true, help_heading = "Network options")]
    pub net_only: bool,
    /// IP address to bind to (default: any; use 127.0.0.1 for private)
    #[arg(long, value_name = "ip", help_heading = "Network options")]
    pub net_bind_address: Option<std::net::IpAddr>,
    /// TCP Beast output listen ports
    #[arg(
        long,
        value_name = "ports",
        default_value = "0",
        help_heading = "Network options"
    )]
    pub net_bo_port: String,
    /// TCP Beast input listen ports
    #[arg(
        long,
        value_name = "ports",
        default_value = "0",
        help_heading = "Network options"
    )]
    pub net_bi_port: String,
    /// TCP raw output listen ports
    #[arg(
        long,
        value_name = "ports",
        default_value = "0",
        help_heading = "Network options"
    )]
    pub net_ro_port: String,
    /// TCP raw input listen ports
    #[arg(
        long,
        value_name = "ports",
        default_value = "0",
        help_heading = "Network options"
    )]
    pub net_ri_port: String,
    /// TCP BaseStation (SBS) output listen ports
    #[arg(
        long,
        value_name = "ports",
        default_value = "0",
        help_heading = "Network options"
    )]
    pub net_sbs_port: String,
    /// Connect as a TCP client, e.g. 127.0.0.1,30004,beast_out (can be given more than once;
    /// protocols: beast_in, beast_out, beast_reduce_out, beast_reduce_plus_out, raw_in, raw_out,
    /// sbs_out; a fallback host and port may follow, as may silent_fail and uuid=<UUID>)
    #[arg(
        long,
        value_name = "IP,PORT,PROTOCOL[,IP2,PORT2][,silent_fail][,uuid=UUID]",
        help_heading = "Network options"
    )]
    pub net_connector: Vec<String>,
    /// TCP BeastReduce output listen ports
    #[arg(
        long,
        value_name = "ports",
        default_value = "0",
        help_heading = "Network options"
    )]
    pub net_beast_reduce_out_port: String,
    /// BeastReduce data update interval in seconds, longer means less data (default 0.25,
    /// at most 15)
    #[arg(
        long,
        value_name = "seconds",
        default_value_t = 0.25,
        help_heading = "Network options"
    )]
    pub net_beast_reduce_interval: f64,
    /// TCP output flush interval for beast-reduce outputs (accepted; see --net-ro-interval)
    #[arg(
        long,
        value_name = "sec",
        hide = true,
        help_heading = "Network options"
    )]
    pub net_ro_interval_beast_reduce: Option<f64>,
    /// UUID to send with beast_reduce_plus_out (generate one with
    /// cat /proc/sys/kernel/random/uuid)
    #[arg(long, value_name = "UUID", help_heading = "Network options")]
    pub uuid: Option<String>,
    /// TCP heartbeat rate in seconds (default 60, 0 to disable)
    #[arg(
        long,
        value_name = "rate",
        default_value_t = 60.0,
        help_heading = "Network options"
    )]
    pub net_heartbeat: f64,
    /// Outbound re-connection delay in seconds (default 15)
    #[arg(
        long,
        value_name = "seconds",
        default_value_t = 15.0,
        help_heading = "Network options"
    )]
    pub net_connector_delay: f64,
    /// Forward received Beast mlat results to Beast output ports
    #[arg(long, help_heading = "Network options")]
    pub forward_mlat: bool,
    /// Forward received mlat results to SBS output ports
    #[arg(long, help_heading = "Network options")]
    pub forward_mlat_sbs: bool,

    // ---- ifile ----
    /// Read samples from given file ('-' for stdin)
    #[arg(long, value_name = "path", help_heading = "ifile options")]
    pub ifile: Option<String>,
    /// Set sample format (UC8, SC16, SC16Q11)
    #[arg(long, value_name = "type", value_parser = parse_format, help_heading = "ifile options")]
    pub iformat: Option<InputFormat>,
    /// Process samples at the original capture speed
    #[arg(long, action = ArgAction::SetTrue, help_heading = "ifile options")]
    pub throttle: bool,

    // ---- RTL-SDR ----
    /// Select device by index or serial number (default 0)
    #[arg(
        long,
        value_name = "index|serial",
        default_value = "0",
        help_heading = "RTL-SDR options"
    )]
    pub device: String,
    /// Set gain in dB (default: maximum). -10 selects the tuner's automatic gain
    #[arg(
        long,
        value_name = "dB",
        allow_negative_numbers = true,
        help_heading = "RTL-SDR options"
    )]
    pub gain: Option<f32>,
    /// Set oscillator frequency correction in PPM
    #[arg(
        long,
        value_name = "ppm",
        default_value_t = 0,
        allow_negative_numbers = true,
        help_heading = "RTL-SDR options"
    )]
    pub ppm: i32,
    /// Set frequency in Hz (default 1090000000)
    #[arg(
        long,
        value_name = "Hz",
        default_value_t = 1_090_000_000,
        help_heading = "RTL-SDR options"
    )]
    pub freq: u64,
    /// Enable the bias tee (RF power on the antenna connector)
    #[arg(long, help_heading = "RTL-SDR options")]
    pub enable_biastee: bool,

    // ---- Accepted for compatibility with upstream command lines ----
    /// Receiver location in the JSON metadata: 0 none, 1 approximate, 2 exact (only 2 is implemented)
    #[arg(
        long,
        value_name = "0-2",
        default_value_t = 2,
        help_heading = "JSON options"
    )]
    pub json_location_accuracy: u8,
    /// Hours of range outline to keep for outline.json (default 24)
    #[arg(long, value_name = "hours", help_heading = "JSON options")]
    pub range_outline_hours: Option<f64>,
    /// TCP output flush size: send once this many bytes are waiting (default 1280, minimum 750)
    #[arg(long, value_name = "bytes", help_heading = "Network options")]
    pub net_ro_size: Option<usize>,
    /// TCP output flush interval in seconds (accepted; output is flushed after every block of
    /// samples, about 27 ms, as upstream does with an SDR and an interval under 51 ms)
    #[arg(
        long,
        value_name = "sec",
        hide = true,
        help_heading = "Network options"
    )]
    pub net_ro_interval: Option<f64>,
    /// File holding the UUID to send with beast_reduce_plus_out
    #[arg(long, value_name = "path", help_heading = "Network options")]
    pub uuid_file: Option<std::path::PathBuf>,
}

fn parse_hex24(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| format!("not a hex address: {e}"))
}

fn parse_format(s: &str) -> Result<InputFormat, String> {
    InputFormat::parse(s).ok_or_else(|| {
        format!("Input format '{s}' not understood (supported values: UC8, SC16, SC16Q11)")
    })
}

impl Cli {
    /// Is any networking requested?
    pub fn network_enabled(&self) -> bool {
        self.net
            || self.net_only
            || !self.net_connector.is_empty()
            || [
                &self.net_bo_port,
                &self.net_bi_port,
                &self.net_ro_port,
                &self.net_ri_port,
                &self.net_sbs_port,
            ]
            .iter()
            .any(|p| p.as_str() != "0")
    }

    /// Number of CRC bit errors to correct: 0, 1 (default) or 2.
    pub fn nfix_crc(&self) -> u8 {
        if self.aggressive {
            2
        } else if self.no_fix {
            0
        } else {
            1
        }
    }

    /// Preamble threshold, clamped to the allowed range.
    pub fn preamble_threshold(&self) -> Option<u32> {
        self.preamble_threshold
            .map(|v| v.clamp(PREAMBLE_THRESHOLD_MIN as i64, PREAMBLE_THRESHOLD_MAX as i64) as u32)
    }

    /// DF filter as a bit set, if `--filter-DF` was given.
    pub fn filter_df_bits(&self) -> Option<u32> {
        if self.filter_df.is_empty() {
            None
        } else {
            Some(
                self.filter_df
                    .iter()
                    .filter(|&&d| d < 32)
                    .fold(0u32, |acc, &d| acc | (1 << d)),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("openradome").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn defaults() {
        let c = parse(&[]);
        assert_eq!(c.nfix_crc(), 1);
        assert_eq!(c.preamble_threshold(), None);
        assert_eq!(c.sdr_buffer_size, 128);
        assert!(c.filter_df_bits().is_none());
    }

    #[test]
    fn fix_flags() {
        assert_eq!(parse(&["--no-fix"]).nfix_crc(), 0);
        assert_eq!(parse(&["--aggressive"]).nfix_crc(), 2);
        assert_eq!(parse(&["--fix"]).nfix_crc(), 1);
    }

    #[test]
    fn ifile_options() {
        let c = parse(&[
            "--ifile",
            "cap.bin",
            "--iformat",
            "sc16q11",
            "--raw",
            "--modeac",
        ]);
        assert_eq!(c.ifile.as_deref(), Some("cap.bin"));
        assert_eq!(c.iformat, Some(InputFormat::Sc16Q11));
        assert!(c.raw && c.modeac);
        assert!(Cli::try_parse_from(["openradome", "--iformat", "bogus"]).is_err());
        // a repeated single-value option takes the last value, like upstream
        let c = Cli::try_parse_from(["openradome", "--uuid-file=/a", "--uuid-file", "/b"]).unwrap();
        assert_eq!(c.uuid_file.as_deref(), Some(std::path::Path::new("/b")));
    }

    #[test]
    fn network_options() {
        assert!(!parse(&[]).network_enabled());
        assert!(parse(&["--net"]).network_enabled());
        assert!(parse(&["--net-bo-port", "30005"]).network_enabled());
        let c = parse(&[
            "--net-bind-address",
            "127.0.0.1",
            "--dashboard-port",
            "8090",
            "--dashboard-bind-address",
            "0.0.0.0",
        ]);
        assert_eq!(c.dashboard_port, Some(8090));
        assert_eq!(c.dashboard_bind_address, Some("0.0.0.0".parse().unwrap()));
        assert_eq!(
            c.net_bind_address,
            Some("127.0.0.1".parse().unwrap()),
            "the data ports stay local"
        );
        assert!(
            parse(&[
                "--net-connector",
                "h,1,beast_in",
                "--net-connector",
                "h,2,raw_out"
            ])
            .network_enabled()
        );
        assert_eq!(
            parse(&["--net-connector", "a", "--net-connector", "b"])
                .net_connector
                .len(),
            2
        );
    }

    #[test]
    fn numeric_options() {
        let c = parse(&[
            "--preamble-threshold",
            "9999",
            "--lat",
            "-33.5",
            "--lon",
            "151.2",
        ]);
        assert_eq!(c.preamble_threshold(), Some(400));
        assert_eq!(c.lat, Some(-33.5));
        let c = parse(&["--preamble-threshold", "1"]);
        assert_eq!(c.preamble_threshold(), Some(40));
        let c = parse(&["--show-only", "4840d6", "--filter-DF", "11,17,99"]);
        assert_eq!(c.show_only, Some(0x4840D6));
        assert_eq!(c.filter_df_bits(), Some((1 << 11) | (1 << 17)));
    }
}
