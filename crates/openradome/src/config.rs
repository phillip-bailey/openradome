//! `--config <file.toml>`: options from a TOML file, and `--print-config` to write one.
//!
//! Each key is a long option's name, as `openradome --help` shows it, without the dashes (`net-bo-port`;
//! `net_bo_port` works too). A value becomes the option's argument; `true` turns an on/off option on and
//! `false` leaves it off; an array gives the option once per element (`net-connector`). The file's options
//! go before the command line's, and a single-value option given twice keeps the last value, so the
//! command line overrides the file. Turning the options into command-line arguments means every option
//! works in the file, checked exactly as on the command line.
//!
//! ```toml
//! device-type = "rtlsdr"
//! gain = 49.6
//! fix = true
//! net-bo-port = 30005
//! net-connector = ["feed.adsb.lol,30004,beast_reduce_plus_out"]
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use clap::CommandFactory;

use crate::cli::Cli;

/// Options that make no sense in the file.
const NOT_IN_FILE: &[&str] = &["config", "print-config", "help", "version"];

/// How an option is given: on its own, or with a value.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Flag,
    Value,
}

/// Every long option openradome knows, by name (aliases included).
fn options() -> BTreeMap<String, Kind> {
    let mut out = BTreeMap::new();
    for arg in Cli::command().get_arguments() {
        let kind = if arg.get_action().takes_values() {
            Kind::Value
        } else {
            Kind::Flag
        };
        for name in arg
            .get_long()
            .into_iter()
            .chain(arg.get_all_aliases().into_iter().flatten())
        {
            out.insert(name.to_string(), kind);
        }
    }
    out
}

/// The option a key names: its exact name, or with `_` for `-`.
fn option_for(key: &str, known: &BTreeMap<String, Kind>) -> Option<(String, Kind)> {
    [key.to_string(), key.replace('_', "-")]
        .into_iter()
        .find_map(|k| known.get(&k).map(|kind| (k, *kind)))
}

/// One TOML value as an option argument.
fn scalar(key: &str, v: &toml::Value) -> Result<String, String> {
    match v {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Integer(i) => Ok(i.to_string()),
        toml::Value::Float(f) => Ok(f.to_string()),
        _ => Err(format!(
            "{key}: want a string or a number (or a list of them)"
        )),
    }
}

/// The command-line arguments a TOML file stands for.
pub fn file_args(text: &str) -> Result<Vec<String>, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let known = options();
    let mut out = Vec::new();
    for (key, value) in &table {
        if value.is_table()
            || value
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.is_table()))
        {
            return Err(format!(
                "[{key}]: sections are not used; put each option at the top of the file"
            ));
        }
        let Some((name, kind)) =
            option_for(key, &known).filter(|(n, _)| !NOT_IN_FILE.contains(&n.as_str()))
        else {
            return Err(format!("unknown option {key:?} (see openradome --help)"));
        };
        let flag = format!("--{name}");
        match (kind, value) {
            (Kind::Flag, toml::Value::Boolean(on)) => {
                if *on {
                    out.push(flag);
                }
            }
            (Kind::Flag, _) => return Err(format!("{key}: an on/off option: want true or false")),
            (Kind::Value, toml::Value::Boolean(_)) => {
                return Err(format!("{key}: takes a value, not true or false"));
            }
            (Kind::Value, toml::Value::Array(items)) => {
                for item in items {
                    out.push(flag.clone());
                    out.push(scalar(key, item)?);
                }
            }
            (Kind::Value, v) => {
                out.push(flag);
                out.push(scalar(key, v)?);
            }
        }
    }
    Ok(out)
}

/// The path given with `--config`, if any (`--config PATH` or `--config=PATH`).
fn config_path(args: &[String]) -> Option<String> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--config" {
            return it.next().cloned();
        }
        if let Some(p) = a.strip_prefix("--config=") {
            return Some(p.to_string());
        }
    }
    None
}

/// The arguments with the `--config` file's options inserted ahead of the command line's.
pub fn expand(args: Vec<String>) -> Result<Vec<String>, String> {
    let Some(path) = config_path(&args) else {
        return Ok(args);
    };
    let text = std::fs::read_to_string(Path::new(&path))
        .map_err(|e| format!("config file {path}: {e}"))?;
    let from_file = file_args(&text).map_err(|e| format!("config file {path}: {e}"))?;
    let mut out = Vec::with_capacity(args.len() + from_file.len());
    let mut it = args.into_iter();
    out.extend(it.next()); // the program name
    out.extend(from_file);
    out.extend(it);
    Ok(out)
}

/// A value as TOML: a number when it reads back the same, else a string.
fn toml_value(s: &str) -> String {
    if s.parse::<i64>().is_ok_and(|i| i.to_string() == s) {
        return s.to_string();
    }
    if s.parse::<f64>()
        .is_ok_and(|f| f.is_finite() && f.to_string() == s)
    {
        return s.to_string();
    }
    toml::Value::String(s.to_string()).to_string()
}

