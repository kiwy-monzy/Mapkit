//! Mapbox terrain-rgb DEM: decode → stitched metre grid → binary save/load.

use crate::mercator::{lonlat_to_tile, tile_span_m, Mercator};
use std::io::Read as _;

/// raw Mercator y of a latitude (unit sphere — spans cancel in row math)
pub fn merc_y(lat_deg: f64) -> f64 {
    lat_deg.to_radians().tan().asinh()
}

pub struct Grid {
    pub w: usize,
    pub h: usize,
    /// metres per cell (ground)
    pub cell: f32,
    /// lat/lon of the grid ORIGIN (x0,y0 = sample [0][0], y grows SOUTH)
    pub lat0: f64,
    pub lon0: f64,
    /// Mercator-y of the north edge and the grid's total Mercator-y span.
    /// Slippy-tile rows are uniform in MERCATOR space, not in ground metres —
    /// finding rows by a uniform ground cell tilts the sampled terrain
    /// against the basemap drawn over it (9 m over Dar es Salaam, per the
    /// mapmap-scene DEM loader that pins this down). 0.0 = unknown (grids
    /// saved by older builds) → the sample falls back to the uniform cell.
    pub merc_north: f64,
    pub merc_span: f64,
    pub data: Vec<f32>,
}

impl Grid {
    pub fn sample(&self, lon: f64, lat: f64) -> f32 {
        let mx = Mercator::new(self.lat0, self.lon0);
        let (dx, _) = mx.project(lon, lat);
        // longitude IS linear in Mercator x — the column is exact as-is
        let gx = dx / self.cell as f64;
        // rows: find in Mercator space (see merc_north/merc_span docs)
        let gy = if self.merc_span != 0.0 {
            (self.merc_north - merc_y(lat)) / self.merc_span * self.h as f64
        } else {
            let (_, dy) = mx.project(lon, lat);
            -dy / self.cell as f64 // y north → row south
        };
        self.sample_xy(gx as f32, gy as f32)
    }

    pub fn sample_xy(&self, x: f32, y: f32) -> f32 {
        if x < 0.0 || y < 0.0 || x >= self.w as f32 - 1.0 || y >= self.h as f32 - 1.0 {
            return f32::NAN;
        }
        let x0 = x.floor() as usize;
        let y0 = y.floor() as usize;
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        // NaN-aware bilinear: nodata samples are skipped and the weights
        // renormalized; all-NaN neighbourhood → NaN (filled later).
        let g = |xx: usize, yy: usize| self.data[yy * self.w + xx];
        let mut acc = 0.0f32;
        let mut wsum = 0.0f32;
        let corners = [
            (g(x0, y0), (1.0 - fx) * (1.0 - fy)),
            (g(x0 + 1, y0), fx * (1.0 - fy)),
            (g(x0, y0 + 1), (1.0 - fx) * fy),
            (g(x0 + 1, y0 + 1), fx * fy),
        ];
        for (v, w) in corners {
            if v.is_finite() {
                acc += v * w;
                wsum += w;
            }
        }
        if wsum > 0.0 {
            acc / wsum
        } else {
            f32::NAN
        }
    }
}

/// decode a terrain-rgb PNG (256x256) to metres
pub fn decode_terrain_png(raw: &[u8]) -> Option<Vec<f32>> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(raw));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut rdr = dec.read_info().ok()?;
    let mut buf = vec![0u8; rdr.output_buffer_size()];
    let info = rdr.next_frame(&mut buf).ok()?;
    let w = info.width as usize;
    let h = info.height as usize;
    let bpp = info.color_type.samples();
    if info.color_type != png::ColorType::Rgb && info.color_type != png::ColorType::Rgba {
        return None;
    }
    let mut out = vec![0f32; w * h];
    for i in 0..w * h {
        let r = buf[i * bpp] as u32;
        let g = buf[i * bpp + 1] as u32;
        let b = buf[i * bpp + 2] as u32;
        out[i] = -10_000.0 + ((r * 65536 + g * 256 + b) as f32) * 0.1;
    }
    Some(out)
}

