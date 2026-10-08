//! XYZ tile streaming — the basemap/vector/terrain planes.
//!
//! Every tile is served straight off the shared `tilecache/` directory:
//!
//!   GET /tiles/<z>/<x>/<y>?style=<id>
//!                              retina (1024 px `@2x`) raster basemap, style in
//!                              streets|navigation|satellite|satellite-streets|
//!                              outdoors|light|dark (default streets; Mapbox
//!                              style → matching Esri fallback, Esri is 256 px)
//!                              cache: tilecache/basemap/{id}_{z}_{x}_{y}_2x.png|.jpg
//!   GET /vector/<z>/<x>/<y>    Mapbox streets-v8 MVT
//!                              cache: tilecache/v8_z{z}_{x}_{y}.mvt   (SAME
//!                              files the mapkit-cli grabber consumes)
//!   GET /terrain/<z>/<x>/<y>   Mapbox terrain-RGB DEM PNG
//!                              cache: tilecache/dem_z{z}_{x}_{y}.png   (SAME)
//!
//! On a cache miss the server backfills from the upstream (blocking), saves
//! atomically and records the tile in the SQLite index.

use std::path::PathBuf;

use rocket::http::{ContentType, Header, Status};
use rocket::serde::json::Json;
use rocket::{Request, Response, State};
use serde_json::json;

use super::db;
use super::ServerState;
use super::upstream;

const TILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct TileResponse {
    pub bytes: Vec<u8>,
    pub content_type: ContentType,
}

impl<'r> rocket::response::Responder<'r, 'static> for TileResponse {
    fn respond_to(self, _: &'r Request<'_>) -> rocket::response::Result<'static> {
        Response::build()
            .header(self.content_type)
            .header(Header::new("Cache-Control", "public, max-age=86400"))
            .sized_body(self.bytes.len(), std::io::Cursor::new(self.bytes))
            .ok()
    }
}

fn check_coords(z: u32, x: u32, y: u32) -> Result<(), (Status, Json<serde_json::Value>)> {
    let n = 1u32.checked_shl(z).unwrap_or(0);
    let bad = z > 19 || n == 0 || x >= n || y >= n;
    if bad {
        return Err((
            Status::BadRequest,
            Json(json!({ "error": "tile coords out of range" })),
        ));
    }
    Ok(())
}

fn serve_cached(path: PathBuf, ct: ContentType) -> Option<TileResponse> {
    let bytes = std::fs::read(path).ok()?;
    Some(TileResponse {
        bytes,
        content_type: ct,
    })
}

fn fetch_and_cache(
    state: &ServerState,
    z: u32,
    x: u32,
    y: u32,
    source: &str,
    url: String,
    path: PathBuf,
    ct: ContentType,
) -> Result<TileResponse, (Status, Json<serde_json::Value>)> {
    let bytes = upstream::fetch_mapbox(&url, TILE_TIMEOUT).map_err(|e| {
        eprintln!("[SERVER] tile backfill failed {z}/{x}/{y} ({source}): {e}");
        (
            Status::BadGateway,
            Json(json!({ "error": format!("all upstream sources failed: {e}") })),
        )
    })?;
    // Mapbox signals errors as small JSON bodies — never cache or serve those
    if bytes.starts_with(b"{") || bytes.len() < 100 {
        eprintln!(
            "[SERVER] tile backfill {z}/{x}/{y} ({source}): upstream returned an error body ({} bytes)",
            bytes.len()
        );
        return Err((
            Status::BadGateway,
            Json(json!({ "error": "upstream returned an error for this tile" })),
        ));
    }
    match upstream::write_atomic(&path, &bytes) {
        Ok(_) => {
            let mut conn = state.conn.lock().unwrap();
            db::record_tile(&mut conn, z, x, y, source, bytes.len());
        }
        Err(e) => eprintln!("[SERVER] cache write failed {}: {e}", path.display()),
    }
    Ok(TileResponse {
        bytes,
        content_type: ct,
    })
}