/// `--print-config`: the given options (the program name first) as a TOML file, in the order given.
/// A single-value option given more than once keeps its last value, as when the program runs; one
/// that may be repeated becomes a list.
pub fn to_toml(args: &[String]) -> Result<String, String> {
    let known = options();
    let multiple: Vec<String> = Cli::command()
        .get_arguments()
        .filter(|a| matches!(a.get_action(), clap::ArgAction::Append))
        .filter_map(|a| a.get_long().map(str::to_string))
        .collect();
    let mut order: Vec<String> = Vec::new();
    let mut values: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
    let mut it = args.iter().skip(1).peekable();
    while let Some(a) = it.next() {
        let Some(body) = a.strip_prefix("--") else {
            return Err(format!("{a:?}: only long options can go in a config file"));
        };
        let (name, inline) = match body.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (body.to_string(), None),
        };
        let Some(kind) = known.get(&name).copied() else {
            return Err(format!("unknown option --{name}"));
        };
        if matches!(name.as_str(), "config" | "print-config") {
            if kind == Kind::Value && inline.is_none() {
                it.next();
            }
            continue;
        }
        let value = match kind {
            Kind::Flag => None,
            Kind::Value => Some(
                inline
                    .or_else(|| it.next().cloned())
                    .ok_or_else(|| format!("--{name} needs a value"))?,
            ),
        };
        if !values.contains_key(&name) {
            order.push(name.clone());
        }
        let slot = values.entry(name.clone()).or_default();
        if multiple.contains(&name) {
            slot.push(value);
        } else {
            *slot = vec![value];
        }
    }
    let mut out = String::from(
        "# openradome options: one key per long option, as in openradome --help (openradome --config <this file>)\n",
    );
    for name in order {
        let vals = &values[&name];
        let line = match vals.as_slice() {
            [None] => format!("{name} = true"),
            [Some(v)] if !multiple.contains(&name) => format!("{name} = {}", toml_value(v)),
            many => format!(
                "{name} = [{}]",
                many.iter()
                    .flatten()
                    .map(|v| toml_value(v))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn a_file_becomes_arguments() {
        let got = file_args(
            "lat = 51.5\nlon = -0.1\nfix = true\nmodeac = false\nnet_bo_port = 30005\n\
             device-type = \"rtlsdr\"\nnet-connector = [\"a,1,beast_in\", \"b,2,raw_in\"]\n",
        )
        .unwrap();
        // a TOML table keeps its keys sorted
        assert_eq!(
            got,
            args(&[
                "--device-type",
                "rtlsdr",
                "--fix",
                "--lat",
                "51.5",
                "--lon",
                "-0.1",
                "--net-connector",
                "a,1,beast_in",
                "--net-connector",
                "b,2,raw_in",
                "--net-bo-port",
                "30005"
            ])
        );
    }

    #[test]
    fn mistakes_are_named() {
        let err = |t: &str| file_args(t).unwrap_err();
        assert!(err("net-bo-prt = 30005\n").contains("unknown option \"net-bo-prt\""));
        assert!(err("fix = 1\n").contains("want true or false"));
        assert!(err("gain = true\n").contains("takes a value"));
        assert!(err("[net]\nbo-port = 1\n").contains("sections are not used"));
        assert!(
            err("config = \"x.toml\"\n").contains("unknown option"),
            "no nesting of files"
        );
        assert!(
            err("lat = \n").contains("TOML parse error") || err("lat = \n").contains("invalid")
        );
    }

    #[test]
    fn the_command_line_overrides_the_file() {
        let dir = std::env::temp_dir().join(format!("openradome-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("r.toml");
        std::fs::write(&file, "gain = 40.2\nlat = 51.5\nquiet = true\n").unwrap();
        let path = file.to_str().unwrap();
        let expanded = expand(args(&["openradome", "--config", path, "--gain", "49.6"])).unwrap();
        let cli = Cli::try_parse_from(&expanded).unwrap();
        assert_eq!(cli.gain, Some(49.6), "the command line wins");
        assert_eq!(cli.lat, Some(51.5));
        assert!(cli.quiet);
        let e = expand(args(&[
            "openradome",
            &format!("--config={}", dir.join("none.toml").display()),
        ]));
        assert!(e.unwrap_err().contains("none.toml"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What --print-config writes reads back as the same options.
    #[test]
    fn print_config_round_trips() {
        let given = args(&[
            "openradome",
            "--device-type",
            "rtlsdr",
            "--gain",
            "49.6",
            "--lat",
            "51.5",
            "--lon",
            "-0.1",
            "--fix",
            "--net",
            "--net-bind-address",
            "127.0.0.1",
            "--net-bi-port",
            "30004,30104",
            "--net-bo-port",
            "30005",
            "--net-connector",
            "a,1,beast_in",
            "--net-connector=b,2,raw_in",
            "--write-json",
            "/run/openradome",
            "--quiet",
            "--gain",
            "40.2",
            "--print-config",
        ]);
        let text = to_toml(&given).unwrap();
        assert!(text.contains("gain = 40.2\n"), "the last value: {text}");
        assert!(
            text.contains("net-bo-port = 30005\n")
                && text.contains("net-bi-port = \"30004,30104\"\n")
        );
        assert!(
            text.contains("net-connector = [\"a,1,beast_in\", \"b,2,raw_in\"]\n"),
            "{text}"
        );
        assert!(
            text.contains("fix = true\n") && !text.contains("print-config"),
            "{text}"
        );
        let back = file_args(&text).unwrap();
        let a = Cli::try_parse_from(std::iter::once("openradome".to_string()).chain(back)).unwrap();
        let b = Cli::try_parse_from(&given[..given.len() - 1]).unwrap();
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }
}
