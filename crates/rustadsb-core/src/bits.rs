//! Bit field access with 1-based, MSB-first bit numbers, as used in the ICAO
//! Annex 10 field tables (and upstream's `getbit`/`getbits`).

/// Bit `bitnum` (1 = most significant bit of `data[0]`).
#[inline]
pub fn getbit(data: &[u8], bitnum: u32) -> bool {
    let bi = bitnum - 1;
    data[(bi >> 3) as usize] & (1 << (7 - (bi & 7))) != 0
}

/// Bits `first..=last` (1-based, inclusive, at most 32 wide) as an integer.
#[inline]
pub fn getbits(data: &[u8], first: u32, last: u32) -> u32 {
    debug_assert!(first >= 1 && first <= last && last - first < 32);
    let fbi = first - 1;
    let lbi = last - 1;
    let mut v: u64 = 0;
    for &b in &data[(fbi >> 3) as usize..=(lbi >> 3) as usize] {
        v = (v << 8) | b as u64;
    }
    v >>= 7 - (lbi & 7);
    (v & ((1u64 << (last - first + 1)) - 1)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_numbering() {
        let d = [0b1010_0000, 0x01];
        assert!(getbit(&d, 1));
        assert!(!getbit(&d, 2));
        assert!(getbit(&d, 3));
        assert!(getbit(&d, 16));
    }

    #[test]
    fn fields() {
        let d = [0x8D, 0x48, 0x40, 0xD6, 0x20];
        assert_eq!(getbits(&d, 1, 5), 17);
        assert_eq!(getbits(&d, 6, 8), 5);
        assert_eq!(getbits(&d, 9, 32), 0x4840D6);
        assert_eq!(getbits(&d, 5, 36), 0xD4840D62);
        assert_eq!(getbits(&d, 9, 40), 0x4840D620);
    }
}
