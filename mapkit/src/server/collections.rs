//! Collections API — the zone registry endpoints.
//!
//!   GET  /api/collections              list registered zones (SQLite, synced
//!                                      against assets/collections on boot)
//!   POST /api/collections/grab         grab a drawn polygon or admin zone
//!                                      into a new `.mapbin` — runs the
//!                                      mapkit grabber IN-PROCESS (no
//!                                      mapkit-cli binary needed), blocks
//!                                      until done (hard 20 min cap)
//!   GET  /api/collections/download    download one bundle (name-guarded)

use std::path::Path;
use std::time::{Duration, Instant};

use rocket::http::{ContentType, Status};
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::State;
use serde_json::json;

use super::db;
use super::ServerState;

const GRAB_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// One zone row as the API returns it (camelCase, same shape the old web
/// service used so tooling keeps working).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZoneDto {
    pub file: String,
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub district: String,
    pub region: String,
    pub center_lat: f64,
    pub center_lon: f64,
    pub span_m: f64,
    pub mode: String,
    pub terrain_size: i32,
    pub roads: i32,
    pub buildings: i32,
    pub rivers: i32,
    pub size_bytes: i32,
    pub status: String,
}

fn zone_dto(z: &super::models::Zone) -> ZoneDto {
    ZoneDto {
        file: format!("{}.mapbin", z.slug),
        slug: z.slug.clone(),
        name: z.name.clone(),
        kind: z.kind.clone(),
        district: z.district.clone(),
        region: z.region.clone(),
        center_lat: z.center_lat,
        center_lon: z.center_lon,
        span_m: z.span_m,
        mode: z.mode.clone(),
        terrain_size: z.terrain_size,
        roads: z.n_roads,
        buildings: z.n_buildings,
        rivers: z.n_rivers,
        size_bytes: z.bytes,
        status: z.status.clone(),
    }
}

