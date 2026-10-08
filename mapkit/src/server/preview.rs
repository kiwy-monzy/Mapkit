//! Selectable preview — decode cached Mapbox MVT tiles into per-class GeoJSON.
//!
//!   POST /api/collections/preview
//!   req: { polygon: [[lng,lat] x >=3], zoom?: u32 }
//!   resp: { tiles, counts, classes, layers: { roads, water, waterways,
//!           landcover, buildings, rails } }
//!
//! Every layer is a GeoJSON FeatureCollection with per-feature `class`
//! properties, so the web UI can offer per-class checkboxes and send the
//! chosen allow-lists back to `/api/collections/grab`. This decodes the
//! SAME `tilecache/v8_z{z}_{x}_{y}.mvt` files the grabber packs — preview
//! === what grab will pack. No Overpass, no new deps, works offline once
//! tiles are cached.

use std::collections::BTreeMap;
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
/// hard cap so a region-sized polygon can't block the handler on 100s of tiles
const MAX_PREVIEW_TILES: u32 = 64;
/// z15 building-only backfill cap (see below)
const MAX_BUILDING_TILES: usize = 25;

#[derive(Deserialize)]
pub struct PreviewReq {
    pub polygon: Vec<(f64, f64)>,
    #[serde(default)]
    pub zoom: Option<u32>,
}

fn err(code: Status, msg: impl Into<String>) -> status::Custom<Json<Value>> {
    status::Custom(code, Json(json!({ "error": msg.into() })))
}

/// tile window covering centre+span at zoom z (same math as grab.rs)
fn tile_window(lat: f64, lon: f64, z: u32, span_m: f64) -> (u32, u32, u32, u32) {
    let (cx, cy) = crate::mercator::lonlat_to_tile(lon, lat, z);
    let tspan = crate::mercator::tile_span_m(lat, z);
    let half = ((span_m / 2.0 / tspan).ceil() as u32).max(1);
    let n = 1u32 << z;
    (
        cx.saturating_sub(half),
        cy.saturating_sub(half),
        (cx + half + 1).min(n),
        (cy + half + 1).min(n),
    )
}

/// ensure one vector tile is cached (fetch on miss); returns raw bytes if usable
fn ensure_vector(
    st: &ServerState,
    z: u32,
    x: u32,
    y: u32,
) -> Option<Vec<u8>> {
    let path = st.tilecache_dir().join(format!("v8_z{z}_{x}_{y}.mvt"));
    if let Ok(raw) = std::fs::read(&path) {
        if !raw.is_empty() && !raw.starts_with(b"{") {
            return Some(raw);
        }
    }
    let url = format!(
        "https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.mvt?access_token={}",
        crate::grab::token()
    );
    let bytes = super::upstream::fetch_mapbox(&url, TILE_TIMEOUT).ok()?;
    if bytes.starts_with(b"{") || bytes.len() < 100 {
        return None;
    }
    let _ = super::upstream::write_atomic(&path, &bytes);
    let mut conn = st.conn.lock().unwrap();
    db::record_tile(&mut conn, z, x, y, "vector-preview", bytes.len());
    Some(bytes)
}

fn line_feat(pts: &[(f64, f64)], props: Value) -> Value {
    json!({
        "type": "Feature",
        "properties": props,
        "geometry": {
            "type": "LineString",
            "coordinates": pts.iter().map(|(lo, la)| [*lo, *la]).collect::<Vec<_>>(),
        }
    })
}

