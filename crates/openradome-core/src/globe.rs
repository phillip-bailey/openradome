//! The globe index: how tar1090 splits the world into tiles (`globe_XXXX.binCraft`).
//!
//! A port of `globe_index()` and `init_globe_index()` from upstream's `globe_index.c`. Tiles 0..60 are
//! hand-drawn regions over busy areas (the table below is upstream's, in the same order); the rest is a
//! 3 degree grid numbered from 1000.

/// Grid size in degrees (`GLOBE_INDEX_GRID`).
pub const GRID: i32 = 3;
/// `GLOBE_MIN_INDEX`: the first grid tile.
pub const MIN_INDEX: i32 = 1000;
const LAT_MULT: i32 = 360 / GRID + 1;
/// `GLOBE_MAX_INDEX`
pub const MAX_INDEX: i32 = 180 / GRID * LAT_MULT + MIN_INDEX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub south: i32,
    pub west: i32,
    pub north: i32,
    pub east: i32,
}

/// Upstream's special tiles, in index order.
pub const SPECIAL_TILES: [Tile; 60] = [
    Tile {
        south: 60,
        west: -126,
        north: 90,
        east: 0,
    },
    Tile {
        south: 60,
        west: 0,
        north: 90,
        east: 150,
    },
    Tile {
        south: 51,
        west: 150,
        north: 90,
        east: -126,
    },
    Tile {
        south: 9,
        west: 150,
        north: 51,
        east: -126,
    },
    Tile {
        south: 51,
        west: -126,
        north: 60,
        east: -69,
    },
    Tile {
        south: 45,
        west: -120,
        north: 51,
        east: -114,
    },
    Tile {
        south: 45,
        west: -114,
        north: 51,
        east: -102,
    },
    Tile {
        south: 45,
        west: -102,
        north: 51,
        east: -90,
    },
    Tile {
        south: 45,
        west: -90,
        north: 51,
        east: -75,
    },
    Tile {
        south: 45,
        west: -75,
        north: 51,
        east: -69,
    },
    Tile {
        south: 42,
        west: 12,
        north: 48,
        east: 18,
    },
    Tile {
        south: 42,
        west: 18,
        north: 48,
        east: 24,
    },
    Tile {
        south: 48,
        west: 18,
        north: 54,
        east: 24,
    },
    Tile {
        south: 54,
        west: 12,
        north: 60,
        east: 24,
    },
    Tile {
        south: 54,
        west: 3,
        north: 60,
        east: 12,
    },
    Tile {
        south: 54,
        west: -9,
        north: 60,
        east: 3,
    },
    Tile {
        south: 42,
        west: -9,
        north: 48,
        east: 0,
    },
    Tile {
        south: 42,
        west: 24,
        north: 51,
        east: 51,
    },
    Tile {
        south: 51,
        west: 24,
        north: 60,
        east: 51,
    },
    Tile {
        south: 30,
        west: 51,
        north: 60,
        east: 90,
    },
    Tile {
        south: 30,
        west: 90,
        north: 60,
        east: 120,
    },
    Tile {
        south: 30,
        west: 120,
        north: 39,
        east: 129,
    },
    Tile {
        south: 30,
        west: 129,
        north: 39,
        east: 138,
    },
    Tile {
        south: 30,
        west: 138,
        north: 39,
        east: 150,
    },
    Tile {
        south: 39,
        west: 120,
        north: 60,
        east: 150,
    },
    Tile {
        south: 9,
        west: 90,
        north: 21,
        east: 111,
    },
    Tile {
        south: 21,
        west: 90,
        north: 30,
        east: 111,
    },
    Tile {
        south: 9,
        west: 111,
        north: 24,
        east: 129,
    },
    Tile {
        south: 24,
        west: 111,
        north: 30,
        east: 120,
    },
    Tile {
        south: 24,
        west: 120,
        north: 30,
        east: 129,
    },
    Tile {
        south: 9,
        west: 129,
        north: 30,
        east: 150,
    },
    Tile {
        south: 9,
        west: 51,
        north: 30,
        east: 69,
    },
    Tile {
        south: 9,
        west: 69,
        north: 30,
        east: 90,
    },
    Tile {
        south: -90,
        west: -30,
        north: 9,
        east: 51,
    },
    Tile {
        south: -90,
        west: 51,
        north: 9,
        east: 111,
    },
    Tile {
        south: -90,
        west: 111,
        north: -18,
        east: 160,
    },
    Tile {
        south: -18,
        west: 111,
        north: 9,
        east: 160,
    },
    Tile {
        south: -90,
        west: 160,
        north: -42,
        east: -90,
    },
    Tile {
        south: -42,
        west: 160,
        north: 9,
        east: -90,
    },
    Tile {
        south: -9,
        west: -90,
        north: 9,
        east: -42,
    },
    Tile {
        south: -90,
        west: -90,
        north: -9,
        east: -63,
    },
    Tile {
        south: -21,
        west: -63,
        north: -9,
        east: -42,
    },
    Tile {
        south: -90,
        west: -63,
        north: -21,
        east: -42,
    },
    Tile {
        south: -90,
        west: -42,
        north: 9,
        east: -30,
    },
    Tile {
        south: 9,
        west: -126,
        north: 33,
        east: -117,
    },
    Tile {
        south: 9,
        west: -117,
        north: 30,
        east: -102,
    },
    Tile {
        south: 9,
        west: -102,
        north: 27,
        east: -90,
    },
    Tile {
        south: 24,
        west: -90,
        north: 30,
        east: -84,
    },
    Tile {
        south: 9,
        west: -90,
        north: 18,
        east: -69,
    },
    Tile {
        south: 18,
        west: -90,
        north: 24,
        east: -69,
    },
    Tile {
        south: 36,
        west: 6,
        north: 42,
        east: 18,
    },
    Tile {
        south: 36,
        west: 18,
        north: 42,
        east: 30,
    },
    Tile {
        south: 9,
        west: -9,
        north: 39,
        east: 6,
    },
    Tile {
        south: 9,
        west: 6,
        north: 36,
        east: 30,
    },
    Tile {
        south: 9,
        west: 30,
        north: 42,
        east: 51,
    },
    Tile {
        south: 24,
        west: -75,
        north: 39,
        east: -69,
    },
    Tile {
        south: 9,
        west: -69,
        north: 30,
        east: -33,
    },
    Tile {
        south: 30,
        west: -69,
        north: 60,
        east: -33,
    },
    Tile {
        south: 9,
        west: -33,
        north: 30,
        east: -9,
    },
    Tile {
        south: 30,
        west: -33,
        north: 60,
        east: -9,
    },
];

