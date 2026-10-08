//! Tile prefetch — the brush backend.
//!
//!   POST /api/tiles/prefetch { lat, lon, radiusM, kinds? }
//!
//! Fetches the vector (z15+z14, buildings live at z15) and terrain
//! (z14+z13+z12, every DEM zoom a grab may consume) tiles covering a disc,
//! so the web UI's brush can fill missing spots before preview/grab.
//! Terrain tiles that arrive as flat-0 ocean masks are immediately replaced
//! via the Terrarium backfill, so brushed coast gets real relief.
//! Already-cached valid tiles are not refetched. Bounded: at most
//! MAX_FETCH remote fetches per request (`capped: true` when more are
//! missing — brush a smaller spot or repeat).

use std::time::Duration;

use rocket::http::Status;
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::serde::Deserialize;
use rocket::State;
use serde_json::{json, Value};

use super::db;
use super::ServerState;

const TILE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RADIUS_M: f64 = 5000.0;
const MAX_FETCH: usize = 40;

#[derive(Deserialize)]
pub struct PrefetchReq {
    pub lat: f64,
    pub lon: f64,
    #[serde(default = "default_radius")]
    pub radius_m: f64,
    /// subset of ["vector", "terrain"] (default both)
    #[serde(default)]
    pub kinds: Vec<String>,
}

fn default_radius() -> f64 {
    500.0
}

fn err(code: Status, msg: impl Into<String>) -> status::Custom<Json<Value>> {
    status::Custom(code, Json(json!({ "error": msg.into() })))
}

/// tile window at zoom z covering a disc (clamped to the valid range)
fn disc_window(lat: f64, lon: f64, radius_m: f64, z: u32) -> Vec<(u32, u32)> {
    let n = 1u32 << z;
    let (cx, cy) = crate::mercator::lonlat_to_tile(lon, lat, z);
    let half = ((radius_m / crate::mercator::tile_span_m(lat, z)).ceil() as i64 + 1).max(1);
    let mut out = Vec::new();
    for dy in -half..=half {
        for dx in -half..=half {
            let (x, y) = (cx as i64 + dx, cy as i64 + dy);
            if x >= 0 && y >= 0 && x < n as i64 && y < n as i64 {
                out.push((x as u32, y as u32));
            }
        }
    }
    out
}

fn valid_vector(bytes: &[u8]) -> bool {
    !(bytes.starts_with(b"{") || bytes.len() < 100)
}

fn valid_terrain(bytes: &[u8]) -> bool {
    !(bytes.starts_with(b"{") || bytes.len() < 500)
}

#[allow(clippy::too_many_arguments)]
fn fetch_remote(
    st: &ServerState,
    kind: &str,
    z: u32,
    x: u32,
    y: u32,
    path: std::path::PathBuf,
    url: String,
    valid: fn(&[u8]) -> bool,
    source: String,
) -> Result<(), String> {
    let bytes = super::upstream::fetch_mapbox(&url, TILE_TIMEOUT)
        .map_err(|e| format!("{kind} {z}/{x}/{y}: {e}"))?;
    if !valid(&bytes) {
        return Err(format!("{kind} {z}/{x}/{y}: upstream error body"));
    }
    super::upstream::write_atomic(&path, &bytes)
        .map_err(|e| format!("{kind} {z}/{x}/{y}: cache write: {e}"))?;
    let mut conn = st.conn.lock().unwrap();
    db::record_tile(&mut conn, z, x, y, &source, bytes.len());
    Ok(())
}