fn poly_feat(rings: &[Vec<(f64, f64)>], props: Value) -> Value {
    json!({
        "type": "Feature",
        "properties": props,
        "geometry": {
            "type": "Polygon",
            "coordinates": rings.iter().map(|r| r.iter().map(|(lo, la)| [*lo, *la]).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }
    })
}

/// split a polyline into runs of consecutive points passing `keep`
fn clip_runs(
    pts: &[(f64, f64)],
    keep: &dyn Fn(f64, f64) -> bool,
) -> Vec<Vec<(f64, f64)>> {
    let mut runs = Vec::new();
    let mut cur: Vec<(f64, f64)> = Vec::new();
    for &(lo, la) in pts {
        if keep(lo, la) {
            cur.push((lo, la));
        } else if cur.len() >= 2 {
            runs.push(std::mem::take(&mut cur));
        } else {
            cur.clear();
        }
    }
    if cur.len() >= 2 {
        runs.push(cur);
    }
    runs
}

/// POST /api/collections/preview — per-class GeoJSON preview of a polygon.
#[post("/api/collections/preview", format = "json", data = "<req>")]
pub fn preview(
    req: Json<PreviewReq>,
    state: &State<ServerState>,
) -> Result<Json<Value>, status::Custom<Json<Value>>> {
    let poly = &req.polygon;
    if poly.len() < 3 {
        return Err(err(
            Status::BadRequest,
            "polygon needs >= 3 [lng,lat] points",
        ));
    }
    for (lo, la) in poly.iter() {
        if !lo.is_finite() || !la.is_finite() {
            return Err(err(Status::BadRequest, "polygon has non-finite coords"));
        }
    }
    let st = state.inner();

    // close the ring for point-in-ring tests
    let mut ring = poly.clone();
    if ring.first() != ring.last() {
        ring.push(ring[0]);
    }
    let in_zone = |lo: f64, la: f64| -> bool {
        crate::boundaries::point_in_ring(lo, la, &ring)
    };

    let (mut minlo, mut minla, mut maxlo, mut maxla) =
        (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (lo, la) in poly.iter() {
        minlo = minlo.min(*lo);
        minla = minla.min(*la);
        maxlo = maxlo.max(*lo);
        maxla = maxla.max(*la);
    }
    let clat = (minla + maxla) / 2.0;
    let clon = (minlo + maxlo) / 2.0;

    // pick the vector zoom like the grabber (diag thresholds), then coarsen
    // until the window fits the preview cap
    let m = crate::mercator::Mercator::new(clat, clon);
    let (x0, y0) = m.project(minlo, minla);
    let (x1, y1) = m.project(maxlo, maxla);
    let diag = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
    let mut z: u32 = match req.zoom {
        Some(v) => v.clamp(9, 15),
        None => {
            if diag < 8_000.0 {
                15
            } else if diag < 40_000.0 {
                14
            } else {
                13
            }
        }
    };
    // span estimate for the window (diag + margin, like the grabber)
    let span = (diag + 2_000.0).max(2_000.0);
    loop {
        let (tx0, ty0, tx1, ty1) = tile_window(clat, clon, z, span);
        let n = (tx1.saturating_sub(tx0)) * (ty1.saturating_sub(ty0));
        if n <= MAX_PREVIEW_TILES || z <= 9 {
            break;
        }
        z -= 1;
    }
    let (tx0, ty0, tx1, ty1) = tile_window(clat, clon, z, span);

    let mut roads: Vec<Value> = Vec::new();
    let mut water: Vec<Value> = Vec::new();
    let mut waterways: Vec<Value> = Vec::new();
    let mut landcover: Vec<Value> = Vec::new();
    let mut buildings: Vec<Value> = Vec::new();
    let mut rails: Vec<Value> = Vec::new();
    let mut road_classes: BTreeMap<String, usize> = BTreeMap::new();
    let mut water_classes: BTreeMap<String, usize> = BTreeMap::new();
    let mut waterway_classes: BTreeMap<String, usize> = BTreeMap::new();
    let mut landcover_classes: BTreeMap<String, usize> = BTreeMap::new();
    let mut tiles_hit = 0u32;

    for ty in ty0..ty1 {
        for tx in tx0..tx1 {
            let Some(raw) = ensure_vector(st, z, tx, ty) else {
                continue;
            };
            let Some(td) =
                crate::mvt::parse(&raw, &crate::mvt::TileRef { z, x: tx, y: ty })
            else {
                continue;
            };
            tiles_hit += 1;
            for r in &td.roads {
                if r.rail {
                    for run in clip_runs(&r.pts, &in_zone) {
                        *road_classes.entry(format!("rail:{}", r.class)).or_default() += 1;
                        rails.push(line_feat(
                            &run,
                            json!({ "class": r.class, "kind": "rail" }),
                        ));
                    }
                    continue;
                }
                // skip pure footpaths from the road set (grabber drops them too)
                if matches!(
                    r.class.as_str(),
                    "path" | "sidewalk" | "steps" | "pedestrian"
                ) {
                    continue;
                }
                for run in clip_runs(&r.pts, &in_zone) {
                    *road_classes.entry(r.class.clone()).or_default() += 1;
                    roads.push(line_feat(
                        &run,
                        json!({
                            "class": r.class, "surface": r.surface,
                            "oneway": r.oneway, "ref": r.road_ref, "name": r.name,
                        }),
                    ));
                }
            }
            for w in &td.water {
                let keep = w
                    .rings
                    .first()
                    .map(|r| r.iter().any(|&(lo, la)| in_zone(lo, la)))
                    .unwrap_or(false);
                if !keep {
                    continue;
                }
                *water_classes.entry(w.class.clone()).or_default() += 1;
                water.push(poly_feat(&w.rings, json!({ "class": w.class })));
            }
            for w in &td.waterways {
                for run in clip_runs(&w.pts, &in_zone) {
                    *waterway_classes.entry(w.class.clone()).or_default() += 1;
                    waterways.push(line_feat(&run, json!({ "class": w.class })));
                }
            }
            for p in &td.landcover {
                let keep = p
                    .rings
                    .first()
                    .map(|r| r.iter().any(|&(lo, la)| in_zone(lo, la)))
                    .unwrap_or(false);
                if !keep {
                    continue;
                }
                *landcover_classes.entry(p.class.clone()).or_default() += 1;
                landcover.push(poly_feat(&p.rings, json!({ "class": p.class })));
            }
            // z13/z14 vector tiles carry no building layer — flag it below
            for b in &td.buildings {
                let keep = b
                    .rings
                    .first()
                    .map(|r| r.iter().any(|&(lo, la)| in_zone(lo, la)))
                    .unwrap_or(false);
                if !keep {
                    continue;
                }
                buildings.push(poly_feat(
                    &b.rings,
                    json!({ "class": "building", "height": b.height }),
                ));
            }
        }
    }

    // buildings backfill at z15 when the window zoom has no building layer
    let mut buildings_incomplete = false;
    if z < 15 {
        let bz = 15u32;
        let n = 1u32 << bz;
        let (bx0, by0, bx1, by1) = tile_window(clat, clon, bz, span);
        let mut wanted: Vec<(u32, u32)> = Vec::new();
        for ty in by0..by1 {
            for tx in bx0..bx1 {
                let lon = (tx as f64 + 0.5) / n as f64 * 360.0 - 180.0;
                let nrm = 1.0 - 2.0 * (ty as f64 + 0.5) / n as f64;
                let lat = (nrm * std::f64::consts::PI).sinh().atan().to_degrees();
                if in_zone(lon, lat) {
                    wanted.push((tx, ty));
                }
                if wanted.len() >= MAX_BUILDING_TILES {
                    break;
                }
            }
            if wanted.len() >= MAX_BUILDING_TILES {
                break;
            }
        }
        let total = (bx1.saturating_sub(bx0)) * (by1.saturating_sub(by0));
        buildings_incomplete = (wanted.len() as u32) < total.min(10_000);
        // only backfill when the cap didn't truncate the zone too hard;
        // otherwise report incomplete so the UI warns instead of lying
        if !buildings_incomplete || !wanted.is_empty() {
            for (tx, ty) in wanted {
                let Some(raw) = ensure_vector(st, bz, tx, ty) else {
                    continue;
                };
                let Some(td) =
                    crate::mvt::parse(&raw, &crate::mvt::TileRef { z: bz, x: tx, y: ty })
                else {
                    continue;
                };
                for b in &td.buildings {
                    let keep = b
                        .rings
                        .first()
                        .map(|r| r.iter().any(|&(lo, la)| in_zone(lo, la)))
                        .unwrap_or(false);
                    if !keep {
                        continue;
                    }
                    buildings.push(poly_feat(
                        &b.rings,
                        json!({ "class": "building", "height": b.height }),
                    ));
                }
            }
        }
        if total > MAX_BUILDING_TILES as u32 {
            buildings_incomplete = true;
        }
    }

    // cap feature payloads so a dense city zone can't blow up the response
    // (counts below still report the true pre-truncation totals)
    const MAX_FEATS: usize = 6000;
    let counts = serde_json::json!({
        "roads": roads.len(), "water": water.len(),
        "waterways": waterways.len(), "landcover": landcover.len(),
        "buildings": buildings.len(), "rails": rails.len(),
    });
    for v in [&mut roads, &mut water, &mut waterways, &mut landcover, &mut buildings, &mut rails] {
        if v.len() > MAX_FEATS {
            v.truncate(MAX_FEATS);
        }
    }

    Ok(Json(json!({
        "tiles": { "z": z, "x0": tx0, "y0": ty0, "x1": tx1, "y1": ty1, "hit": tiles_hit },
        "counts": counts,
        "classes": {
            "roads": road_classes, "water": water_classes,
            "waterways": waterway_classes, "landcover": landcover_classes,
        },
        "buildingsIncomplete": buildings_incomplete,
        "layers": {
            "roads": { "type": "FeatureCollection", "features": roads },
            "water": { "type": "FeatureCollection", "features": water },
            "waterways": { "type": "FeatureCollection", "features": waterways },
            "landcover": { "type": "FeatureCollection", "features": landcover },
            "vegetation": { "type": "FeatureCollection", "features": landcover.clone() },
            "buildings": { "type": "FeatureCollection", "features": buildings },
            "rails": { "type": "FeatureCollection", "features": rails },
        }
    })))
}