/// The tile for a position (`globe_index`).
pub fn globe_index(lat_in: f64, lon_in: f64) -> i32 {
    let lat = GRID * (((lat_in + 90.0) / GRID as f64) as i32) - 90;
    let lon = GRID * (((lon_in + 180.0) / GRID as f64) as i32) - 180;
    for (i, t) in SPECIAL_TILES.iter().enumerate() {
        if lat >= t.south && lat < t.north {
            if t.west < t.east && lon >= t.west && lon < t.east {
                return i as i32;
            }
            if t.west > t.east && (lon >= t.west || lon < t.east) {
                return i as i32;
            }
        }
    }
    let i = (lat + 90) / GRID;
    let j = (lon + 180) / GRID;
    let res = i * LAT_MULT + j + MIN_INDEX;
    if res > MAX_INDEX { 0 } else { res }
}

/// `globe_index_index`: the tile of a grid tile's south-west corner (grid tiles covered by a special
/// tile map to that tile's index instead).
pub fn globe_index_index(index: i32) -> i32 {
    let lat = ((index - MIN_INDEX) / LAT_MULT) * GRID - 90;
    let lon = ((index - MIN_INDEX) % LAT_MULT) * GRID - 180;
    globe_index(lat as f64, lon as f64)
}

/// Every tile that gets a file: the special tiles, then each grid tile not covered by one.
pub fn all_indexes() -> Vec<i32> {
    let mut v: Vec<i32> = (0..SPECIAL_TILES.len() as i32).collect();
    for i in MIN_INDEX..=MAX_INDEX {
        if globe_index_index(i) == i {
            v.push(i);
        }
    }
    v
}

/// The bounds (south, west, north, east) of a tile, as written in the binCraft header.
pub fn bounds(index: i32) -> (i16, i16, i16, i16) {
    if index >= MIN_INDEX {
        let south = ((index - MIN_INDEX) / LAT_MULT) * GRID - 90;
        let west = ((index - MIN_INDEX) % LAT_MULT) * GRID - 180;
        (
            south as i16,
            west as i16,
            (south + GRID) as i16,
            (west + GRID) as i16,
        )
    } else if let Some(t) = usize::try_from(index)
        .ok()
        .and_then(|i| SPECIAL_TILES.get(i))
    {
        (t.south as i16, t.west as i16, t.north as i16, t.east as i16)
    } else {
        (-90, -180, 90, 180)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_upstreams() {
        assert_eq!(SPECIAL_TILES.len(), 60);
        assert_eq!(
            SPECIAL_TILES[0],
            Tile {
                south: 60,
                west: -126,
                north: 90,
                east: 0
            }
        );
    }

    #[test]
    fn positions_land_in_tiles() {
        // the Arctic tile
        assert_eq!(globe_index(75.0, -50.0), 0);
        // a grid tile's centre maps back to the tile, and its bounds are 3 degrees
        let i = *all_indexes().iter().find(|&&i| i >= MIN_INDEX).unwrap();
        let (s, w, n, e) = bounds(i);
        assert_eq!((n - s, e - w), (3, 3));
        assert_eq!(globe_index(s as f64 + 1.5, w as f64 + 1.5), i);
        // every position maps to a tile whose file exists
        let all = all_indexes();
        for lat in (-90..90).step_by(7) {
            for lon in (-180..180).step_by(11) {
                let t = globe_index(lat as f64 + 0.5, lon as f64 + 0.5);
                assert!(all.contains(&t), "{lat},{lon} -> {t}");
            }
        }
    }
}

#[cfg(test)]
mod count_tests {
    use super::*;

    #[test]
    fn the_special_tiles_cover_most_of_the_grid() {
        // 60 special tiles plus the 3 degree grid tiles they leave uncovered (upstream's tile set)
        assert_eq!(all_indexes().len(), 192);
        // Sydney is inside a special tile, so it has no grid tile of its own
        assert!(globe_index(-33.5, 151.5) < MIN_INDEX);
    }
}
