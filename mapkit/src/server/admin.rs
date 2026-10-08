//! Admin zones API — free, data-driven browsing of the full Tanzania
//! admin pack (geoBoundaries ADM1 regions → ADM2 districts → ADM3 wards).
//!
//!   GET /api/admin/overview                 pack presence + zone counts
//!   GET /api/admin/regions                  every region (bbox, centre, district count)
//!   GET /api/admin/districts?region=<name>  districts of a region (ward counts)
//!   GET /api/admin/wards?district=<name>[&region=<name>]
//!                                           wards of a district
//!   GET /api/admin/search?q=<text>          fuzzy find across all levels (max 50)
//!   GET /api/admin/geometry?query=<q>[&maxPoints=N]
//!                                           one zone's rings as GeoJSON
//!                                           (`region:`/`district:`/`ward:` prefix,
//!                                           same syntax as mapkit-cli --admin)
//!
//! The pack dir is `<root>/assets/tanzania-pack/boundaries`, parsed once
//! and cached in [`ServerState::admin`]. Parent scoping (district→region,
//! ward→district) reuses [`AdminIndex::parents`], the same rule the
//! `zoning` crate and the grabber's boundary layer use.

use rocket::http::Status;
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::State;
use serde_json::{json, Value};

use super::state::AdminCache;
use super::ServerState;
use crate::boundaries::AdminIndex;

fn err(code: Status, msg: impl Into<String>) -> status::Custom<Json<Value>> {
    status::Custom(code, Json(json!({ "error": msg.into() })))
}

fn pack_dir(st: &ServerState) -> std::path::PathBuf {
    st.root.join("assets/tanzania-pack/boundaries")
}

fn index(st: &ServerState) -> Result<AdminIndex, status::Custom<Json<Value>>> {
    let dir = pack_dir(st);
    AdminCache::get_or_load(&st.admin, &dir)
        .ok_or_else(|| err(Status::ServiceUnavailable, format!("admin pack missing at {}", dir.display())))
}

/// bbox centre as [lat, lon] (best fly-to point; the vertex-average centroid
/// is only used for parent scoping, never for display).
fn bbox_center(b: &(f64, f64, f64, f64)) -> (f64, f64) {
    ((b.1 + b.3) / 2.0, (b.0 + b.2) / 2.0)
}

fn zone_json(
    idx: &AdminIndex,
    z: &crate::boundaries::Zone,
    with_counts: bool,
) -> Value {
    let (clat, clon) = bbox_center(&z.bbox);
    let (district, region) = idx.parents(z);
    let mut v = json!({
        "name": z.name,
        "kind": match z.adm { 1 => "region", 2 => "district", _ => "ward" },
        "district": district,
        "region": region,
        "bbox": [z.bbox.0, z.bbox.1, z.bbox.2, z.bbox.3],
        "centerLat": clat,
        "centerLon": clon,
        "rings": z.rings.len(),
    });
    if with_counts {
        let (d, w) = match z.adm {
            1 => (
                idx.zones
                    .iter()
                    .filter(|o| o.adm == 2 && idx.parents(o).1 == z.name)
                    .count(),
                0,
            ),
            2 => (
                0,
                idx.zones
                    .iter()
                    .filter(|o| o.adm == 3 && idx.parents(o).0 == z.name)
                    .count(),
            ),
            _ => (0, 0),
        };
        v["districts"] = json!(d);
        v["wards"] = json!(w);
    }
    v
}

/// GET /api/admin/overview — is the pack there and how big is it?
#[get("/api/admin/overview")]
pub fn overview(state: &State<ServerState>) -> Json<Value> {
    let st = state.inner();
    let dir = pack_dir(st);
    match index(st) {
        Ok(idx) => {
            let mut regions = 0;
            let mut districts = 0;
            let mut wards = 0;
            for z in &idx.zones {
                match z.adm {
                    1 => regions += 1,
                    2 => districts += 1,
                    _ => wards += 1,
                }
            }
            Json(json!({
                "pack": true, "dir": dir.display().to_string(),
                "regions": regions, "districts": districts, "wards": wards,
            }))
        }
        Err(_) => Json(json!({
            "pack": false, "dir": dir.display().to_string(),
            "regions": 0, "districts": 0, "wards": 0,
        })),
    }
}

/// GET /api/admin/regions — every ADM1 region with its district count.
#[get("/api/admin/regions")]
pub fn regions(
    state: &State<ServerState>,
) -> Result<Json<Value>, status::Custom<Json<Value>>> {
    let idx = index(state.inner())?;
    let mut out: Vec<Value> = idx
        .zones
        .iter()
        .filter(|z| z.adm == 1)
        .map(|z| zone_json(&idx, z, true))
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(json!({ "regions": out })))
}

/// GET /api/admin/districts?region=<name> — districts of one region.
#[get("/api/admin/districts?<region>")]
pub fn districts(
    region: String,
    state: &State<ServerState>,
) -> Result<Json<Value>, status::Custom<Json<Value>>> {
    let idx = index(state.inner())?;
    if !idx
        .zones
        .iter()
        .any(|z| z.adm == 1 && z.name.eq_ignore_ascii_case(region.trim()))
    {
        return Err(err(Status::NotFound, format!("region '{region}' not found")));
    }
    let mut out: Vec<Value> = idx
        .zones
        .iter()
        .filter(|z| z.adm == 2 && idx.parents(z).1.eq_ignore_ascii_case(region.trim()))
        .map(|z| zone_json(&idx, z, true))
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(json!({ "region": region, "districts": out })))
}

