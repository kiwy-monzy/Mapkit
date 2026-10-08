//! Collection layers API — serve one `.mapbin` bundle's layers to the web
//! UI as GeoJSON with display colors.
//!
//!   GET /api/collections/layers?file=<slug>.mapbin
//!
//! Every vector layer becomes a FeatureCollection whose features carry the
//! same colors the engine's layer registry uses (`native_app::layers`):
//! roads colored per class, rivers steel-blue, water blue, landcover per
//! land-use class, buildings grey (+height), boundary gold / districts
//! orange / wards grey, rails grey dashed. Terrain (`tzd1` binary) returns
//! stats only — its visual is the basemap drape.
//!
//! Dense zones (Dar es Salaam: 300k+ road edges) are stride-sampled to
//! `MAX_FEATS` per layer; the response reports `total` vs served counts.

use rocket::http::Status;
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::State;
use serde_json::{json, Value};

use super::ServerState;

const MAX_FEATS: usize = 8000;

fn err(code: Status, msg: impl Into<String>) -> status::Custom<Json<Value>> {
    status::Custom(code, Json(json!({ "error": msg.into() })))
}

/// polygon style for a ring class (stroke/fill/weight as Leaflet options)
type PolyStyle = dyn Fn(&str) -> Value;

fn check_file(file: &str) -> Result<(), status::Custom<Json<Value>>> {
    if !file
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || !file.ends_with(".mapbin")
        || file.contains("..")
    {
        return Err(err(Status::BadRequest, "bad file name"));
    }
    Ok(())
}

// ── display colors (mirror native_app/src/layers/mod.rs) ────────────────

fn road_color(class: &str) -> &'static str {
    match class {
        "motorway" | "trunk" | "motorway_link" | "trunk_link" => "#fb923c",
        "primary" | "primary_link" => "#facc15",
        "secondary" | "secondary_link" => "#e7e5e4",
        "tertiary" | "tertiary_link" => "#d6d3d1",
        _ => "#a8a29e",
    }
}

fn road_weight(class: &str) -> f32 {
    match class {
        "motorway" | "trunk" => 4.0,
        "motorway_link" | "trunk_link" => 3.0,
        "primary" | "primary_link" => 3.0,
        "secondary" | "secondary_link" => 2.5,
        "tertiary" | "tertiary_link" => 2.0,
        _ => 1.5,
    }
}

fn landcover_fill(class: &str) -> &'static str {
    match class {
        "wood" | "forest" => "#226422",
        "park" | "grass" | "meadow" | "recreation_ground" => "#5aa046",
        "school" | "college" | "university" | "hospital" | "cemetery" => "#b48cc8",
        "industrial" | "quarry" | "railway" => "#82828c",
        "commercial" | "retail" | "commercial_area" => "#c8aa5a",
        "residential" => "#a0a078",
        "parking" | "pitch" | "sand" | "agriculture" => "#8caa78",
        "water" => "#4682b4",
        _ => "#8caa78",
    }
}

fn boundary_style(class: &str) -> (&'static str, f32) {
    match class {
        "district" => ("#eb8c32", 3.5),
        "ward" => ("#a5aabe", 1.5),
        _ => ("#d4af37", 5.0),
    }
}

// ── ego-txt parsers (reverse of grab.rs writers) ─────────────────────────

fn floats(s: &str) -> Vec<f64> {
    s.split_whitespace().filter_map(|t| t.parse().ok()).collect()
}

