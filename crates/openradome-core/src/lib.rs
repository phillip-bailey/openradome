//! Core ADS-B and Mode S decoder. No I/O, no async.

pub mod api;
pub mod beast;
pub mod bincraft;
pub mod bits;
pub mod comm_b;
pub mod convert;
pub mod cpr;
pub mod crc;
pub mod demod;
pub mod geo;
pub mod geomag;
pub mod globe;
pub mod icao;
pub mod json;
pub mod mode_ac;
pub mod mode_s;
pub mod output;
pub mod pipeline;
pub mod sbs;
pub mod synth;
pub mod trace;
pub mod track;

/// Crate version, for `--version` output.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