/// stitch a window of terrain tiles into one grid.
/// (z, tile x0..x1 exclusive, y0..y1 exclusive)
///
/// Nodata policy (the sunk-terrain fix): missing/undecodable tiles AND tiles
/// that decode to a UNIFORM exactly-0.0 grid become NaN — never 0.0. Mapbox
/// serves flat-0 tiles for ocean (no bathymetry in this delivery) and a 0.0
/// that reaches the carve step becomes an ~8 m sea pit, stamping rectangular
/// holes into coastal land. The caller gap-fills NaN from valid neighbours
/// ([`fill_missing`]) and gives the unfillable remainder real sea depth.
pub fn stitch(z: u32, x0: u32, y0: u32, x1: u32, y1: u32, cache: &str) -> Option<Grid> {
    let tw = (x1 - x0) as usize * 256;
    let th = (y1 - y0) as usize * 256;
    let mut data = vec![f32::NAN; tw * th];
    let mut got = 0usize;
    let mut flat_zero = 0usize;
    for ty in y0..y1 {
        for tx in x0..x1 {
            let path = format!("{}/dem_z{}_{}_{}.png", cache, z, tx, ty);
            let Ok(raw) = std::fs::read(&path) else { continue };
            let Some(px) = decode_terrain_png(&raw) else { continue };
            if is_uniform_zero(&px) {
                // ocean mask tile, not land at 0 m — leave NaN for the fill
                flat_zero += 1;
                continue;
            }
            let bx = (tx - x0) as usize * 256;
            let by = (ty - y0) as usize * 256;
            for py in 0..256 {
                for px2 in 0..256 {
                    data[(by + py) * tw + bx + px2] = px[py * 256 + px2];
                }
            }
            got += 1;
        }
    }
    if got == 0 {
        return None;
    }
    if flat_zero > 0 {
        eprintln!("stitch: {flat_zero} flat-0 ocean tile(s) marked nodata (gap-filled later)");
    }
    // origin lat/lon of tile (x0,y0) top-left corner
    let n = (1u64 << z) as f64;
    let lon0 = x0 as f64 / n * 360.0 - 180.0;
    let lat_top = ((1.0 - 2.0 * y0 as f64 / n) * std::f64::consts::PI)
        .sinh()
        .atan()
        .to_degrees();
    let lat_bottom = ((1.0 - 2.0 * y1 as f64 / n) * std::f64::consts::PI)
        .sinh()
        .atan()
        .to_degrees();
    // ground metres per cell: tile span / 256
    let cell = (tile_span_m(lat_top, z) / 256.0) as f32;
    let merc_north = merc_y(lat_top);
    let merc_span = merc_north - merc_y(lat_bottom);
    Some(Grid {
        w: tw,
        h: th,
        cell,
        lat0: lat_top,
        lon0,
        merc_north,
        merc_span,
        data,
    })
}

/// resample a stitched grid onto a regular ground-metre window around a centre
pub fn resample_window(g: &Grid, m: &Mercator, span_m: f64, out_cells: usize) -> Grid {
    let cell = (span_m / out_cells as f64) as f32;
    let mut data = vec![0f32; out_cells * out_cells];
    for r in 0..out_cells {
        for c in 0..out_cells {
            let x = (c as f64 + 0.5) * cell as f64 - span_m / 2.0;
            let y = (r as f64 + 0.5) * cell as f64 - span_m / 2.0;
            // world metres (x east, y north) → lon/lat via inverse approx
            let (lon, lat) = m.unproject(x, y);
            data[r * out_cells + c] = g.sample(lon, lat);
        }
    }
    Grid {
        w: out_cells,
        h: out_cells,
        cell,
        lat0: 0.0,
        lon0: 0.0,
        merc_north: 0.0,
        merc_span: 0.0,
        data,
    }
}

/// true when every decoded sample is exactly 0.0 m — Mapbox's ocean mask,
/// not real land at sea level (sensor noise makes a natural all-zero
/// 256×256 tile essentially impossible).
pub fn is_uniform_zero(px: &[f32]) -> bool {
    !px.is_empty() && px.iter().all(|v| *v == 0.0)
}

