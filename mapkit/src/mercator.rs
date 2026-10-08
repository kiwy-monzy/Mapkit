//! THE one Mercator projection — spherical Web Mercator rescaled to ground
//! metres at the zone centre. 1 world unit = 1 ground metre at (lat0, lon0).
//! Used identically by the exporter and by the Egregoria engine loader.

pub const EARTH_C: f64 = 40_075_016.685_578_6; // equatorial circumference, metres

#[derive(Clone, Copy)]
pub struct Mercator {
    pub lat0: f64,
    pub lon0: f64,
    k: f64,
    mx0: f64,
    my0: f64,
}

impl Mercator {
    pub fn new(lat0: f64, lon0: f64) -> Self {
        let k = lat0.to_radians().cos();
        let mx0 = lon_to_mx(lon0);
        let my0 = lat_to_my(lat0);
        Mercator { lat0, lon0, k, mx0, my0 }
    }

    /// lat/lon (deg) → world metres (x east, y north)
    pub fn project(&self, lon: f64, lat: f64) -> (f64, f64) {
        let mx = lon_to_mx(lon);
        let my = lat_to_my(lat);
        ((mx - self.mx0) * self.k, (my - self.my0) * self.k)
    }

    /// world metres → lat/lon (deg)
    pub fn unproject(&self, x: f64, y: f64) -> (f64, f64) {
        let mx = x / self.k + self.mx0;
        let my = y / self.k + self.my0;
        let lon = mx / EARTH_C * 360.0;
        let phi = (my / (EARTH_C / (2.0 * std::f64::consts::PI))).sinh().atan();
        (lon, phi.to_degrees())
    }
}

fn lon_to_mx(lon: f64) -> f64 {
    lon / 360.0 * EARTH_C
}

fn lat_to_my(lat: f64) -> f64 {
    let phi = lat.to_radians();
    (phi.tan().asinh()) * (EARTH_C / (2.0 * std::f64::consts::PI))
}

/// tile x/y for a lon/lat at zoom z
pub fn lonlat_to_tile(lon: f64, lat: f64, z: u32) -> (u32, u32) {
    let n = (1u64 << z) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor().max(0.0) as u32;
    let lat = lat.to_radians();
    let y = ((1.0 - lat.tan().asinh() / std::f64::consts::PI) / 2.0 * n)
        .floor()
        .clamp(0.0, n - 1.0) as u32;
    (x, y)
}

/// metres per tile edge at a latitude / zoom (ground metres)
pub fn tile_span_m(lat: f64, z: u32) -> f64 {
    EARTH_C * lat.to_radians().cos() / (1u64 << z) as f64
}