/// GET /api/admin/wards?district=<name>[&region=<name>] — wards of a district.
/// `region` disambiguates districts that share a name across regions.
#[get("/api/admin/wards?<district>&<region>")]
pub fn wards(
    district: String,
    region: Option<String>,
    state: &State<ServerState>,
) -> Result<Json<Value>, status::Custom<Json<Value>>> {
    let idx = index(state.inner())?;
    let district = district.trim();
    let in_scope = |z: &crate::boundaries::Zone| -> bool {
        if z.adm != 2 || !z.name.eq_ignore_ascii_case(district) {
            return false;
        }
        match &region {
            Some(r) => idx.parents(z).1.eq_ignore_ascii_case(r.trim()),
            None => true,
        }
    };
    let scopes: Vec<&crate::boundaries::Zone> =
        idx.zones.iter().filter(|z| in_scope(z)).collect();
    if scopes.is_empty() {
        return Err(err(
            Status::NotFound,
            format!("district '{district}' not found"),
        ));
    }
    // one district name can exist in several regions — union them unless the
    // caller pinned a region (then there is exactly one scope)
    let mut out: Vec<Value> = idx
        .zones
        .iter()
        .filter(|z| {
            z.adm == 3
                && scopes.iter().any(|s| {
                    let (d, r) = idx.parents(z);
                    d == s.name
                        && match &region {
                            Some(rq) => r.eq_ignore_ascii_case(rq.trim()),
                            None => true,
                        }
                })
        })
        .map(|z| zone_json(&idx, z, false))
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(json!({ "district": district, "wards": out })))
}

/// GET /api/admin/search?q=<text> — fuzzy find across regions/districts/wards.
#[get("/api/admin/search?<q>")]
pub fn search(
    q: String,
    state: &State<ServerState>,
) -> Result<Json<Value>, status::Custom<Json<Value>>> {
    let q = q.trim().to_lowercase();
    if q.len() < 2 {
        return Err(err(Status::BadRequest, "q needs >= 2 characters"));
    }
    let idx = index(state.inner())?;
    let mut hits: Vec<(&crate::boundaries::Zone, bool)> = idx
        .zones
        .iter()
        .filter(|z| z.name.to_lowercase().contains(&q))
        .map(|z| (z, z.name.to_lowercase() == q))
        .collect();
    // exact matches first, then regions, districts, wards, then by name
    hits.sort_by(|a, b| {
        (!a.1)
            .cmp(&(!b.1))
            .then(a.0.adm.cmp(&b.0.adm))
            .then(a.0.name.cmp(&b.0.name))
    });
    hits.truncate(50);
    let out: Vec<Value> = hits
        .into_iter()
        .map(|(z, _)| zone_json(&idx, z, false))
        .collect();
    Ok(Json(json!({ "q": q, "hits": out })))
}

/// stride-decimate a ring to ~`max` points (always keeps the closing point)
fn decimate(ring: &[(f64, f64)], max: usize) -> Vec<(f64, f64)> {
    if ring.len() <= max.max(4) {
        return ring.to_vec();
    }
    let stride = (ring.len() as f64 / max as f64).ceil() as usize;
    let mut out: Vec<(f64, f64)> = ring.iter().step_by(stride).copied().collect();
    if let Some(last) = ring.last() {
        if out.last() != Some(last) {
            out.push(*last);
        }
    }
    out
}

/// GET /api/admin/geometry?query=<q>[&maxPoints=N] — one zone's rings as
/// GeoJSON (MultiPolygon lon/lat), ready to draw + to derive a preview
/// polygon from. `query` uses the mapkit-cli --admin syntax.
/// NOTE: each source ring becomes its own polygon (holes render filled);
/// the UI only needs the outer boundary, so this is acceptable.
#[get("/api/admin/geometry?<query>&<maxPoints>")]
#[allow(non_snake_case)] // maxPoints is the public query-param name
pub fn geometry(
    query: String,
    maxPoints: Option<usize>,
    state: &State<ServerState>,
) -> Result<Json<Value>, status::Custom<Json<Value>>> {
    let idx = index(state.inner())?;
    let Some(z) = idx.find(&query) else {
        return Err(err(
            Status::NotFound,
            format!("zone '{query}' not found (try region:/district:/ward: prefixes)"),
        ));
    };
    let max = maxPoints.unwrap_or(4000).clamp(64, 50_000);
    let polys: Vec<Vec<Vec<(f64, f64)>>> = z
        .rings
        .iter()
        .map(|r| vec![decimate(r, max)])
        .collect();
    let (district, region) = idx.parents(z);
    let coords: Vec<Value> = polys
        .iter()
        .map(|rings| {
            Value::Array(
                rings
                    .iter()
                    .map(|r| {
                        Value::Array(
                            r.iter().map(|(lo, la)| json!([lo, la])).collect(),
                        )
                    })
                    .collect(),
            )
        })
        .collect();
    // resolve the canonical display name (find() is fuzzy)
    Ok(Json(json!({
        "type": "Feature",
        "properties": {
            "name": z.name,
            "kind": match z.adm { 1 => "region", 2 => "district", _ => "ward" },
            "query": format!("{}:{}",
                match z.adm { 1 => "region", 2 => "district", _ => "ward" }, z.name),
            "district": district, "region": region,
        },
        "geometry": { "type": "MultiPolygon", "coordinates": coords },
    })))
}
