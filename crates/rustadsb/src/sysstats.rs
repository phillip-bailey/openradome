//! The host's own figures for the dashboard: CPU, load, memory, temperature, under-voltage, disk,
//! Wi-Fi and the CPU used by every rustadsb process. Read from Linux's `/proc` and `/sys` (and the
//! root filesystem's statistics); elsewhere [`read`] returns `None` and the dashboard leaves the
//! section out. The parsers take the files' text, so they are tested with real samples.

// the parsers are only called by the Linux readers (they are tested everywhere)
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashMap;

/// One reading of the host.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// Cumulative CPU time in clock ticks: (busy, total), all cores together.
    pub cpu_ticks: (u64, u64),
    pub cores: usize,
    /// Load average over 1, 5 and 15 minutes.
    pub load: [f64; 3],
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
    /// SoC temperature, °C.
    pub temp_c: Option<f64>,
    /// Raspberry Pi under-voltage alarm (the power supply sags): Some(true) while it is on.
    pub undervoltage: Option<bool>,
    /// Root filesystem in bytes, as `df` counts: (used, available to users, size).
    pub disk: Option<(u64, u64, u64)>,
    /// Wi-Fi signal level, dBm, of the first wireless interface.
    pub wifi: Option<(String, f64)>,
    /// Seconds since the host booted.
    pub uptime_s: Option<f64>,
}

/// CPU busy share (0..100 %) between two readings of the cumulative ticks.
pub fn busy_pct(before: (u64, u64), after: (u64, u64)) -> Option<f64> {
    let total = after.1.checked_sub(before.1)?;
    let busy = after.0.checked_sub(before.0)?;
    (total > 0).then(|| busy as f64 * 100.0 / total as f64)
}

/// `/proc/stat`, first line: `cpu user nice system idle iowait irq softirq steal ...`. Idle and
/// iowait are not busy; guest time is already inside user and nice.
pub fn parse_cpu(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .filter_map(|x| x.parse().ok())
        .collect();
    if v.len() < 4 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some((total - idle, total))
}

/// Number of `cpuN` lines in `/proc/stat`.
pub fn parse_cores(stat: &str) -> usize {
    stat.lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .count()
}

/// `/proc/loadavg`: `4.64 4.69 4.77 6/289 25167`.
pub fn parse_load(s: &str) -> Option<[f64; 3]> {
    let mut it = s.split_whitespace().map(|x| x.parse::<f64>().ok());
    Some([it.next()??, it.next()??, it.next()??])
}

/// `/proc/meminfo`: (MemTotal, MemAvailable, SwapTotal, SwapFree) in kB.
pub fn parse_meminfo(s: &str) -> Option<(u64, u64, u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        s.lines()
            .find(|l| l.starts_with(name) && l[name.len()..].starts_with(':'))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    Some((
        field("MemTotal")?,
        field("MemAvailable")?,
        field("SwapTotal").unwrap_or(0),
        field("SwapFree").unwrap_or(0),
    ))
}

/// `/proc/net/wireless`: the first interface and its signal level (dBm).
/// ` wlan0: 0000   40.  -70.  -256        0 ...`
pub fn parse_wireless(s: &str) -> Option<(String, f64)> {
    let line = s.lines().skip(2).find(|l| l.contains(':'))?;
    let (iface, rest) = line.split_once(':')?;
    let level: f64 = rest
        .split_whitespace()
        .nth(2)?
        .trim_end_matches('.')
        .parse()
        .ok()?;
    // some drivers report 0..100 instead of dBm; only dBm is shown
    (level < 0.0).then(|| (iface.trim().to_string(), level))
}

/// `/proc/<pid>/stat`: (command name, utime + stime ticks). The name is in parentheses and may
/// contain spaces and parentheses itself, so it runs to the last `)`.
pub fn parse_proc_stat(s: &str) -> Option<(&str, u64)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let name = s.get(open + 1..close)?;
    let fields: Vec<&str> = s.get(close + 2..)?.split_whitespace().collect();
    // utime and stime are fields 14 and 15; the slice starts at field 3
    let ticks = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    Some((name, ticks))
}

