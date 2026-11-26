//! Mode A/C helpers: Gillham code conversion and squawk formats.
//! Port of upstream `mode_ac.c` and the Mode A helpers from `track.h`.
//!
//! A "mode A value" is hex-encoded: `00:A4:A2:A1:00:B4:B2:B1:00:C4:C2:C1:00:D4:D2:D1`.

use std::sync::OnceLock;

/// Convert a (hex) Mode A value to a 0-4095 index.
pub fn mode_a_to_index(mode_a: u32) -> u32 {
    (mode_a & 0x0007)
        | ((mode_a & 0x0070) >> 1)
        | ((mode_a & 0x0700) >> 2)
        | ((mode_a & 0x7000) >> 3)
}

/// Convert a 0-4095 index to a (hex) Mode A value.
pub fn index_to_mode_a(index: u32) -> u32 {
    (index & 0o0007) | ((index & 0o0070) << 1) | ((index & 0o0700) << 2) | ((index & 0o7000) << 3)
}

/// Hex squawk (each hex digit is an octal digit) to decimal squawk, e.g. 0x7700 -> 7700.
pub fn squawk_hex_to_dec(s: u32) -> u32 {
    (s & 0xf000) / 0x1000 * 1000
        + (s & 0x0f00) / 0x100 * 100
        + (s & 0x00f0) / 0x10 * 10
        + (s & 0x000f)
}

/// Decimal squawk to hex squawk, e.g. 7700 -> 0x7700.
pub fn squawk_dec_to_hex(s: u32) -> u32 {
    (s / 1000 % 10) * 0x1000 + (s / 100 % 10) * 0x100 + (s / 10 % 10) * 0x10 + (s % 10)
}

fn internal_mode_a_to_mode_c(mode_a: u32) -> Option<i32> {
    // Zero bits must be zero (D1 set is illegal) and C1..C4 cannot all be zero.
    if mode_a & 0xFFFF_8889 != 0 || mode_a & 0x00F0 == 0 {
        return None;
    }

    let mut five_hundreds: u32 = 0;
    let mut one_hundreds: u32 = 0;

    if mode_a & 0x0010 != 0 {
        one_hundreds ^= 0x007;
    } // C1
    if mode_a & 0x0020 != 0 {
        one_hundreds ^= 0x003;
    } // C2
    if mode_a & 0x0040 != 0 {
        one_hundreds ^= 0x001;
    } // C4

    // Remove 7s from one_hundreds (7 -> 5, and 5 -> 7).
    if one_hundreds & 5 == 5 {
        one_hundreds ^= 2;
    }
    // Only 1 to 5 are valid.
    if one_hundreds > 5 {
        return None;
    }

    // D1 is never used for altitude.
    if mode_a & 0x0002 != 0 {
        five_hundreds ^= 0x0FF;
    } // D2
    if mode_a & 0x0004 != 0 {
        five_hundreds ^= 0x07F;
    } // D4
    if mode_a & 0x1000 != 0 {
        five_hundreds ^= 0x03F;
    } // A1
    if mode_a & 0x2000 != 0 {
        five_hundreds ^= 0x01F;
    } // A2
    if mode_a & 0x4000 != 0 {
        five_hundreds ^= 0x00F;
    } // A4
    if mode_a & 0x0100 != 0 {
        five_hundreds ^= 0x007;
    } // B1
    if mode_a & 0x0200 != 0 {
        five_hundreds ^= 0x003;
    } // B2
    if mode_a & 0x0400 != 0 {
        five_hundreds ^= 0x001;
    } // B4

    if five_hundreds & 1 != 0 {
        one_hundreds = 6 - one_hundreds;
    }

    Some((five_hundreds * 5 + one_hundreds) as i32 - 13)
}

struct Tables {
    a_to_c: Vec<Option<i32>>,
    c_to_a: Vec<u32>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut a_to_c = vec![None; 4096];
        let mut c_to_a = vec![0u32; 4096];
        for i in 0..4096u32 {
            let mode_a = index_to_mode_a(i);
            let mode_c = internal_mode_a_to_mode_c(mode_a);
            a_to_c[i as usize] = mode_c;
            if let Some(c) = mode_c {
                let c = c + 13;
                if (0..4096).contains(&c) {
                    debug_assert_eq!(c_to_a[c as usize], 0);
                    c_to_a[c as usize] = mode_a;
                }
            }
        }
        Tables { a_to_c, c_to_a }
    })
}

/// Mode A value (hex-encoded) to Mode C, in units of 100 ft. `None` if not a valid Mode C code.
pub fn mode_a_to_mode_c(mode_a: u32) -> Option<i32> {
    let i = mode_a_to_index(mode_a);
    if i >= 4096 {
        return None;
    }
    tables().a_to_c[i as usize]
}

/// Mode C (units of 100 ft) to Mode A value, or 0 if not representable.
pub fn mode_c_to_mode_a(mode_c: i32) -> u32 {
    let c = mode_c + 13;
    if !(0..4096).contains(&c) {
        return 0;
    }
    tables().c_to_a[c as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_round_trip() {
        for i in 0..4096 {
            assert_eq!(mode_a_to_index(index_to_mode_a(i)), i);
        }
    }

    #[test]
    fn squawk_conversion() {
        assert_eq!(squawk_hex_to_dec(0x7700), 7700);
        assert_eq!(squawk_hex_to_dec(0x1200), 1200);
        assert_eq!(squawk_dec_to_hex(7500), 0x7500);
        for d in [0, 1200, 7000, 7777] {
            assert_eq!(squawk_hex_to_dec(squawk_dec_to_hex(d)), d);
        }
    }

    #[test]
    fn gillham_known_values() {
        // Mode A 0x0040 (only C4 set) is the lowest valid code: -1200 ft = -12 (100s).
        assert_eq!(mode_a_to_mode_c(0x0040), Some(-12));
        assert_eq!(mode_a_to_mode_c(0x0010), Some(-8)); // C1 only
        // Illegal: no C bits, or D1 set.
        assert_eq!(mode_a_to_mode_c(0x0000), None);
        assert_eq!(mode_a_to_mode_c(0x0011), None);
        // Altitude is monotonic over valid codes, steps of one (100 ft), and invertible.
        let mut seen = 0;
        for c in -12..=1267 {
            let a = mode_c_to_mode_a(c);
            if a != 0 {
                assert_eq!(mode_a_to_mode_c(a), Some(c));
                seen += 1;
            }
        }
        assert_eq!(seen, 1280, "every level -12..=1267 has a code");
    }
}