fn default_collection_name(root: &Path) -> Option<String> {
    let txt = std::fs::read_to_string(root.join("assets/collections/config.txt")).ok()?;
    for line in txt.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("default_collection=") {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// GET /api/collections — list every registered zone.
#[get("/api/collections")]
pub fn list(state: &State<ServerState>) -> Json<serde_json::Value> {
    let st = state.inner();
    let mut conn = st.conn.lock().unwrap();
    // re-sync first so bundles added by mapkit-cli since boot show up
    db::sync_collections_dir(&mut conn, &st.collections_dir());
    let zones = db::list_zones(&mut conn);
    Json(json!({
        "collections": zones.iter().map(zone_dto).collect::<Vec<_>>(),
        "defaultCollection": default_collection_name(&st.root),
        "dir": st.collections_dir(),
    }))
}

#[derive(Deserialize)]
pub struct GrabReq {
    pub name: String,
    /// open ring, >= 3 [lng, lat] points (drawn zone). Omit when `admin`
    /// is set — the zone then comes from the Tanzania admin pack.
    #[serde(default)]
    pub polygon: Vec<(f64, f64)>,
    /// admin source query, mapkit-cli --admin syntax
    /// ("region:Dar es Salaam", "district:Ilala", "ward:Mbezi").
    #[serde(default)]
    pub admin: Option<String>,
    #[serde(default)]
    pub force_city: bool,
    /// lower bound on road class (grabber's --min-class)
    #[serde(default)]
    pub min_class: Option<String>,
    /// selectable preview: per-class allow-lists (None/empty = keep all)
    #[serde(default)]
    pub road_classes: Option<Vec<String>>,
    #[serde(default)]
    pub water_classes: Option<Vec<String>>,
    #[serde(default)]
    pub waterway_classes: Option<Vec<String>>,
    #[serde(default)]
    pub landcover_classes: Option<Vec<String>>,
    /// toggles (default true when omitted)
    pub include_buildings: Option<bool>,
    pub include_rails: Option<bool>,
    pub include_waterways: Option<bool>,
    pub include_boundary_sub: Option<bool>,
}

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .replace('\'', "")
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Clean an allow-list: trim, drop empties, None when nothing selected
/// (None = keep all, same as the CLI's --x-classes parsing).
fn clean_list(v: &Option<Vec<String>>) -> Option<Vec<String>> {
    match v {
        Some(items) => {
            let kept: Vec<String> = items
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if kept.is_empty() {
                None
            } else {
                Some(kept)
            }
        }
        None => None,
    }
}

/// Insert points every ~`step` degrees along a ring so coarse drawn
/// polygons clip cleanly (same as mapkit-cli).
fn densify_ring(ring: &[(f64, f64)], step: f64) -> Vec<(f64, f64)> {
    if ring.len() < 2 {
        return ring.to_vec();
    }
    let mut out = Vec::with_capacity(ring.len() * 4);
    for i in 0..ring.len() - 1 {
        let (x0, y0) = ring[i];
        let (x1, y1) = ring[i + 1];
        let d = ((x1 - x0).hypot(y1 - y0)).max(1e-12);
        let n = (d / step).ceil() as usize;
        for k in 0..n {
            let t = k as f64 / n as f64;
            out.push((x0 + t * (x1 - x0), y0 + t * (y1 - y0)));
        }
    }
    out.push(*ring.last().unwrap());
    out
}

/// POST /api/collections/grab — grab a drawn zone into a `.mapbin`.
///
/// Runs the mapkit grabber (`crate::grab`) on a worker thread IN this
/// process — no mapkit-cli binary needed. Accepts either a drawn `polygon`
/// or an `admin` query (`region:`/`district:`/`ward:`, resolved against the
/// cached Tanzania pack, same as the CLI's --admin). Waits with a hard
/// 20 min cap, registers the bundle and returns its manifest metadata.
#[post("/api/collections/grab", format = "json", data = "<req>")]
pub fn grab(req: Json<GrabReq>, state: &State<ServerState>) -> Result<Json<serde_json::Value>, status::Custom<Json<serde_json::Value>>> {
    let st = state.inner();
    let name = req.name.trim().to_string();
    let poly = &req.polygon;
    if name.is_empty() {
        return Err(status::Custom(
            Status::BadRequest,
            Json(json!({ "error": "name required" })),
        ));
    }
    if poly.len() < 3 && req.admin.as_ref().map(|a| a.trim().is_empty()).unwrap_or(true) {
        return Err(status::Custom(
            Status::BadRequest,
            Json(json!({ "error": "polygon needs >= 3 [lng,lat] points, or pass admin (region:/district:/ward:…)" })),
        ));
    }
    let use_admin = poly.len() < 3;
    let admin_query = req.admin.clone().map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
    if use_admin && admin_query.is_none() {
        return Err(status::Custom(
            Status::BadRequest,
            Json(json!({ "error": "admin query required when no polygon is given" })),
        ));
    }

    let slug = slugify(&name);
    if slug.is_empty() {
        return Err(status::Custom(
            Status::BadRequest,
            Json(json!({ "error": "name produces no usable file name" })),
        ));
    }

    // drawn zone: build rings directly (densified like the CLI does)
    let spec = if use_admin {
        let q = admin_query.clone().unwrap_or_default();
        let packdir = st.root.join("assets/tanzania-pack");
        let bdir = packdir.join("boundaries");
        let idx = super::state::AdminCache::get_or_load(&st.admin, &bdir).ok_or_else(|| {
            status::Custom(
                Status::ServiceUnavailable,
                Json(json!({ "error": format!("admin pack missing at {}", bdir.display()) })),
            )
        })?;
        let Some(z) = idx.find(&q) else {
            return Err(status::Custom(
                Status::NotFound,
                Json(json!({ "error": format!("zone '{q}' not found (try region:/district:/ward: prefixes)") })),
            ));
        };
        let (district, region) = idx.parents(z);
        crate::grab::ZoneSpec {
            name: name.clone(),
            kind: match z.adm {
                1 => "Region".into(),
                2 => "District".into(),
                _ => "Ward".into(),
            },
            district,
            region,
            source: "tanzania-pack".into(),
            adm: z.adm,
            rings: z.rings.clone(),
            bbox: z.bbox,
        }
    } else {
        let mut rings = vec![poly.clone()];
        for r in &mut rings {
            *r = densify_ring(r, 0.002);
        }
        rings.retain(|r| r.len() >= 3);
        if rings.is_empty() {
            return Err(status::Custom(
                Status::BadRequest,
                Json(json!({ "error": "polygon has no usable rings" })),
            ));
        }
        let bbox = crate::grab::bbox_of(&rings);
        crate::grab::ZoneSpec {
            name: name.clone(),
            kind: "Zone".into(),
            district: String::new(),
            region: String::new(),
            source: String::new(),
            adm: 0,
            rings,
            bbox,
        }
    };

    let selection = crate::grab::GrabSelection {
        road_allow: clean_list(&req.road_classes),
        water_allow: clean_list(&req.water_classes),
        waterway_allow: clean_list(&req.waterway_classes),
        landcover_allow: clean_list(&req.landcover_classes),
        include_buildings: req.include_buildings.unwrap_or(true),
        include_rails: req.include_rails.unwrap_or(true),
        include_waterways: req.include_waterways.unwrap_or(true),
        include_boundary_sub: req.include_boundary_sub.unwrap_or(true),
    };
    let opts = crate::grab::GrabOptions {
        cache: st.tilecache_dir().to_string_lossy().to_string(),
        span_cap_m: 80_000.0,
        force_city: req.force_city,
        min_class: None,
        packdir: Some(st.root.join("assets/tanzania-pack").to_string_lossy().to_string()),
        selection,
    };
    let min_class = req.min_class.clone();
    let out_file = st.collections_dir().join(format!("{slug}.mapbin"));

    // Run the grabber on a worker thread (thread-locals are per-thread, so
    // the detail floor + selection are set INSIDE the worker). Wait with a
    // hard cap; on timeout the worker keeps running detached and the
    // request fails — the bundle lands on disk whenever it finishes.
    eprintln!("[SERVER] grab '{name}' -> {slug} (in-process)");
    let started = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    let out_file_w = out_file.clone();
    std::thread::Builder::new()
        .name("mapkit-grab".into())
        .spawn(move || {
            crate::grab::set_min_class(min_class);
            crate::grab::set_selection(opts.selection.clone());
            let out = crate::grab::grab(&spec, &opts).and_then(|bundle| {
                let bytes = bundle.write_to(&out_file_w).map_err(|e| e.to_string())?;
                Ok((bundle, bytes))
            });
            let _ = tx.send(out);
        })
        .map_err(|e| {
            status::Custom(
                Status::InternalServerError,
                Json(json!({ "error": format!("spawn grab thread: {e}") })),
            )
        })?;
    let result: Result<(crate::bundle::Bundle, u64), String> = match rx.recv_timeout(GRAB_TIMEOUT) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("[SERVER] grab '{name}' timed out after 20 min (worker detached)");
            return Err(status::Custom(
                Status::InternalServerError,
                Json(json!({ "error": "grab timed out after 20 min" })),
            ));
        }
    };
    let (bundle, bytes) = match result {
        Ok(v) => v,
        Err(e) => {
            return Err(status::Custom(
                Status::InternalServerError,
                Json(json!({ "error": format!("grab failed: {e}"), "file": out_file.display().to_string() })),
            ));
        }
    };

    // sidecar report for humans (same shape as mapkit-cli writes)
    let m0 = &bundle.meta;
    let _ = std::fs::write(
        st.collections_dir().join(format!("{slug}.report.txt")),
        format!(
            "name: {}\nkind: {}\ndistrict: {}\nregion: {}\ncenter: {:.5}, {:.5}\nspan: {:.0} m\nmode: {}\nintersections: {}\nroads: {}\nbuildings: {}\nrivers: {}\nbytes: {}\ntook: {:.1}s\n",
            m0.name, m0.kind, m0.district, m0.region,
            m0.center_lat, m0.center_lon, m0.span_m, m0.mode,
            m0.n_inters, m0.n_roads, m0.n_buildings, m0.n_rivers,
            bytes, started.elapsed().as_secs_f32(),
        ),
    );

    // register + answer with the probed manifest
    let mut conn = st.conn.lock().unwrap();
    let zones = db::sync_collections_dir(&mut conn, &st.collections_dir());
    drop(conn);
    let meta = crate::bundle::Bundle::probe_file(&out_file).ok();

    eprintln!(
        "[SERVER] grab '{}' done in {:.1}s — {} (zones now: {})",
        name,
        started.elapsed().as_secs_f32(),
        out_file.display(),
        zones
    );

    let m = meta.unwrap_or_default();
    Ok(Json(json!({
        "ok": true,
        "file": format!("{slug}.mapbin"),
        "zones": zones,
        "meta": {
            "name": m.name, "kind": m.kind, "district": m.district, "region": m.region,
            "zoneSource": m.zone_source, "zoneAdm": m.zone_adm, "zoneQuery": m.zone_query,
            "centerLat": m.center_lat, "centerLon": m.center_lon, "spanM": m.span_m,
            "mode": m.mode, "terrainSize": m.terrain_size,
            "roads": m.n_roads, "buildings": m.n_buildings, "rivers": m.n_rivers,
            "sizeBytes": std::fs::metadata(&out_file).map(|md| md.len()).unwrap_or(0),
        }
    })))
}