#[cfg(target_os = "linux")]
pub fn read() -> Option<Sample> {
    let text = |p: &str| std::fs::read_to_string(p).ok();
    let stat = text("/proc/stat")?;
    let (mem_total_kb, mem_available_kb, swap_total_kb, swap_free_kb) =
        parse_meminfo(&text("/proc/meminfo")?)?;
    let temp_c = text("/sys/class/thermal/thermal_zone0/temp")
        .and_then(|t| t.trim().parse::<f64>().ok())
        .map(|m| m / 1000.0);
    Some(Sample {
        cpu_ticks: parse_cpu(&stat)?,
        cores: parse_cores(&stat).max(1),
        load: text("/proc/loadavg")
            .and_then(|l| parse_load(&l))
            .unwrap_or_default(),
        mem_total_kb,
        mem_available_kb,
        swap_total_kb,
        swap_free_kb,
        temp_c,
        undervoltage: undervoltage(),
        disk: disk("/"),
        wifi: text("/proc/net/wireless").and_then(|w| parse_wireless(&w)),
        uptime_s: text("/proc/uptime").and_then(|u| u.split_whitespace().next()?.parse().ok()),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn read() -> Option<Sample> {
    None
}

/// The Raspberry Pi firmware's under-voltage alarm (`rpi_volt` hwmon device), if there is one.
#[cfg(target_os = "linux")]
fn undervoltage() -> Option<bool> {
    for entry in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let dir = entry.path();
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        if name.trim() == "rpi_volt" {
            let alarm = std::fs::read_to_string(dir.join("in0_lcrit_alarm")).ok()?;
            return Some(alarm.trim() != "0");
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn disk(path: &str) -> Option<(u64, u64, u64)> {
    let s = rustix::fs::statvfs(path).ok()?;
    let used = s.f_blocks.saturating_sub(s.f_bfree) * s.f_frsize;
    Some((used, s.f_bavail * s.f_frsize, s.f_blocks * s.f_frsize))
}

/// CPU time of every process named `rustadsb`, by pid, in ticks.
#[cfg(target_os = "linux")]
pub fn rustadsb_ticks() -> HashMap<u32, u64> {
    let mut out = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue; // gone since the listing
        };
        if let Some(("rustadsb", ticks)) = parse_proc_stat(&stat) {
            out.insert(pid, ticks);
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
pub fn rustadsb_ticks() -> HashMap<u32, u64> {
    HashMap::new()
}

/// CPU used by the processes between two readings, in ticks. A process that started in between
/// counts from zero; one that stopped is left out.
pub fn ticks_between(before: &HashMap<u32, u64>, after: &HashMap<u32, u64>) -> u64 {
    after
        .iter()
        .map(|(pid, t)| t.saturating_sub(before.get(pid).copied().unwrap_or(0)))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    // from the Raspberry Pi 2 this runs on
    const STAT: &str = "cpu  115902969 40242845 60672440 256702347 129828 0 24845079 0 0 0\n\
        cpu0 28975742 10060711 15168110 64175586 32457 0 6211269 0 0 0\n\
        cpu1 1 2 3 4 5 0 6 0 0 0\ncpu2 1 2 3 4 5 0 6 0 0 0\ncpu3 1 2 3 4 5 0 6 0 0 0\nintr 1 2 3\n";

    #[test]
    fn cpu() {
        let (busy, total) = parse_cpu(STAT).unwrap();
        assert_eq!(
            total,
            115902969 + 40242845 + 60672440 + 256702347 + 129828 + 24845079
        );
        assert_eq!(busy, total - 256702347 - 129828);
        assert_eq!(parse_cores(STAT), 4);
        assert_eq!(busy_pct((10, 100), (40, 200)), Some(30.0));
        assert_eq!(busy_pct((10, 100), (10, 100)), None, "no time passed");
    }

    #[test]
    fn load_and_memory() {
        assert_eq!(
            parse_load("4.64 4.69 4.77 6/289 25167\n"),
            Some([4.64, 4.69, 4.77])
        );
        let mem = "MemTotal:         942120 kB\nMemFree:           55000 kB\nMemAvailable:     511204 kB\n\
                   SwapCached:  1 kB\nSwapTotal:        942076 kB\nSwapFree:         858076 kB\n";
        assert_eq!(parse_meminfo(mem), Some((942120, 511204, 942076, 858076)));
    }

    #[test]
    fn wireless() {
        let w = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n \
                 face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n \
                 wlan0: 0000   40.  -70.  -256        0      0      0   3575 11127473        0\n";
        assert_eq!(parse_wireless(w), Some(("wlan0".to_string(), -70.0)));
        let header_only = w.lines().take(2).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_wireless(&header_only), None, "wired only");
    }

    #[test]
    fn process_stat() {
        let s = "8255 (rustadsb) S 1 8255 8255 0 -1 4194560 1234 0 0 0 290000 193000 0 0 15 -5 5 0 100 25976832 2760 4294967295";
        assert_eq!(parse_proc_stat(s), Some(("rustadsb", 483000)));
        let odd = "42 (a b) c)) R 1 2 3 4 5 6 7 8 9 10 70 30 0";
        assert_eq!(parse_proc_stat(odd), Some(("a b) c)", 100)));
        let before = HashMap::from([(1, 100), (2, 50), (3, 10)]);
        let after = HashMap::from([(1, 160), (2, 50), (4, 7)]);
        assert_eq!(
            ticks_between(&before, &after),
            60 + 7,
            "pid 3 stopped, pid 4 started"
        );
    }
}
