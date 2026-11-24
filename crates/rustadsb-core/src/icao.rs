//! Recently-seen ICAO address filter. Port of upstream `icao_filter.c`.
//!
//! Two generations of addresses: adds go to the active set, tests look at both,
//! and `expire` drops the older generation. Call `expire` periodically.

use std::collections::HashSet;

#[derive(Debug, Default, Clone)]
pub struct IcaoFilter {
    active: HashSet<u32>,
    previous: HashSet<u32>,
}

impl IcaoFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, addr: u32) {
        self.active.insert(addr);
    }

    pub fn test(&self, addr: u32) -> bool {
        self.active.contains(&addr) || self.previous.contains(&addr)
    }

    /// Age out addresses not re-added since the last call (they survive one call).
    pub fn expire(&mut self) {
        self.previous = std::mem::take(&mut self.active);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_survive_one_expiry() {
        let mut f = IcaoFilter::new();
        assert!(!f.test(0x4840d6));
        f.add(0x4840d6);
        assert!(f.test(0x4840d6));
        f.expire();
        assert!(f.test(0x4840d6));
        f.expire();
        assert!(!f.test(0x4840d6));
    }

    #[test]
    fn re_adding_keeps_it_alive() {
        let mut f = IcaoFilter::new();
        f.add(1);
        f.expire();
        f.add(1);
        f.expire();
        assert!(f.test(1));
    }
}