/// A bundle streamed as an attachment.
pub struct BundleFile {
    pub bytes: Vec<u8>,
    pub name: String,
}

impl<'r> rocket::response::Responder<'r, 'static> for BundleFile {
    fn respond_to(self, _: &'r rocket::Request<'_>) -> rocket::response::Result<'static> {
        rocket::Response::build()
            .header(ContentType::Binary)
            .header(rocket::http::Header::new(
                "Content-Disposition",
                format!("attachment; filename=\"{}\"", self.name),
            ))
            .sized_body(self.bytes.len(), std::io::Cursor::new(self.bytes))
            .ok()
    }
}

/// GET /api/collections/download?file=<slug>.mapbin — stream one bundle.
#[get("/api/collections/download?<file>")]
pub fn download(file: String, state: &State<ServerState>) -> Result<BundleFile, status::Custom<Json<serde_json::Value>>> {
    let st = state.inner();
    if !file.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || !file.ends_with(".mapbin")
        || file.contains("..")
    {
        return Err(status::Custom(
            Status::BadRequest,
            Json(json!({ "error": "bad file name" })),
        ));
    }
    let p = st.collections_dir().join(&file);
    match std::fs::read(&p) {
        Ok(bytes) => Ok(BundleFile { bytes, name: file }),
        Err(_) => Err(status::Custom(
            Status::NotFound,
            Json(json!({ "error": "not found" })),
        )),
    }
}