/// GET /tiles/<z>/<x>/<y>?style=<id> — retina raster basemap (1024 px
/// `@2x` style tiles). Style defaults to streets; unknown ids are a 400.
/// Mapbox first (same account/token as the grabber), matching Esri look
/// as fallback (OSM/CARTO block datacenter IPs).
#[get("/tiles/<z>/<x>/<y>?<style>")]
pub fn raster(
    z: u32,
    x: u32,
    y: u32,
    style: Option<String>,
    state: &State<ServerState>,
) -> Result<TileResponse, (Status, Json<serde_json::Value>)> {
    check_coords(z, x, y)?;
    let style = match style.as_deref().map(crate::style::BasemapStyle::parse) {
        Some(None) => {
            return Err((
                Status::BadRequest,
                Json(json!({                     "error": "unknown basemap style (streets|navigation|satellite|satellite-streets|outdoors|light|dark)" })),
            ))
        }
        Some(Some(v)) => v,
        None => crate::style::BasemapStyle::default(),
    };
    let st = state.inner();
    let sid = style.id();

    let png = st.root.join(format!("tilecache/basemap/{sid}_{z}_{x}_{y}_2x.png"));
    let jpg = st.root.join(format!("tilecache/basemap/{sid}_{z}_{x}_{y}_2x.jpg"));

    if let Some(resp) = serve_cached(png.clone(), ContentType::PNG) {
        return Ok(resp);
    }
    if let Some(resp) = serve_cached(jpg.clone(), ContentType::JPEG) {
        return Ok(resp);
    }

    let token = crate::grab::token();
    let mapbox = format!(
        "https://api.mapbox.com/styles/v1/mapbox/{}/tiles/{z}/{x}/{y}@2x?access_token={token}",
        style.mapbox_style(),
    );
    let esri = style.esri_url(z, x, y);

    match upstream::fetch_mapbox(&mapbox, TILE_TIMEOUT) {
        Ok(bytes) if bytes.len() > 800 => {
            let _ = upstream::write_atomic(&png, &bytes);
            let mut conn = st.conn.lock().unwrap();
            db::record_tile(&mut conn, z, x, y, &format!("raster-mapbox-{sid}"), bytes.len());
            Ok(TileResponse { bytes, content_type: ContentType::PNG })
        }
        _ => {
            let bytes = upstream::fetch(&esri, TILE_TIMEOUT).map_err(|e| {
                (
                    Status::BadGateway,
                    Json(json!({ "error": format!("all tile sources failed: {e}") })),
                )
            })?;
            let _ = upstream::write_atomic(&jpg, &bytes);
            let mut conn = st.conn.lock().unwrap();
            db::record_tile(&mut conn, z, x, y, &format!("raster-{}-{sid}", style.esri_kind()), bytes.len());
            Ok(TileResponse { bytes, content_type: ContentType::JPEG })
        }
    }
}

/// GET /vector/<z>/<x>/<y> — Mapbox streets-v8 vector tile (MVT).
/// Shares the grabber's cache files (`tilecache/v8_z{z}_{x}_{y}.mvt`).
#[get("/vector/<z>/<x>/<y>")]
pub fn vector(z: u32, x: u32, y: u32, state: &State<ServerState>) -> Result<TileResponse, (Status, Json<serde_json::Value>)> {
    check_coords(z, x, y)?;
    let st = state.inner();
    let path = st.root.join(format!("tilecache/v8_z{z}_{x}_{y}.mvt"));
    if let Some(resp) = serve_cached(path.clone(), ContentType::new("application", "octet-stream")) {
        return Ok(resp);
    }
    let url = format!(
        "https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.mvt?access_token={}",
        crate::grab::token()
    );
    fetch_and_cache(st, z, x, y, "vector", url, path, ContentType::new("application", "octet-stream"))
}

/// GET /terrain/<z>/<x>/<y> — Mapbox terrain-RGB DEM PNG.
/// Shares the grabber's cache files (`tilecache/dem_z{z}_{x}_{y}.png`).
#[get("/terrain/<z>/<x>/<y>")]
pub fn terrain(z: u32, x: u32, y: u32, state: &State<ServerState>) -> Result<TileResponse, (Status, Json<serde_json::Value>)> {
    check_coords(z, x, y)?;
    let st = state.inner();
    let path = st.root.join(format!("tilecache/dem_z{z}_{x}_{y}.png"));
    if let Some(resp) = serve_cached(path.clone(), ContentType::PNG) {
        return Ok(resp);
    }
    let url = format!(
        "https://api.mapbox.com/v4/mapbox.terrain-rgb/{z}/{x}/{y}.png?access_token={}",
        crate::grab::token()
    );
    fetch_and_cache(st, z, x, y, "terrain", url, path, ContentType::PNG)
}