/// POST /api/tiles/prefetch — fill a disc with vector + terrain tiles.
#[post("/api/tiles/prefetch", format = "json", data = "<req>")]
pub fn prefetch(
    req: Json<PrefetchReq>,
    state: &State<ServerState>,
) -> Result<Json<Value>, status::Custom<Json<Value>>> {
    if !req.lat.is_finite() || !req.lon.is_finite() {
        return Err(err(Status::BadRequest, "lat/lon must be finite"));
    }
    if req.lat < -85.0 || req.lat > 85.0 || req.lon < -180.0 || req.lon > 180.0 {
        return Err(err(Status::BadRequest, "lat/lon out of range"));
    }
    let radius = req.radius_m.clamp(50.0, MAX_RADIUS_M);
    let kinds = if req.kinds.is_empty() {
        vec!["vector".to_string(), "terrain".to_string()]
    } else {
        req.kinds.clone()
    };
    let want_vector = kinds.iter().any(|k| k == "vector");
    let want_terrain = kinds.iter().any(|k| k == "terrain");
    if !want_vector && !want_terrain {
        return Err(err(Status::BadRequest, "kinds must include vector and/or terrain"));
    }

    let st = state.inner();
    let mut fetched = 0usize;
    let mut cached = 0usize;
    let mut failed: Vec<Value> = Vec::new();
    let mut attempted = 0usize;
    let mut capped = false;
    // terrain windows for the Terrarium backfill pass
    let mut terrain_windows: Vec<(u32, u32, u32, u32, u32)> = Vec::new();

    macro_rules! fetch_tile {
        ($kind:expr, $z:expr, $x:expr, $y:expr, $path:expr, $url:expr, $valid:expr, $source:expr) => {
            // cached fast path never consumes the fetch budget
            match std::fs::read(&$path) {
                Ok(raw) if $valid(&raw) => cached += 1,
                _ => {
                    if attempted >= MAX_FETCH {
                        capped = true;
                    } else {
                        attempted += 1;
                        match fetch_remote(st, $kind, $z, $x, $y, $path, $url, $valid, $source) {
                            Ok(()) => fetched += 1,
                            Err(e) => failed.push(json!({"kind": $kind, "z": $z, "x": $x, "y": $y, "error": e})),
                        }
                    }
                }
            }
        };
    }

    if want_vector {
        for z in [15u32, 14] {
            for (x, y) in disc_window(req.lat, req.lon, radius, z) {
                let path = st.tilecache_dir().join(format!("v8_z{z}_{x}_{y}.mvt"));
                let url = format!(
                    "https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.mvt?access_token={}",
                    crate::grab::token()
                );
                fetch_tile!("vector", z, x, y, path, url, valid_vector, "vector-prefetch".to_string());
                if capped {
                    break;
                }
            }
            if capped {
                break;
            }
        }
    }
    if want_terrain && !capped {
        for z in [14u32, 13, 12] {
            let tiles = disc_window(req.lat, req.lon, radius, z);
            if !tiles.is_empty() {
                let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
                for (x, y) in &tiles {
                    x0 = x0.min(*x);
                    y0 = y0.min(*y);
                    x1 = x1.max(*x);
                    y1 = y1.max(*y);
                }
                terrain_windows.push((z, x0, y0, x1 + 1, y1 + 1));
                for (x, y) in tiles {
                    let path = st.tilecache_dir().join(format!("dem_z{z}_{x}_{y}.png"));
                    let url = format!(
                        "https://api.mapbox.com/v4/mapbox.terrain-rgb/{z}/{x}/{y}.png?access_token={}",
                        crate::grab::token()
                    );
                    fetch_tile!("terrain", z, x, y, path, url, valid_terrain, "terrain-prefetch".to_string());
                    if capped {
                        break;
                    }
                }
            }
            if capped {
                break;
            }
        }
    }
    // replace flat-0 ocean masks with Terrarium relief right away
    let mut replaced = 0usize;
    for (z, x0, y0, x1, y1) in &terrain_windows {
        replaced += crate::terrain::backfill_ocean_tiles(
            &st.tilecache_dir().to_string_lossy(),
            *z,
            *x0,
            *y0,
            *x1,
            *y1,
        );
    }

    Ok(Json(json!({
        "ok": true, "lat": req.lat, "lon": req.lon, "radiusM": radius,
        "fetched": fetched, "cached": cached, "failed": failed,
        "capped": capped, "terrariumReplaced": replaced,
    })))
}