/// ego-map-txt-v2 road graph → LineString features (node coords + interior pts)
fn parse_roads(txt: &str) -> Vec<Value> {
    let mut lines = txt.lines();
    let head: Vec<usize> = lines
        .next()
        .map(|l| {
            l.split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    if head.len() < 2 {
        return vec![];
    }
    let (n_inters, n_edges) = (head[0], head[1]);
    let mut nodes: Vec<(f64, f64)> = Vec::with_capacity(n_inters.min(1_000_000));
    for _ in 0..n_inters {
        let Some(l) = lines.next() else { break };
        let v = floats(l);
        if v.len() >= 2 {
            nodes.push((v[1], v[0])); // file is lat lon → keep lng,lat
        } else {
            nodes.push((f64::NAN, f64::NAN));
        }
    }
    let mut out = Vec::new();
    for _ in 0..n_edges {
        let Some(l) = lines.next() else { break };
        let mut t = l.split_whitespace();
        let (a, b) = (
            t.next().and_then(|v| v.parse::<usize>().ok()),
            t.next().and_then(|v| v.parse::<usize>().ok()),
        );
        let lanes = t.next().unwrap_or("1");
        let oneway = t.next().unwrap_or("0");
        let speed = t.next().unwrap_or("0");
        let npts: usize = t.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let rest: Vec<&str> = t.collect();
        // interior points are lat lon pairs; v2 appends trailing class + ref
        // tokens (v1 bundles have none — detect by numeric-ness so v1 keeps
        // its last coordinate pair instead of eating it as class/ref)
        let (num_toks, class, road_ref): (&[&str], &str, &str) = match rest.as_slice() {
            [] => (&[], "-", "-"),
            [c] => (&[], c, "-"),
            _ => {
                let n = rest.len();
                if rest[n - 2].parse::<f64>().is_ok() && rest[n - 1].parse::<f64>().is_ok() {
                    (rest.as_slice(), "-", "-")
                } else {
                    (&rest[..n - 2], rest[n - 2], rest[n - 1])
                }
            }
        };
        let nums = floats(&num_toks.join(" "));
        let mut pts: Vec<(f64, f64)> = Vec::with_capacity(npts + 2);
        if let (Some(a), Some(b)) = (a, b) {
            if let (Some(na), Some(nb)) = (nodes.get(a), nodes.get(b)) {
                if na.0.is_finite() && nb.0.is_finite() {
                    pts.push(*na);
                    for w in nums.chunks(2) {
                        if w.len() == 2 && w[0].is_finite() && w[1].is_finite() {
                            pts.push((w[1], w[0]));
                        }
                    }
                    pts.push(*nb);
                }
            }
        }
        if pts.len() < 2 {
            continue;
        }
        let cls = if class == "-" { "" } else { class };
        out.push(json!({
            "type": "Feature",
            "properties": {
                "class": cls, "ref": if road_ref == "-" { "" } else { road_ref },
                "lanes": lanes, "oneway": oneway, "speed": speed,
                "color": road_color(cls), "weight": road_weight(cls),
            },
            "geometry": {
                "type": "LineString",
                "coordinates": pts.iter().map(|(lo, la)| [*lo, *la]).collect::<Vec<_>>(),
            }
        }));
    }
    out
}

/// ego-lines-txt-v1 (rivers / rails): `count`, per line `npts class lat lon…`
fn parse_lines(txt: &str, color: &str, weight: f32, dash: bool) -> Vec<Value> {
    let mut lines = txt.lines();
    let total: usize = lines
        .next()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    let mut out = Vec::with_capacity(total.min(1_000_000));
    for l in lines {
        let mut t = l.split_whitespace();
        let n: usize = match t.next().and_then(|v| v.parse().ok()) {
            Some(n) => n,
            None => continue,
        };
        let cls = t.next().unwrap_or("").to_string();
        let rest = t.collect::<Vec<_>>().join(" ");
        let nums = floats(&rest);
        let pts: Vec<(f64, f64)> = nums
            .chunks(2)
            .take(n)
            .filter_map(|w| {
                if w.len() == 2 && w[0].is_finite() && w[1].is_finite() {
                    Some((w[1], w[0]))
                } else {
                    None
                }
            })
            .collect();
        if pts.len() < 2 {
            continue;
        }
        let mut props = json!({
            "class": cls, "color": color, "weight": weight,
        });
        if dash {
            props["dashArray"] = json!("4 4");
        }
        out.push(json!({
            "type": "Feature",
            "properties": props,
            "geometry": {
                "type": "LineString",
                "coordinates": pts.iter().map(|(lo, la)| [*lo, *la]).collect::<Vec<_>>(),
            }
        }));
    }
    out
}

/// one closed ring (lon/lat), guaranteed closed for Leaflet
fn close_ring(pts: &[(f64, f64)]) -> Vec<[f64; 2]> {
    let mut r: Vec<[f64; 2]> = pts.iter().map(|(lo, la)| [*lo, *la]).collect();
    if r.len() > 2 && r.first() != r.last() {
        r.push(r[0]);
    }
    r
}

/// ego-rings-txt-v1, both shapes:
/// * write_polys: `{nrings} {class}` header + `{n} lat lon…` ring lines
/// * write_landcover: `{n} {class} lat lon…` single-ring entries
/// ego-rings-txt-v1 in all three shapes the writers emit:
/// * poly header `{nrings} {class}` (class may be empty: `{nrings} `) +
///   that many ring lines `{n} lat lon…` (write_polys: water/boundary)
/// * landcover entry `{n} {class}` + coords on the NEXT line as pure
///   `lat lon…` pairs (write_landcover)
///
/// Disambiguation: ring lines are all-numeric with an ODD token count
/// (`n` + 2n coords); landcover coord lines are all-numeric with an EVEN
/// count. A `{N} {class}` line is a landcover entry iff the next line is
/// coords-shaped, else a poly header.
fn parse_rings(
    txt: &str,
    poly_style: &PolyStyle,
) -> Vec<Value> {
    fn pairs(nums: &[f64], n: usize) -> Vec<(f64, f64)> {
        nums.chunks(2)
            .take(n)
            .filter_map(|w| {
                if w.len() == 2 && w[0].is_finite() && w[1].is_finite() {
                    Some((w[1], w[0])) // file is lat lon → keep lng,lat
                } else {
                    None
                }
            })
            .collect()
    }
    fn emit(out: &mut Vec<Value>, poly_style: &PolyStyle, cls: String, rings: Vec<Vec<(f64, f64)>>) {
        if rings.is_empty() {
            return;
        }
        let mut props = poly_style(&cls);
        props["class"] = Value::String(cls);
        out.push(json!({
            "type": "Feature",
            "properties": props,
            "geometry": {
                "type": "Polygon",
                "coordinates": rings.iter().map(|r| close_ring(r)).collect::<Vec<_>>(),
            }
        }));
    }
    /// all-numeric line → Some((is_coords_shape, numbers))
    fn numeric_line(l: &str) -> Option<(bool, Vec<f64>)> {
        let toks: Vec<&str> = l.split_whitespace().collect();
        if toks.len() < 2 {
            return None;
        }
        let nums: Vec<f64> = toks.iter().filter_map(|t| t.parse().ok()).collect();
        if nums.len() != toks.len() {
            return None;
        }
        // ring line: n + 2n numbers (odd); coords line: pure pairs (even)
        Some((nums.len() % 2 == 0, nums))
    }

    let mut lines = txt.lines().peekable();
    let _ = lines.next(); // header count (parse defensively to EOF anyway)
    let mut out = Vec::new();
    let mut pending: Option<(String, usize, Vec<Vec<(f64, f64)>>)> = None;
    // landcover entry awaiting its coords line
    let mut pending_coords: Option<(String, usize)> = None;

    while let Some(l) = lines.next() {
        if let Some((cls, n)) = pending_coords.take() {
            if let Some((true, nums)) = numeric_line(l) {
                let pts = pairs(&nums, n);
                if pts.len() >= 3 {
                    emit(&mut out, poly_style, cls, vec![pts]);
                }
            }
            continue;
        }
        let toks: Vec<&str> = l.split_whitespace().collect();
        if toks.is_empty() {
            continue;
        }
        if let Some((is_coords, nums)) = numeric_line(l) {
            if is_coords {
                continue; // stray coords line with no entry — skip
            }
            // ring line `{n} lat lon…` for the pending header
            let n = nums[0] as usize;
            let pts = pairs(&nums[1..], n);
            if pts.len() < 3 {
                continue;
            }
            match pending.as_mut() {
                Some((_, need, rings)) => {
                    rings.push(pts);
                    if rings.len() >= *need {
                        if let Some((cls, _, rings)) = pending.take() {
                            emit(&mut out, poly_style, cls, rings);
                        }
                    }
                }
                None => {} // orphan ring — skip
            }
            continue;
        }
        // `{N} {class?}` line: landcover entry or poly header (or empty class)
        let n: usize = toks[0].parse().unwrap_or(0);
        let cls = toks.get(1).unwrap_or(&"").to_string();
        // flush any completed header before starting a new poly
        if let Some((pc, _, pr)) = pending.take() {
            emit(&mut out, poly_style, pc, pr);
        }
        if n == 0 {
            continue;
        }
        let next_is_coords = lines
            .peek()
            .and_then(|nl| numeric_line(nl))
            .map(|(c, _)| c)
            .unwrap_or(false);
        if next_is_coords {
            pending_coords = Some((cls, n));
        } else {
            pending = Some((cls, n, Vec::new()));
        }
    }
    if let Some((pc, _, pr)) = pending.take() {
        emit(&mut out, poly_style, pc, pr);
    }
    out
}

/// ego-buildings-txt: `count`, per building `{h} {min_h} {n}` line +
/// coords on the NEXT line (`lat lon…`, closing point dropped)
fn parse_buildings(txt: &str) -> Vec<Value> {
    let mut lines = txt.lines();
    let _ = lines.next();
    let mut out = Vec::new();
    loop {
        let Some(h) = lines.next() else { break };
        let ht: Vec<&str> = h.split_whitespace().collect();
        if ht.len() < 3 {
            continue;
        }
        let (height, n): (f32, usize) = (
            ht[0].parse().unwrap_or(4.0),
            ht[2].parse().unwrap_or(0),
        );
        let Some(cl) = lines.next() else { break };
        let nums = floats(cl);
        let pts: Vec<(f64, f64)> = nums
            .chunks(2)
            .take(n)
            .filter_map(|w| {
                if w.len() == 2 && w[0].is_finite() && w[1].is_finite() {
                    Some((w[1], w[0]))
                } else {
                    None
                }
            })
            .collect();
        if pts.len() < 3 {
            continue;
        }
        out.push(json!({
            "type": "Feature",
            "properties": {
                "class": "building", "height": height,
                "color": "#a8a29e", "fill": "#d6d3d1", "fillOpacity": 0.45, "weight": 1,
            },
            "geometry": {
                "type": "Polygon",
                "coordinates": [close_ring(&pts)],
            }
        }));
    }
    out
}

/// stride-sample dense layers so the response stays small but covers the zone
fn sample(feats: Vec<Value>) -> (Vec<Value>, usize, bool) {
    let total = feats.len();
    if total > MAX_FEATS {
        let stride = (total as f64 / MAX_FEATS as f64).ceil() as usize;
        let sampled: Vec<Value> = feats.into_iter().step_by(stride).collect();
        (sampled, total, true)
    } else {
        (feats, total, false)
    }
}

fn fc(feats: Vec<Value>, extra: Value) -> Value {
    let (feats, total, truncated) = sample(feats);
    json!({
        "type": "FeatureCollection",
        "features": feats,
        "total": total,
        "truncated": truncated,
        "style": extra,
    })
}

/// GET /api/collections/layers?file=<slug>.mapbin — every layer as GeoJSON.
#[get("/api/collections/layers?<file>")]
pub fn layers(
    file: String,
    state: &State<ServerState>,
) -> Result<Json<Value>, status::Custom<Json<Value>>> {
    check_file(&file)?;
    let st = state.inner();
    let p = st.collections_dir().join(&file);
    let bundle = crate::bundle::Bundle::from_file(&p)
        .map_err(|_| err(Status::NotFound, "not found"))?;
    let text = |id: &str| bundle.layer_text(id).unwrap_or_default();
    let m = &bundle.meta;

    let roads = parse_roads(&text("roads"));
    let rivers = parse_lines(&text("rivers"), "#4682b4", 2.0, false);
    let rails = parse_lines(&text("rails"), "#78787f", 2.0, true);
    let water = parse_rings(&text("water"), &|_| {
        json!({"color": "#60a5fa", "fill": "#3778af", "fillOpacity": 0.55, "weight": 2})
    });
    let landcover = parse_rings(&text("landcover"), &|cls| {
        json!({"color": "#4ade80", "fill": landcover_fill(cls), "fillOpacity": 0.5, "weight": 1})
    });
    let buildings = parse_buildings(&text("buildings"));
    let boundary = parse_rings(&text("boundary"), &|cls| {
        let (c, w) = boundary_style(cls);
        json!({"color": c, "weight": w})
    });

    // terrain stats (sampled — Dar bundles are ~95 MB of f32 DEM)
    let terrain = bundle.layer("terrain").map(|b| {
        if b.len() >= 20 && &b[0..4] == b"TZD1" {
            let w = u32::from_le_bytes(b[4..8].try_into().unwrap_or([0; 4])) as usize;
            let h = u32::from_le_bytes(b[8..12].try_into().unwrap_or([0; 4])) as usize;
            let cell = f32::from_le_bytes(b[12..16].try_into().unwrap_or([0; 4]));
            let n = (w * h).min((b.len() - 20) / 4);
            let (mut lo, mut hi, mut sum, mut cnt) =
                (f32::MAX, f32::MIN, 0f64, 0usize);
            let mut i = 0;
            while i < n {
                let v = f32::from_le_bytes(
                    b[20 + i * 4..24 + i * 4].try_into().unwrap_or([0; 4]),
                );
                if v.is_finite() {
                    lo = lo.min(v);
                    hi = hi.max(v);
                    sum += v as f64;
                    cnt += 1;
                }
                i += 16; // sample
            }
            json!({
                "cells": [w, h], "cellM": cell,
                "minM": lo, "maxM": hi, "meanM": if cnt > 0 { sum / cnt as f64 } else { 0.0 },
            })
        } else {
            json!({"error": "no terrain layer"})
        }
    }).unwrap_or(json!({"error": "no terrain layer"}));

    Ok(Json(json!({
        "file": file,
        "meta": {
            "name": m.name, "kind": m.kind, "district": m.district, "region": m.region,
            "centerLat": m.center_lat, "centerLon": m.center_lon, "spanM": m.span_m,
            "mode": m.mode, "terrainSize": m.terrain_size,
            "roads": m.n_roads, "buildings": m.n_buildings, "rivers": m.n_rivers,
        },
        "layers": {
            "roads": fc(roads, json!({"kind": "lines"})),
            "rivers": fc(rivers, json!({"kind": "lines", "label": "Rivers"})),
            "rails": fc(rails, json!({"kind": "lines", "label": "Railways"})),
            "water": fc(water, json!({"kind": "polygons", "label": "Lakes / water"})),
            "landcover": fc(landcover, json!({"kind": "polygons", "label": "Vegetation & land use"})),
            "buildings": fc(buildings, json!({"kind": "polygons", "label": "Building footprints"})),
            "boundary": fc(boundary, json!({"kind": "lines", "label": "Boundary / districts / wards"})),
            "terrain": {"stats": terrain},
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rings_style(_cls: &str) -> Value {
        json!({"color": "#fff"})
    }

    #[test]
    fn roads_v2_keeps_class_ref_and_points() {
        // v2: a b lanes oneway speed npts [lat lon]* class ref
        let txt = "2 1\n-6.8 39.2\n-6.81 39.21\n0 1 1 0 18 2 -6.805 39.205 -6.808 39.208 primary T1\n";
        let f = parse_roads(txt);
        assert_eq!(f.len(), 1);
        let p = &f[0]["properties"];
        assert_eq!(p["class"], json!("primary"));
        assert_eq!(p["ref"], json!("T1"));
        assert_eq!(p["color"], json!("#facc15"));
        // endpoints + 2 interior points
        assert_eq!(
            f[0]["geometry"]["coordinates"].as_array().unwrap().len(),
            4
        );
    }

    #[test]
    fn roads_v1_without_class_ref_keeps_last_point() {
        // v1 (stress bundle): no trailing class/ref — last pair is geometry
        let txt = "2 1\n-6.8 39.2\n-6.81 39.21\n0 1 1 0 50 1 -6.805 39.205\n";
        let f = parse_roads(txt);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["properties"]["class"], json!(""));
        assert_eq!(
            f[0]["geometry"]["coordinates"].as_array().unwrap().len(),
            3
        );
    }

    #[test]
    fn rings_write_polys_shapes() {
        // header {nrings} {class} (class may be empty) + ring lines
        let txt = "2\n1 \n4 -6.6 39.0 -6.7 39.0 -6.7 39.1 -6.6 39.0\n1 district\n4 -6.5 39.0 -6.6 39.0 -6.6 39.1 -6.5 39.0\n";
        let f = parse_rings(txt, &rings_style);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0]["properties"]["class"], json!(""));
        assert_eq!(f[1]["properties"]["class"], json!("district"));
    }

    #[test]
    fn rings_landcover_next_line_coords() {
        // write_landcover: {n} {class} + coords on the NEXT line
        let txt = "1\n4 wood\n-6.6 39.0 -6.7 39.0 -6.7 39.1 -6.65 39.05\n";
        let f = parse_rings(txt, &rings_style);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["properties"]["class"], json!("wood"));
        // closed ring
        assert_eq!(
            f[0]["geometry"]["coordinates"][0].as_array().unwrap().len(),
            5
        );
    }

    #[test]
    fn buildings_and_lines() {
        let b = "1\n4.0 0.0 4\n-6.6 39.0 -6.7 39.0 -6.7 39.1 -6.6 39.0\n";
        let bf = parse_buildings(b);
        assert_eq!(bf.len(), 1);
        assert_eq!(bf[0]["properties"]["height"], json!(4.0));
        let l = "1\n3 river -6.6 39.0 -6.7 39.1 -6.8 39.2\n";
        let lf = parse_lines(l, "#4682b4", 2.0, false);
        assert_eq!(lf.len(), 1);
        assert_eq!(lf[0]["properties"]["class"], json!("river"));
    }
}