/// Fill NaN (nodata) cells from valid 8-neighbours, one ring per pass.
/// `passes` bounds how far the fill reaches: coastal gaps close up while
/// the open ocean stays NaN for the caller to depth explicitly.
/// Returns the number of cells still NaN.
pub fn fill_missing(g: &mut Grid, passes: usize) -> usize {
    let (w, h) = (g.w, g.h);
    for _ in 0..passes {
        let mut filled = 0usize;
        let src = g.data.clone();
        for r in 0..h {
            for c in 0..w {
                let i = r * w + c;
                if src[i].is_finite() {
                    continue;
                }
                let mut sum = 0f64;
                let mut n = 0usize;
                for dr in -1i64..=1 {
                    for dc in -1i64..=1 {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        let rr = r as i64 + dr;
                        let cc = c as i64 + dc;
                        if rr < 0 || cc < 0 || rr >= h as i64 || cc >= w as i64 {
                            continue;
                        }
                        let v = src[rr as usize * w + cc as usize];
                        if v.is_finite() {
                            sum += v as f64;
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    g.data[i] = (sum / n as f64) as f32;
                    filled += 1;
                }
            }
        }
        if filled == 0 {
            break;
        }
    }
    g.data.iter().filter(|v| !v.is_finite()).count()
}

/// pure Terrarium elevation math (AWS elevation-tiles-prod):
/// elev = R*256 + G + B/256 − 32768
pub fn terrarium_elev(r: u8, g: u8, b: u8) -> f32 {
    r as f32 * 256.0 + g as f32 + b as f32 / 256.0 - 32768.0
}

/// decode a Terrarium PNG (256×256) to metres
pub fn decode_terrarium_png(raw: &[u8]) -> Option<Vec<f32>> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(raw));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut rdr = dec.read_info().ok()?;
    let mut buf = vec![0u8; rdr.output_buffer_size()];
    let info = rdr.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w != 256 || h != 256 {
        return None;
    }
    let bpp = info.color_type.samples();
    if info.color_type != png::ColorType::Rgb && info.color_type != png::ColorType::Rgba {
        return None;
    }
    let mut out = vec![0f32; w * h];
    for i in 0..w * h {
        out[i] = terrarium_elev(buf[i * bpp], buf[i * bpp + 1], buf[i * bpp + 2]);
    }
    Some(out)
}

/// encode elevations as a Mapbox terrain-RGB PNG so Terrarium backfill can
/// reuse the standard `dem_z{z}_{x}_{y}.png` cache + decoder path.
pub fn encode_terrain_rgb_png(elev: &[f32]) -> Option<Vec<u8>> {
    if elev.len() != 256 * 256 {
        return None;
    }
    let mut raw = vec![0u8; 256 * 256 * 3];
    for (i, e) in elev.iter().enumerate() {
        let v = ((e + 10_000.0) * 10.0).round().clamp(0.0, 16_777_215.0) as u32;
        raw[i * 3] = ((v >> 16) & 0xFF) as u8;
        raw[i * 3 + 1] = ((v >> 8) & 0xFF) as u8;
        raw[i * 3 + 2] = (v & 0xFF) as u8;
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, 256, 256);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .ok()?
        .write_image_data(&raw)
        .ok()?;
    Some(out)
}

/// Backfill nodata DEM tiles from AWS Terrarium: any tile in the window that
/// is missing OR decodes uniform-0 is refetched from
/// elevation-tiles-prod, converted to terrain-RGB and cached under the
/// standard name. Returns the number of tiles replaced.
pub fn backfill_ocean_tiles(cache: &str, z: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
    let mut fixed = 0usize;
    for ty in y0..y1 {
        for tx in x0..x1 {
            let path = format!("{cache}/dem_z{z}_{tx}_{ty}.png");
            let needs = match std::fs::read(&path) {
                Ok(raw) => decode_terrain_png(&raw).map(|px| is_uniform_zero(&px)).unwrap_or(true),
                Err(_) => true,
            };
            if !needs {
                continue;
            }
            let url = format!("https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{tx}/{ty}.png");
            let body: Result<Vec<u8>, String> = (|| {
                let resp = ureq::get(&url)
                    .timeout(std::time::Duration::from_secs(30))
                    .call()
                    .map_err(|e| format!("get {url}: {e}"))?;
                let mut buf = Vec::new();
                resp.into_reader()
                    .take(8 * 1024 * 1024)
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("read {url}: {e}"))?;
                Ok(buf)
            })();
            let Ok(body) = body else { continue };
            if body.starts_with(b"{") || body.len() < 100 {
                continue;
            }
            let Some(elev) = decode_terrarium_png(&body) else { continue };
            if is_uniform_zero(&elev) {
                continue; // Terrarium masked too — leave NaN for the fill
            }
            let Some(png) = encode_terrain_rgb_png(&elev) else { continue };
            // atomic write (same discipline as the tile server cache)
            let tmp = format!("{path}.part");
            if std::fs::write(&tmp, &png).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
                // replace only when the new tile actually carries relief
                fixed += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    if fixed > 0 {
        eprintln!("terrarium backfill: {fixed} DEM tile(s) replaced");
    }
    fixed
}

/// serialize an f32 grid: magic TZD1, w u32, h u32, cell f32, mode u32, data…
pub fn save_bin_bytes(g: &Grid, mode: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + g.data.len() * 4);
    out.extend_from_slice(b"TZD1");
    out.extend_from_slice(&(g.w as u32).to_le_bytes());
    out.extend_from_slice(&(g.h as u32).to_le_bytes());
    out.extend_from_slice(&g.cell.to_le_bytes());
    out.extend_from_slice(&mode.to_le_bytes());
    for v in &g.data {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn save_bin(g: &Grid, mode: u32, path: &str) -> std::io::Result<()> {
    std::fs::write(path, save_bin_bytes(g, mode))
}

/// load an f32 grid (TZD1) from raw bytes
pub fn load_bin_bytes(b: &[u8]) -> Option<(Grid, u32)> {
    if b.len() < 20 || &b[0..4] != b"TZD1" {
        return None;
    }
    let w = u32::from_le_bytes(b[4..8].try_into().ok()?) as usize;
    let h = u32::from_le_bytes(b[8..12].try_into().ok()?) as usize;
    let cell = f32::from_le_bytes(b[12..16].try_into().ok()?);
    let mode = u32::from_le_bytes(b[16..20].try_into().ok()?);
    let n = w * h;
    if b.len() < 20 + n * 4 {
        return None;
    }
    let mut data = vec![0f32; n];
    for i in 0..n {
        data[i] = f32::from_le_bytes(b[20 + i * 4..24 + i * 4].try_into().ok()?);
    }
    Some((
        Grid {
            w,
            h,
            cell,
            lat0: 0.0,
            lon0: 0.0,
            merc_north: 0.0,
            merc_span: 0.0,
            data,
        },
        mode,
    ))
}

pub fn load_bin(path: &str) -> Option<(Grid, u32)> {
    let b = std::fs::read(path).ok()?;
    load_bin_bytes(&b)
}

/// window of tiles covering a lon/lat centre with a span (ground metres)
pub fn tile_window(lat: f64, lon: f64, z: u32, span_m: f64) -> (u32, u32, u32, u32) {
    let (cx, cy) = lonlat_to_tile(lon, lat, z);
    let tspan = tile_span_m(lat, z);
    let need = ((span_m / tspan).ceil() as u32).max(1) / 2 + 1;
    (
        cx.saturating_sub(need),
        cy.saturating_sub(need),
        cx + need + 1,
        cy + need + 1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows are uniform in MERCATOR space: a ramp grid sampled through
    /// merc_y lands on the exact row at every latitude. (The old
    /// uniform-ground-cell path drifts against this away from row 0 —
    /// the mapmap-scene finding: "tilts the terrain, 9 m over Dar".)
    #[test]
    fn rows_are_found_in_mercator_space() {
        let rows = 64usize;
        let lat_top = 5.0_f64;
        let lat_bottom = 2.0_f64; // ~3° of latitude south
        let merc_north = merc_y(lat_top);
        let merc_span = merc_north - merc_y(lat_bottom);
        // linear ramp: value = row index
        let data = (0..rows * 4).map(|i| (i / 4) as f32).collect::<Vec<_>>();
        let g = Grid {
            w: 4,
            h: rows,
            cell: 100.0,
            lat0: lat_top,
            lon0: 39.0,
            merc_north,
            merc_span,
            data,
        };
        // sample each row centre: bilinear between ramp rows r and r+1
        // yields r + 0.5 (the last row's centre sits on the guard band)
        for r in [0usize, 1, 16, 31, 47, 62] {
            let my = merc_north - (r as f64 + 0.5) / rows as f64 * merc_span;
            let lat = my.sinh().atan().to_degrees();
            let v = g.sample(39.001, lat);
            assert!(
                (v - (r as f32 + 0.5)).abs() < 0.25,
                "row {r}: sampled {v} at lat {lat:.4}"
            );
        }
    }

    /// and the merc_y roundtrip used by stitch()
    #[test]
    fn merc_y_roundtrips() {
        for lat in [-60.0, -6.888, 0.0, 5.0, 45.0] {
            let lat2 = merc_y(lat).sinh().atan().to_degrees();
            assert!((lat2 - lat).abs() < 1e-9);
        }
    }

    fn test_grid(w: usize, h: usize, fill: f32) -> Grid {
        Grid {
            w,
            h,
            cell: 10.0,
            lat0: 0.0,
            lon0: 0.0,
            merc_north: 0.0,
            merc_span: 0.0,
            data: vec![fill; w * h],
        }
    }

    #[test]
    fn uniform_zero_detects_ocean_mask() {
        assert!(is_uniform_zero(&vec![0.0; 256 * 256]));
        let mut almost = vec![0.0; 256 * 256];
        almost[1000] = 0.1;
        assert!(!is_uniform_zero(&almost));
        assert!(!is_uniform_zero(&[]));
    }

    #[test]
    fn fill_missing_closes_holes_but_not_ocean() {
        // 7x7 land at 10 m with a 3x3 nodata hole in the middle
        let mut g = test_grid(7, 7, 10.0);
        for r in 2..5 {
            for c in 2..5 {
                g.data[r * 7 + c] = f32::NAN;
            }
        }
        let left = fill_missing(&mut g, 4);
        assert_eq!(left, 0);
        assert!(g.data.iter().all(|v| (*v - 10.0).abs() < 1e-4));
    }

    #[test]
    fn fill_missing_is_bounded() {
        // whole grid nodata except one corner — 1 pass reaches 1 ring only
        let mut g = test_grid(9, 9, f32::NAN);
        g.data[0] = 5.0;
        let left = fill_missing(&mut g, 1);
        assert!(left > 0);
        assert!((g.data[1] - 5.0).abs() < 1e-4); // neighbour filled
        assert!(g.data[8 * 9 + 8].is_nan()); // far corner untouched
    }

    #[test]
    fn sample_xy_ignores_nan() {
        // 2x2 grid: three valid corners (10 m) + one NaN — centre ≈ 10 m,
        // not NaN and not dragged to 0
        let mut g = test_grid(2, 2, 10.0);
        g.data[3] = f32::NAN;
        let v = g.sample_xy(0.5, 0.5);
        assert!((v - 10.0).abs() < 1e-4, "got {v}");
        g.data = vec![f32::NAN; 4];
        assert!(g.sample_xy(0.5, 0.5).is_nan());
    }

    #[test]
    fn terrarium_math_and_roundtrip() {
        // sea level in Terrarium encoding: 32768 = R128 G0 B0
        assert!((terrarium_elev(128, 0, 0)).abs() < 1e-3);
        // Everest-ish sanity: R=136 G=12 B=128 → 136*256+12+0.5-32768
        let e = terrarium_elev(136, 12, 128);
        assert!((e - 2060.5).abs() < 1e-3);
        // terrain-RGB encode → decode roundtrip within quantisation
        let elev: Vec<f32> = (0..256 * 256).map(|i| (i as f32 % 251.0) - 120.0).collect();
        let png = encode_terrain_rgb_png(&elev).expect("encode");
        let back = decode_terrain_png(&png).expect("decode");
        for (a, b) in elev.iter().zip(back.iter()) {
            assert!((a - b).abs() <= 0.051, "{a} vs {b}");
        }
        // uniform zero survives the roundtrip (the ocean-mask signature)
        let flat = vec![0.0; 256 * 256];
        let back = decode_terrain_png(&encode_terrain_rgb_png(&flat).unwrap()).unwrap();
        assert!(is_uniform_zero(&back));
    }
}
