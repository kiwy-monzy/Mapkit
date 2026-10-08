//! The mapkit grabber — ported from mapstudio-rs `ego`.
//!
//! Takes a zone polygon (any shape: admin boundary, drawn mapkit zone,
//! test polygon), downloads the Mapbox vector tiles + terrain-RGB DEM that
//! cover it, joins roads into a graph, carves water into the DEM and packs
//! every layer into one `.mapbin` bundle.

use crate::boundaries::{point_in_ring, AdminIndex};
use crate::bundle::{Bundle, BundleBuilder, BundleMeta};
use crate::mercator::{lonlat_to_tile, tile_span_m, Mercator};
use crate::mvt::{self, TileData, TileRef};
use crate::terrain;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

thread_local! {
    static MIN_CLASS: RefCell<Option<String>> = const { RefCell::new(None) };
    static SELECTION: RefCell<Option<GrabSelection>> = const { RefCell::new(None) };
}

fn selection() -> GrabSelection {
    SELECTION.with(|s| s.borrow().clone().unwrap_or_else(GrabSelection::all))
}

/// Per-class allow-lists for a grab — built by the web UI's selectable
/// preview (`POST /api/collections/preview` → checkboxes) and honoured by
/// [`grab`]. `None` = keep every class, `Some(list)` = keep only listed.
#[derive(Clone, Debug)]
pub struct GrabSelection {
    pub road_allow: Option<Vec<String>>,
    pub water_allow: Option<Vec<String>>,
    pub waterway_allow: Option<Vec<String>>,
    pub landcover_allow: Option<Vec<String>>,
    pub include_buildings: bool,
    pub include_rails: bool,
    pub include_waterways: bool,
    pub include_boundary_sub: bool,
}

impl GrabSelection {
    pub fn all() -> Self {
        Self {
            road_allow: None,
            water_allow: None,
            waterway_allow: None,
            landcover_allow: None,
            include_buildings: true,
            include_rails: true,
            include_waterways: true,
            include_boundary_sub: true,
        }
    }

    fn allows(list: &Option<Vec<String>>, class: &str) -> bool {
        match list {
            None => true,
            Some(v) => v.iter().any(|c| c == class),
        }
    }
}

/// set the road-class floor for the current thread (grab entry point)
pub fn set_min_class(c: Option<String>) {
    MIN_CLASS.with(|m| *m.borrow_mut() = c);
}

/// set the per-class selection for the current thread (grab entry point)
pub fn set_selection(s: GrabSelection) {
    SELECTION.with(|m| *m.borrow_mut() = Some(s));
}

/// parse a `--x-classes a,b,c` csv flag (empty/missing = keep all)
pub fn parse_csv(v: Option<String>) -> Option<Vec<String>> {
    match v {
        None => None,
        Some(s) => {
            let items: Vec<String> = s
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            if items.is_empty() {
                None
            } else {
                Some(items)
            }
        }
    }
}

/// Mapbox public token — comes from the `MAPBOX_TOKEN` env var / `.env`
/// file (see `.env.example` at the repo root). There is deliberately NO
/// hardcoded fallback: shipping a secret in source leaks it to every clone.
/// When unset, [`tokens`] returns an empty list and callers fail with a
/// clear "set MAPBOX_TOKEN" message instead of a cryptic 401.
pub const DEFAULT_TOKEN: &str = "";
/// Second public token (URL-restricted: only answers with the chatgpt.com
/// referer — the fetcher sends it automatically). Comes from
/// `MAPBOX_TOKEN_2`. Empty = single-key mode. Two keys double the rate
/// quota and cover each other.
pub const SECONDARY_TOKEN: &str = "";
/// Referer the secondary token requires.
pub const SECONDARY_REFERER: &str = "https://chatgpt.com/";

/// vertical exaggeration of the baked DEM heights in city mode — mapmap-scene's
/// `TERRAIN_EXAGGERATION`: at true scale a city's ~60 m hills vanish over a
/// 100 km window, 2.2× is the classic cartographic value that makes relief read
pub const TERRAIN_EXAGGERATION: f32 = 2.2;

static TOKEN_ROUND_ROBIN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Load `.env` once per process (walks up from the cwd, so both the repo
/// root `.env` and `mapkit/.env` work). Called lazily by [`tokens`] and
/// eagerly by both binary entry points.
static DOTENV_ONCE: std::sync::Once = std::sync::Once::new();

/// Load `.env` into `std::env` if present. Idempotent — safe to call from
/// every entry point (`mapkit-cli`, the standalone server, the embedded
/// game thread).
pub fn load_dotenv() {
    DOTENV_ONCE.call_once(|| {
        let _ = dotenvy::dotenv();
    });
}

/// `true` when at least one Mapbox key is configured.
pub fn has_tokens() -> bool {
    !tokens().is_empty()
}

pub fn token() -> String {
    let all = tokens();
    if all.is_empty() {
        return String::new();
    }
    let i = TOKEN_ROUND_ROBIN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    all[i % all.len()].0.clone()
}

/// Every configured key as (token, referer or None), primary first.
/// Concurrent workers spread load across both via [`token`]'s round robin.
///
/// Sources, in order: process env, then `.env` (auto-loaded on first call).
/// Empty / missing values are skipped, so unset keys simply mean fewer
/// keys — never a hardcoded secret. See `.env.example` for the full list.
pub fn tokens() -> Vec<(String, Option<String>)> {
    load_dotenv();
    let primary =
        std::env::var("MAPBOX_TOKEN").unwrap_or_else(|_| DEFAULT_TOKEN.to_string());
    let secondary =
        std::env::var("MAPBOX_TOKEN_2").unwrap_or_else(|_| SECONDARY_TOKEN.to_string());
    let primary = primary.trim().to_string();
    let secondary = secondary.trim().to_string();
    let mut out: Vec<(String, Option<String>)> = Vec::with_capacity(2);
    if !primary.is_empty() {
        out.push((primary.clone(), None));
    }
    if !secondary.is_empty() && secondary != primary {
        out.push((secondary, Some(SECONDARY_REFERER.to_string())));
    }
    out
}

/// Human-readable hint printed when a fetch/grab runs without any key.
pub fn missing_token_hint() -> &'static str {
    "no Mapbox token configured: set MAPBOX_TOKEN (and optionally MAPBOX_TOKEN_2) \
     in the environment or copy .env.example to .env — see .env.example at the repo root"
}

/// A zone to grab: polygon (lon/lat rings) + display names.
#[derive(Clone, Debug, Default)]
pub struct ZoneSpec {
    pub name: String,
    pub kind: String,
    pub district: String,
    pub region: String,
    /// zone-registry link: which admin pack this zone came from
    /// ("geoBoundaries-TZA") and at which ADM level (1/2/3; 0 = drawn poly)
    pub source: String,
    pub adm: u8,
    /// outer rings, (lon, lat)
    pub rings: Vec<Vec<(f64, f64)>>,
    /// min_lon, min_lat, max_lon, max_lat
    pub bbox: (f64, f64, f64, f64),
}

#[derive(Clone, Debug)]
pub struct GrabOptions {
    /// directory for the tile cache (v8_zX_Y.mvt / dem_zX_Y.png)
    pub cache: String,
    /// cap of the data window per side, ground metres
    pub span_cap_m: f64,
    /// force city mode (flat land) regardless of relief
    pub force_city: bool,
    /// drop road classes of this detail level and below (e.g. "service")
    /// to tame the graph size on very large windows
    pub min_class: Option<String>,
    /// tanzania-pack directory — when set (and the zone is a polygon),
    /// district (ADM2) and ward (ADM3) boundaries inside the zone are
    /// written into the boundary layer as `district` / `ward` classes
    pub packdir: Option<String>,
    /// per-class selection (selectable preview): allow-lists + toggles.
    /// `Default` keeps everything (old behaviour).
    pub selection: GrabSelection,
}

impl Default for GrabOptions {
    fn default() -> Self {
        GrabOptions {
            cache: "tilecache".into(),
            span_cap_m: 80_000.0,
            force_city: false,
            min_class: None,
            packdir: None,
            selection: GrabSelection::all(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    City,
    Mountain,
}

/// bbox of a set of rings
pub fn bbox_of(rings: &[Vec<(f64, f64)>]) -> (f64, f64, f64, f64) {
    let mut b = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for r in rings {
        for (x, y) in r {
            b.0 = b.0.min(*x);
            b.1 = b.1.min(*y);
            b.2 = b.2.max(*x);
            b.3 = b.3.max(*y);
        }
    }
    b
}

/// Run the whole pipeline and return the finished bundle (not written anywhere).
pub fn grab(spec: &ZoneSpec, opts: &GrabOptions) -> Result<Bundle, String> {
    if !has_tokens() {
        return Err(missing_token_hint().to_string());
    }
    // publish the per-class selection for join_roads (thread-local, like min_class)
    set_selection(opts.selection.clone());
    let sel = opts.selection.clone();
    let cache = opts.cache.as_str();
    let bbox = if spec.rings.is_empty() {
        spec.bbox
    } else {
        bbox_of(&spec.rings)
    };
    let centre_lon = (bbox.0 + bbox.2) / 2.0;
    let centre_lat = (bbox.1 + bbox.3) / 2.0;
    let m = Mercator::new(centre_lat, centre_lon);
    let (x0, y0) = m.project(bbox.0, bbox.1);
    let (x1, y1) = m.project(bbox.2, bbox.3);
    let diag_m = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();

    let vec_z: u32 = if diag_m < 8_000.0 {
        15
    } else if diag_m < 40_000.0 {
        14
    } else {
        13
    };
    let dem_z = vec_z - 1;

    let raw_span = ((x1 - x0).max(y1 - y0) + 1_200.0).min(opts.span_cap_m);
    // The engine terrain is exactly `terrain_size` chunks of 512 m (the
    // heightmap pipeline auto-fits textures up to 256×256 chunks) — size
    // the DEM window to FILL the playable window so the zone mask and the
    // ocean rim reach every edge instead of stopping at a smaller square.
    let terrain_size: u16 = ((raw_span / 512.0).ceil() as u16 + 2).clamp(16, 256);
    let span_m = (terrain_size as f64 - 2.0) * 512.0;

    println!(
        "── mapkit grab · {} ──\nzone: {} rings · centre {:.4}, {:.4}\nwindow {:.1} km · DEM z{} · vector z{} · terrain {} chunks",
        spec.name,
        spec.rings.len(),
        centre_lat,
        centre_lon,
        span_m / 1000.0,
        dem_z,
        vec_z,
        terrain_size
    );

    // ── tiles ────────────────────────────────────────────────────────────
    let (vx0, vy0, vx1, vy1) = tile_window_raw(centre_lat, centre_lon, vec_z, span_m + 2_000.0);
    let (dx0, dy0, dx1, dy1) = tile_window_raw(centre_lat, centre_lon, dem_z, span_m + 3_000.0);
    ensure_tiles(cache, vec_z, vx0, vy0, vx1, vy1, false);
    ensure_tiles(cache, dem_z, dx0, dy0, dx1, dy1, true);
    // ocean-mask tiles (flat 0.0, no bathymetry) and any still-missing DEM
    // tiles get a second chance from AWS Terrarium before the stitch, so
    // coastal land is backed by real relief instead of nodata pits
    terrain::backfill_ocean_tiles(cache, dem_z, dx0, dy0, dx1, dy1);

    // ── DEM ──────────────────────────────────────────────────────────────
    let dem = terrain::stitch(dem_z, dx0, dy0, dx1, dy1, cache)
        .ok_or("no DEM tiles could be decoded")?;
    let cells = ((span_m / 18.0) as usize).clamp(160, 768);
    let mut grid = terrain::resample_window(&dem, &m, span_m, cells);
    // nodata (NaN) → fill from valid neighbours (bounded: coastal gaps close
    // while the open ocean stays NaN); whatever remains becomes real sea
    // depth inside the zone and is left for the zone mask outside it.
    // Without this, 0.0 nodata flowed into the carve and stamped ~8 m deep
    // rectangular sea pits into coastal land (the sunk blocks).
    terrain::fill_missing(&mut grid, 12);
    {
        let win = span_m / 2.0 + 1_500.0;
        let bb = if spec.rings.is_empty() { bbox } else { bbox_of(&spec.rings) };
        for (i, v) in grid.data.iter_mut().enumerate() {
            if !v.is_nan() {
                continue;
            }
            let x = (i % grid.w) as f64 * grid.cell as f64 - span_m / 2.0;
            let y = (i / grid.w) as f64 * grid.cell as f64 - span_m / 2.0;
            let (lon, lat) = m.unproject(x, y);
            // cheap bbox reject first — point_in_ring over admin rings is
            // far too slow for hundreds of thousands of ocean cells
            let inside = lon >= bb.0 && lon <= bb.2 && lat >= bb.1 && lat <= bb.3
                && {
                    let (px, py) = m.project(lon, lat);
                    px.abs() < win
                        && py.abs() < win
                        && (spec.rings.is_empty()
                            || spec.rings.iter().any(|r| point_in_ring(lon, lat, r)))
                };
            *v = if inside { -8.0 } else { -30.0 };
        }
    }
    let peak = grid.data.iter().cloned().fold(f32::MIN, f32::max);
    let base = {
        let mut s = grid.data.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 20]
    };
    // mountains by relief only: a real climb > 800 m — big flat cities
    // (e.g. the whole Dar es Salaam region) stay flat city maps
    let relief = peak - base;
    let mode = if relief > 800.0 && !opts.force_city {
        Mode::Mountain
    } else {
        Mode::City
    };
    let vscale = if mode == Mode::Mountain {
        (1_900.0 / relief.max(1.0)).min(1.0)
    } else {
        1.0
    };
    println!(
        "raw DEM: {}×{} cells @ {:.1} m, {:.1} – {:.1} m (relief {:.0} m)",
        grid.w, grid.h, grid.cell, base, peak, relief
    );

    // ── vector extraction ────────────────────────────────────────────────
    let mut acc = TileData::default();
    let mut tiles_got = 0;
    for ty in vy0..vy1 {
        for tx in vx0..vx1 {
            let path = format!("{}/v8_z{}_{}_{}.mvt", cache, vec_z, tx, ty);
            if let Ok(raw) = std::fs::read(&path) {
                if let Some(td) = mvt::parse(&raw, &TileRef { z: vec_z, x: tx, y: ty }) {
                    acc.roads.extend(td.roads);
                    acc.buildings.extend(td.buildings);
                    acc.water.extend(td.water);
                    acc.waterways.extend(td.waterways);
                    acc.landcover.extend(td.landcover);
                    tiles_got += 1;
                }
            }
        }
    }
    let n_road_segs = acc.roads.iter().filter(|r| !r.rail).count();
    let n_rail_segs = acc.roads.len() - n_road_segs;
    println!(
        "extracted {}/{} vector tiles @z{}: {} buildings, {} roads, {} rails, {} water polys, {} waterways, {} landcover",
        tiles_got,
        (vx1 - vx0) * (vy1 - vy0),
        vec_z,
        acc.buildings.len(),
        n_road_segs,
        n_rail_segs,
        acc.water.len(),
        acc.waterways.len(),
        acc.landcover.len()
    );

    // ── buildings at z15 ─────────────────────────────────────────────────
    // mapbox-streets-v8 only carries usable building footprints from z15
    // (z13/z14 tiles have no `building` layer at all) — when the region
    // window forced a coarser vector zoom, fetch the zone's z15 tiles for
    // the building layer alone (tiles whose centre lies outside the zone
    // polygon are skipped so sea tiles aren't downloaded).
    if vec_z < 15 {
        let bz = 15u32;
        let (bx0, by0, bx1, by1) = tile_window_raw(centre_lat, centre_lon, bz, span_m + 2_000.0);
        let mut wanted: Vec<(u32, u32)> = Vec::new();
        let n14 = 1u32 << bz;
        for ty in by0..by1 {
            for tx in bx0..bx1 {
                let inside = spec.rings.is_empty() || {
                    let lon = (tx as f64 + 0.5) / n14 as f64 * 360.0 - 180.0;
                    let nrm = 1.0 - 2.0 * (ty as f64 + 0.5) / n14 as f64;
                    let lat =
                        (nrm * std::f64::consts::PI).sinh().atan().to_degrees();
                    spec.rings.iter().any(|r| point_in_ring(lon, lat, r))
                };
                if inside {
                    wanted.push((tx, ty));
                }
            }
        }
        ensure_tile_list(cache, bz, &wanted, false);
        let mut n_bld = 0usize;
        for &(tx, ty) in &wanted {
            let path = format!("{}/v8_z{}_{}_{}.mvt", cache, bz, tx, ty);
            if let Ok(raw) = std::fs::read(&path) {
                if let Some(td) = mvt::parse(&raw, &TileRef { z: bz, x: tx, y: ty }) {
                    n_bld += td.buildings.len();
                    acc.buildings.extend(td.buildings);
                }
            }
        }
        println!(
            "buildings pass @z{}: {}/{} tiles → +{} footprints",
            bz,
            wanted.len(),
            (bx1 - bx0) * (by1 - by0),
            n_bld
        );
    }

    // ── clip feature sets to the zone (before the carve: water polys feed
    // the terrain carving) ───────────────────────────────────────────────
    let win = span_m / 2.0 + 1_500.0;
    let pt_in = |lon: f64, lat: f64| -> bool {
        let (x, y) = m.project(lon, lat);
        x.abs() < win && y.abs() < win
    };
    let dbuf = 0.01; // ≈1 km in degrees
    let in_zone_bbox = |lon: f64, lat: f64| -> bool {
        lon >= bbox.0 - dbuf && lon <= bbox.2 + dbuf && lat >= bbox.1 - dbuf && lat <= bbox.3 + dbuf
    };
    let _ = in_zone_bbox;
    let in_zone_win = |lon: f64, lat: f64| -> bool {
        pt_in(lon, lat)
            && (spec.rings.is_empty() || spec.rings.iter().any(|r| point_in_ring(lon, lat, r)))
    };
    let poly_keep = |ring: &[(f64, f64)]| -> bool {
        if spec.rings.is_empty() {
            ring.iter().any(|&(lo, la)| pt_in(lo, la))
        } else {
            ring.iter()
                .any(|&(lo, la)| spec.rings.iter().any(|r| point_in_ring(lo, la, r)))
        }
    };
    // waterways/rails are LINES: keep only runs of consecutive points inside
    // the window and the polygon itself — a river or rail crossing the border
    // is cut at it instead of drawing over the carved-out sea.
    // Selection: toggles + per-class allow-lists from the preview UI.
    let mut waterways: Vec<(Vec<(f64, f64)>, String)> = Vec::new();
    if sel.include_waterways {
        for w in &acc.waterways {
            if !GrabSelection::allows(&sel.waterway_allow, &w.class) {
                continue;
            }
            for run in clip_line_runs(&w.pts, &in_zone_win) {
                waterways.push((run, w.class.clone()));
            }
        }
    }
    let mut rails: Vec<(Vec<(f64, f64)>, String)> = Vec::new();
    if sel.include_rails {
        for r in &acc.roads {
            if !r.rail {
                continue;
            }
            for run in clip_line_runs(&r.pts, &in_zone_win) {
                rails.push((run, r.class.clone()));
            }
        }
    }
    // Mapbox serves the open sea as TILE-CLIPPED fragments — carved as-is
    // they stamp square ocean blocks into the land. With a zone polygon the
    // mask alone defines the coast (the admin boundary follows it), so only
    // water polys centred INSIDE the zone (inland lakes) may carve.
    // Selection: water_allow filters classes (e.g. drop `ocean` but keep lakes).
    let water: Vec<mvt::PolyF> = if spec.rings.is_empty() {
        acc.water
            .iter()
            .filter(|w| {
                if !GrabSelection::allows(&sel.water_allow, &w.class) {
                    return false;
                }
                w.rings
                    .first()
                    .map(|r| r.iter().any(|&(lo, la)| pt_in(lo, la)))
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    } else {
        acc.water
            .iter()
            .filter(|w| {
                if !GrabSelection::allows(&sel.water_allow, &w.class) {
                    return false;
                }
                w.rings.first().map(|r| {
                    let n = r.len().max(1) as f64;
                    let (sx, sy) = r.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
                    spec.rings
                        .iter()
                        .any(|ring| point_in_ring(sx / n, sy / n, ring))
                }).unwrap_or(false)
            })
            .cloned()
            .collect()
    };
    let landcover: Vec<_> = acc
        .landcover
        .iter()
        .filter(|p| {
            if !GrabSelection::allows(&sel.landcover_allow, &p.class) {
                return false;
            }
            p.rings.first().map(|r| poly_keep(r)).unwrap_or(false)
        })
        .cloned()
        .collect();
    let buildings: Vec<_> = if !sel.include_buildings {
        Vec::new()
    } else {
        acc.buildings
            .iter()
            .filter(|b| b.rings.first().map(|r| poly_keep(r)).unwrap_or(false))
            .cloned()
            .collect()
    };
    println!(
        "zone-clipped: {} water, {} waterways, {} landcover, {} buildings, {} rails",
        water.len(),
        waterways.len(),
        landcover.len(),
        buildings.len(),
        rails.len()
    );

    // ── carve terrain ────────────────────────────────────────────────────
    carve(&mut grid, &m, &water, &waterways, mode, span_m);
    if mode == Mode::City {
        // mapmap-scene-style heights: keep the land's measured relief
        // (gently exaggerated so a city's ~60 m hills still read over 100 km)
        // instead of hard-flattening everything to 0 — the flatten hid the
        // terrain the DEM actually sampled. Below-sea cells stay negative
        // (the carve owns the coast); the water plane at 0 m renders the sea.
        for v in grid.data.iter_mut() {
            if *v > 0.0 {
                *v *= TERRAIN_EXAGGERATION;
            }
        }
    } else {
        for v in grid.data.iter_mut() {
            *v = if *v > 0.0 { (*v - base) * vscale } else { -1.0 };
            *v = v.clamp(-35.0, 2_000.0);
        }
    }

    // ── zone mask: the polygon, not the square DEM window, defines the land.
    // Every cell outside the zone rings is carved to open sea (−30 m), so the
    // collection boots shaped like the actual region — the engine's global
    // water plane turns the mask into ocean for free.
    if !spec.rings.is_empty() {
        let mask = zone_mask(&grid, &m, span_m, &spec.rings);
        let land = mask.iter().filter(|m| **m).count();
        for (i, v) in grid.data.iter_mut().enumerate() {
            if !mask[i] {
                *v = -30.0;
            }
        }
        println!(
            "zone mask: {land}/{} cells inside the polygon ({}% of the window) — rest carved to sea",
            grid.w * grid.h,
            100 * land / (grid.w * grid.h).max(1)
        );
    }

    let gmin = grid.data.iter().cloned().fold(f32::MAX, f32::min);
    let gmax = grid.data.iter().cloned().fold(f32::MIN, f32::max);
    println!(
        "terrain baked: {}×{} cells @ {:.1} m, {:.1} – {:.1} m",
        grid.w, grid.h, grid.cell, gmin, gmax
    );

    // ── roads → joined graph ─────────────────────────────────────────────
    let joined = join_roads(&acc, &m, span_m, &spec.rings, &bbox);

    // ── pack the bundle ──────────────────────────────────────────────────
    let mut meta = BundleMeta {
        name: spec.name.clone(),
        kind: spec.kind.clone(),
        district: spec.district.clone(),
        region: spec.region.clone(),
        zone_source: spec.source.clone(),
        zone_adm: spec.adm,
        zone_query: spec.name.clone(),
        center_lat: centre_lat,
        center_lon: centre_lon,
        span_m,
        mode: if mode == Mode::Mountain { "mountain".into() } else { "city".into() },
        vscale,
        terrain_size,
        n_inters: joined.inters.len(),
        n_roads: joined.edges.len(),
        n_buildings: buildings.len(),
        n_rivers: waterways.len(),
        generator: crate::VERSION.to_string(),
        created: String::new(),
    };

    let mut b = BundleBuilder::new(meta.clone());
    b.add_layer(
        crate::bundle::LAYER_TERRAIN,
        "terrain",
        "tzd1",
        terrain::save_bin_bytes(&grid, if mode == Mode::Mountain { 1 } else { 0 }),
    );
    b.add_layer(crate::bundle::LAYER_ROADS, "road-graph", "ego-map-txt-v2", write_map_txt(&joined).into_bytes());
    b.add_layer(
        crate::bundle::LAYER_RIVERS,
        "waterways",
        "ego-lines-txt-v1",
        write_lines(&waterways).into_bytes(),
    );
    b.add_layer(
        crate::bundle::LAYER_WATER,
        "water-polygons",
        "ego-rings-txt-v1",
        write_polys(
            &water
                .iter()
                .map(|w| (w.rings.clone(), w.class.clone()))
                .collect::<Vec<_>>(),
        )
        .into_bytes(),
    );
    b.add_layer(
        crate::bundle::LAYER_LANDCOVER,
        "landcover",
        "ego-rings-txt-v1",
        write_landcover(&landcover).into_bytes(),
    );
    b.add_layer(
        crate::bundle::LAYER_BUILDINGS,
        "buildings",
        "ego-buildings-txt",
        write_buildings(&buildings).into_bytes(),
    );
    // ── boundary layer: the zone rings plus, when the tanzania-pack is
    // available, every district (ADM2) and ward (ADM3) whose centroid falls
    // inside the zone — tagged with `boundary` / `district` / `ward` classes
    // so the engine can layer them
    let mut boundary_polys: Vec<(Vec<Vec<(f64, f64)>>, String)> = spec
        .rings
        .iter()
        .map(|r| (vec![r.clone()], "boundary".into()))
        .collect();
    let (mut n_districts, mut n_wards) = (0usize, 0usize);
    if sel.include_boundary_sub {
        if let Some(packdir) = &opts.packdir {
        if !spec.rings.is_empty() {
            if let Some(admin) = AdminIndex::load(&format!("{packdir}/boundaries")) {
                // a sub-zone belongs to this zone when a solid share of its
                // boundary vertices lies inside — a centroid test fails for
                // coastal districts whose point average falls in the sea
                let inside_frac = |z: &crate::boundaries::Zone| -> f64 {
                    let mut total = 0usize;
                    let mut inside = 0usize;
                    for r in &z.rings {
                        for &(lo, la) in r {
                            total += 1;
                            if spec.rings.iter().any(|rg| point_in_ring(lo, la, rg)) {
                                inside += 1;
                            }
                        }
                    }
                    inside as f64 / total.max(1) as f64
                };
                for z in admin.zones.iter().filter(|z| z.adm == 2 && inside_frac(z) >= 0.3) {
                    for r in &z.rings {
                        boundary_polys.push((vec![r.clone()], "district".into()));
                    }
                    n_districts += 1;
                }
                for z in admin.zones.iter().filter(|z| z.adm == 3 && inside_frac(z) >= 0.3) {
                    for r in &z.rings {
                        boundary_polys.push((vec![r.clone()], "ward".into()));
                    }
                    n_wards += 1;
                }
            }
        }
        }
    }
    println!(
        "boundary layer: {} zone ring(s) + {n_districts} district(s) + {n_wards} ward(s)",
        spec.rings.len()
    );
    b.add_layer(
        crate::bundle::LAYER_BOUNDARY,
        "boundary",
        "ego-rings-txt-v1",
        write_polys(&boundary_polys).into_bytes(),
    );
    b.add_layer(
        crate::bundle::LAYER_RAILS,
        "railways",
        "ego-lines-txt-v1",
        write_lines(&rails).into_bytes(),
    );

    let mut report = String::new();
    report.push_str(&format!(
        "mapkit grab — {}\nspan {:.1} km · DEM z{} · vector z{}\n",
        spec.name, span_m / 1000.0, dem_z, vec_z
    ));
    report.push_str(&format!(
        "roads {} segs → {} edges, {} intersections\n",
        n_road_segs,
        joined.edges.len(),
        joined.inters.len()
    ));
    report.push_str(&format!(
        "buildings {} · lakes {} · rivers {} · landcover {} · rails {} segs\n",
        acc.buildings.len(),
        acc.water.len(),
        acc.waterways.len(),
        acc.landcover.len(),
        n_rail_segs
    ));
    let mut hist = [0usize; 5];
    for e in &joined.edges {
        let i = match e.surface.as_str() {
            "asphalt" | "paved_smooth" | "paved" => 0,
            "concrete" | "cement" => 1,
            "gravel" | "compacted" | "graded" => 2,
            "unpaved" | "dirt" | "ground" | "sand" | "mud" => 3,
            _ => 4,
        };
        hist[i] += 1;
    }
    report.push_str(&format!(
        "surface histogram: {hist:?} [asphalt, cement, gravel, unpaved, other]\n"
    ));
    report.push_str(&format!(
        "dangling ends after joining: {}\n",
        joined.danglers.len()
    ));

    // feed the counts back into the manifest
    meta.created = now_iso();
    let mut bundle = b.finish();
    bundle.meta = meta;
    let _ = report;

    Ok(bundle)
}

fn now_iso() -> String {
    // no chrono dep: seconds-resolution unix time is enough for provenance
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix-{t}")
}

fn tile_window_raw(lat: f64, lon: f64, z: u32, span_m: f64) -> (u32, u32, u32, u32) {
    let (cx, cy) = lonlat_to_tile(lon, lat, z);
    let tspan = tile_span_m(lat, z);
    let half = ((span_m / 2.0 / tspan).ceil() as u32).max(1);
    let n = 1u32 << z;
    (
        cx.saturating_sub(half),
        cy.saturating_sub(half),
        (cx + half + 1).min(n),
        (cy + half + 1).min(n),
    )
}

fn ensure_tiles(cache: &str, z: u32, x0: u32, y0: u32, x1: u32, y1: u32, dem: bool) {
    let mut list = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            list.push((x, y));
        }
    }
    ensure_tile_list(cache, z, &list, dem);
}

/// download every missing tile of the list (skips what's already cached)
fn ensure_tile_list(cache: &str, z: u32, list: &[(u32, u32)], dem: bool) {
    let mut missing = vec![];
    for (x, y) in list {
        let p = if dem {
            format!("{}/dem_z{}_{}_{}.png", cache, z, x, y)
        } else {
            format!("{}/v8_z{}_{}_{}.mvt", cache, z, x, y)
        };
        if !std::path::Path::new(&p).exists() {
            missing.push((*x, *y, p));
        }
    }
    if missing.is_empty() {
        return;
    }
    println!(
        "fetching {} {} tiles @z{}…",
        missing.len(),
        if dem { "terrain" } else { "vector" },
        z
    );
    let keys = tokens();
    if keys.is_empty() {
        eprintln!("{}. {} tile(s) skipped — nothing cached.", missing_token_hint(), missing.len());
        return;
    }
    for (x, y, p) in &missing {
        let base = if dem {
            format!("https://api.mapbox.com/v4/mapbox.terrain-rgb/{}/{}/{}.png", z, x, y)
        } else {
            format!("https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{}/{}/{}.mvt", z, x, y)
        };
        for attempt in 0..3 {
            // rotate across both keys (attempts double as key fallback);
            // the secondary key is URL-restricted and needs its referer
            let (tok, referer) = &keys[attempt % keys.len()];
            let url = format!("{base}?access_token={tok}");
            // capture stdout and write the file ourselves — some sandboxes
            // block the child's own file writes (curl exit 23)
            let mut cmd = std::process::Command::new("curl");
            cmd.args(["-sS", "--max-time", "30", &url]);
            if let Some(r) = referer {
                cmd.args(["-H", &format!("Referer: {r}")]);
            }
            let st = cmd.output();
            match st {
                Ok(o)
                    if o.status.success()
                        && !o.stdout.is_empty()
                        && !o.stdout.starts_with(b"{") =>
                {
                    match std::fs::write(p, &o.stdout) {
                        Ok(()) => break,
                        Err(e) => {
                            eprintln!("fs::write {} failed: {}", p, e);
                        }
                    }
                }
                Ok(o) => {
                    if attempt == 2 {
                        eprintln!(
                            "curl bad response for {} (status {:?}, {} bytes): {}",
                            url,
                            o.status.code(),
                            o.stdout.len(),
                            String::from_utf8_lossy(&o.stderr)
                        );
                    }
                }
                Err(e) => {
                    if attempt == 2 {
                        eprintln!("curl spawn failed for {}: {}", url, e);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }
}

// ── terrain carving ──────────────────────────────────────────────────────

fn carve(
    g: &mut terrain::Grid,
    m: &Mercator,
    water: &[mvt::PolyF],
    waterways: &[(Vec<(f64, f64)>, String)],
    mode: Mode,
    span_m: f64,
) {
    let w = g.w;
    let h = g.h;
    let mut water_kind = vec![0u8; w * h]; // 0 none, 1 dem-sea, 2 lake, 3 ocean, 4 river

    let cell = g.cell;
    let cell_of = move |lon: f64, lat: f64| -> (f32, f32) {
        let (x, y) = m.project(lon, lat);
        (
            (x + span_m / 2.0) as f32 / cell,
            (y + span_m / 2.0) as f32 / cell,
        )
    };

    for p in water {
        let deep = p.class == "ocean";
        for ring in &p.rings {
            raster_ring(ring, w, h, &cell_of, |c, r| {
                let i = r * w + c;
                if i < water_kind.len() {
                    water_kind[i] = water_kind[i].max(if deep { 3 } else { 2 });
                }
            });
        }
    }
    for (pts, cls) in waterways {
        let halfw = match cls.as_str() {
            "river" | "canal" => 7.0f32,
            _ => 3.5,
        };
        let rw = (halfw / g.cell).max(1.0);
        for seg in pts.windows(2) {
            let (x0, y0) = cell_of(seg[0].0, seg[0].1);
            let (x1, y1) = cell_of(seg[1].0, seg[1].1);
            thick_line(x0, y0, x1, y1, rw, w, h, &mut water_kind, 4);
        }
    }
    // DEM-sea: only genuinely below-sea-level cells. Nodata can no longer
    // reach this point (uniform-0 tiles are NaN → gap-filled → explicit sea
    // depth upstream), so the old < 0.5 rule only has to catch real sea —
    // and must NOT pit real 0–0.5 m beach on a coastal plain like Dar's.
    // mapmap-scene convention kept: the sea floor deepens with the reading.
    if mode == Mode::City {
        for r in 0..h {
            for c in 0..w {
                let i = r * w + c;
                if water_kind[i] == 0 && g.data[i] < -0.2 {
                    water_kind[i] = 1;
                }
            }
        }
    }

    for i in 0..w * h {
        let mtr = g.data[i];
        g.data[i] = match water_kind[i] {
            3 => -30.0,
            2 => -2.5,
            4 => -1.4,
            1 => -(3.0 + (0.0 - mtr).max(0.0) * 10.0).min(28.0),
            _ => mtr,
        };
    }
}

/// split a polyline into runs of consecutive points passing `keep` — lines
/// crossing the zone border are cut at it instead of overdrawn outside
fn clip_line_runs(
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

/// per-cell inside-the-zone mask over the DEM grid, rasterized with the
/// same scanline fill the water carving uses (grid cell centres sample
/// the polygon; y rows run south→north exactly like the resampled DEM)
fn zone_mask(
    g: &terrain::Grid,
    m: &Mercator,
    span_m: f64,
    rings: &[Vec<(f64, f64)>],
) -> Vec<bool> {
    let (w, h) = (g.w, g.h);
    let mut mask = vec![false; w * h];
    let cell_of = |lon: f64, lat: f64| -> (f32, f32) {
        let (x, y) = m.project(lon, lat);
        (
            (x + span_m / 2.0) as f32 / g.cell,
            (y + span_m / 2.0) as f32 / g.cell,
        )
    };
    for ring in rings {
        raster_ring(ring, w, h, &cell_of, |c, r| mask[r * w + c] = true);
    }
    mask
}

fn raster_ring(
    ring: &[(f64, f64)],
    w: usize,
    h: usize,
    cell_of: &dyn Fn(f64, f64) -> (f32, f32),
    mut put: impl FnMut(usize, usize),
) {
    let pts: Vec<(f32, f32)> = ring.iter().map(|&(lo, la)| cell_of(lo, la)).collect();
    if pts.len() < 3 {
        return;
    }
    let miny = (pts.iter().fold(f32::MAX, |a, b| a.min(b.1)).max(0.0)) as i64;
    let maxy = (pts.iter().fold(f32::MIN, |a, b| a.max(b.1)).min(h as f32)) as i64;
    for y in miny..maxy {
        let yc = y as f32 + 0.5;
        let mut xs: Vec<f32> = vec![];
        let n = pts.len();
        for i in 0..n {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[(i + 1) % n];
            if (y0 <= yc && y1 > yc) || (y1 <= yc && y0 > yc) {
                let t = (yc - y0) / (y1 - y0 + 1e-9);
                xs.push(x0 + t * (x1 - x0));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for pair in xs.chunks(2) {
            if pair.len() == 2 {
                let a = (pair[0].ceil().max(0.0)) as i64;
                let b = (pair[1].floor().min(w as f32)) as i64;
                for x in a..b {
                    if x >= 0 {
                        put(x as usize, y as usize);
                    }
                }
            }
        }
    }
}

fn thick_line(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    rw: f32,
    w: usize,
    h: usize,
    grid: &mut [u8],
    val: u8,
) {
    let steps = ((x1 - x0).abs().max((y1 - y0).abs()).ceil() as usize).max(1);
    let rw = rw.max(1.0);
    for s in 0..=steps {
        let t = s as f32 / steps as f32;
        let x = x0 + t * (x1 - x0);
        let y = y0 + t * (y1 - y0);
        for dy in -(rw.ceil() as i32)..=(rw.ceil() as i32) {
            for dx in -(rw.ceil() as i32)..=(rw.ceil() as i32) {
                if (dx * dx + dy * dy) as f32 <= rw * rw {
                    let cx = x as i32 + dx;
                    let cy = y as i32 + dy;
                    if cx >= 0 && cy >= 0 && (cx as usize) < w && (cy as usize) < h {
                        let i = cy as usize * w + cx as usize;
                        if grid[i] < val {
                            grid[i] = val;
                        }
                    }
                }
            }
        }
    }
}

// ── road joining ─────────────────────────────────────────────────────────

struct Inter {
    lon: f64,
    lat: f64,
    x: f64,
    y: f64,
}

struct Edge {
    a: usize,
    b: usize,
    pts: Vec<(f64, f64)>,
    lanes: u8,
    oneway: bool,
    speed: u8,
    surface: String,
    /// streets-v8 road class, kept so the engine can size roads per type
    class: String,
    /// primary route reference, e.g. "T1" ("" when untagged)
    road_ref: String,
}

/// Primary reference of a possibly concurrent ref tag ("T1;T23" → "T1").
/// Whitespace is flattened to '_' so the value stays a single token in
/// the whitespace-separated bundle text formats.
fn primary_ref(road_ref: &str) -> String {
    road_ref
        .split(';')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_")
}

struct Joined {
    inters: Vec<Inter>,
    edges: Vec<Edge>,
    danglers: Vec<(f64, f64)>,
}

fn class_params(class: &str) -> (u8, bool, u8) {
    match class {
        "motorway" => (2, true, 27),
        "motorway_link" => (1, true, 18),
        "trunk" => (2, false, 22),
        "trunk_link" => (1, true, 14),
        "primary" => (1, false, 18),
        "secondary" => (1, false, 15),
        "tertiary" => (1, false, 12),
        "residential" | "unclassified" | "minor" | "street" | "street_limited" => (1, false, 9),
        "service" | "track" => (1, false, 7),
        _ => (1, false, 9),
    }
}

/// detail rank of a road class (higher = more important)
fn class_rank(class: &str) -> u8 {
    match class {
        "motorway" | "trunk" => 9,
        "motorway_link" | "trunk_link" => 8,
        "primary" => 7,
        "secondary" => 6,
        "tertiary" => 5,
        "residential" | "unclassified" | "minor" | "street" | "street_limited" => 3,
        "living_street" => 2,
        "service" | "track" => 1,
        _ => 0,
    }
}

fn join_roads(
    td: &TileData,
    m: &Mercator,
    span_m: f64,
    zone_rings: &[Vec<(f64, f64)>],
    bbox: &(f64, f64, f64, f64),
) -> Joined {
    let min_class = MIN_CLASS.with(|c| c.borrow().clone());
    let road_allow = selection().road_allow;
    // keep roads that touch the zone polygon (or ≤1 km of it) — stops
    // neighbouring-city sprawl from exploding the graph. Multi-polygon
    // zones (islands, exclaves) test every ring.
    let dbuf = 0.01; // ≈1 km in degrees
    let keep = |lon: f64, lat: f64| -> bool {
        if lon < bbox.0 - dbuf || lon > bbox.2 + dbuf || lat < bbox.1 - dbuf || lat > bbox.3 + dbuf
        {
            return false;
        }
        zone_rings.is_empty() || zone_rings.iter().any(|r| point_in_ring(lon, lat, r))
    };
    let half = span_m / 2.0 + 500.0;
    // everything must also sit inside the terrain window so no feature
    // ends up floating beyond the generated terrain
    let win = span_m / 2.0 + 1_200.0;
    let in_window = |lon: f64, lat: f64| -> bool {
        let (x, y) = m.project(lon, lat);
        x.abs() < win && y.abs() < win
    };
    let mut class_hist: HashMap<&str, usize> = HashMap::new();
    let mut inters: Vec<Inter> = vec![];
    let mut grid: HashMap<(i64, i64), usize> = HashMap::new();
    let mut edges: Vec<Edge> = vec![];
    let mut seen: HashSet<(usize, usize)> = HashSet::new();

    for r in &td.roads {
        if r.rail || r.pts.len() < 2 {
            continue;
        }
        *class_hist.entry(r.class.as_str()).or_default() += 1;
        if matches!(
            r.class.as_str(),
            "path" | "sidewalk" | "steps" | "pedestrian"
        ) {
            continue;
        }
        // optional detail floor: "service" drops driveways/parking aisles
        if let Some(min_class) = &min_class {
            let rank = class_rank(&r.class);
            let floor = match min_class.as_str() {
                "service" => 2,
                "residential" => 4,
                _ => 0,
            };
            if rank < floor {
                continue;
            }
        }
        // selectable preview allow-list (per-class checkboxes in the web UI)
        if !GrabSelection::allows(&road_allow, &r.class) {
            continue;
        }
        if !r.pts.iter().any(|&(lo, la)| keep(lo, la) && in_window(lo, la)) {
            continue;
        }
        let mut nodes: Vec<usize> = vec![];
        for &(lon, lat) in &r.pts {
            let (x, y) = m.project(lon, lat);
            let key = ((x / 12.0).round() as i64, (y / 12.0).round() as i64);
            let mut found = grid.get(&key).copied();
            if found.is_none() {
                for (dx, dy) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                    if let Some(&id) = grid.get(&(key.0 + dx, key.1 + dy)) {
                        let it = &inters[id];
                        if (it.x - x).powi(2) + (it.y - y).powi(2) < 144.0 {
                            found = Some(id);
                            break;
                        }
                    }
                }
            }
            let id = match found {
                Some(id) => id,
                None => {
                    let id = inters.len();
                    inters.push(Inter { lon, lat, x, y });
                    grid.insert(key, id);
                    id
                }
            };
            nodes.push(id);
        }
        // dedupe consecutive same nodes
        nodes.dedup();
        if nodes.len() < 2 {
            continue;
        }
        let (lanes, oneway, speed) = class_params(&r.class);
        let road_ref = primary_ref(&r.road_ref);
        // chain: consecutive nodes become edges. Every original vertex was
        // pushed as (or snapped onto) a node above, so consecutive nodes are
        // ADJACENT vertices — there are no interior points left to carry.
        // The old suffix-slice `r.pts[w+1..len-1]` dragged the whole
        // downstream tail of the road behind EVERY edge (kilometres of
        // scribble on long z13 features); each road is now individual:
        // its shape comes from the chain of straight node-to-node edges.
        // Edges with BOTH ends outside the zone are neighbouring-region
        // sprawl (a through-road kept for one Dar-side vertex) — dropped so
        // nothing draws over the carved-out sea.
        let node_inside = |i: usize| -> bool {
            zone_rings.is_empty() || {
                let it = &inters[i];
                zone_rings.iter().any(|r| point_in_ring(it.lon, it.lat, r))
            }
        };
        for w in 0..nodes.len() - 1 {
            let (a, b) = (nodes[w], nodes[w + 1]);
            if a == b {
                continue;
            }
            if !node_inside(a) && !node_inside(b) {
                continue;
            }
            let key = (a.min(b), a.max(b));
            if !seen.insert(key) {
                continue;
            }
            edges.push(Edge {
                a: key.0,
                b: key.1,
                pts: vec![],
                lanes,
                oneway,
                speed,
                surface: r.surface.clone(),
                class: r.class.clone(),
                road_ref: road_ref.clone(),
            });
        }
    }

    let mut classes: Vec<_> = class_hist.into_iter().collect();
    classes.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("road classes: {:?}", classes);

    // reconnect + free-intersection passes
    let mut danglers: Vec<(f64, f64)> = vec![];
    for pass in 0..2 {
        let deg = degree(&edges, inters.len());
        let mut to_connect: Vec<usize> = (0..inters.len()).filter(|&i| deg[i] == 1).collect();
        to_connect.retain(|&i| {
            let it = &inters[i];
            it.x.abs() < half - 300.0 && it.y.abs() < half - 300.0
        });
        let mut recons = 0;
        let mut splits = 0;
        for &i in &to_connect {
            let p = (inters[i].x, inters[i].y);

            // 1) free-intersection: project onto nearby edge, split it there
            if let Some((ei, s_pos, foot)) = project_onto_edges(&edges, &inters, p, 25.0, i, m) {
                let newid = inters.len();
                let (fx, fy) = m.project(foot.0, foot.1);
                inters.push(Inter {
                    lon: foot.0,
                    lat: foot.1,
                    x: fx,
                    y: fy,
                });
                let (lanes, oneway, speed, surface, class, road_ref) = (
                    edges[ei].lanes,
                    edges[ei].oneway,
                    edges[ei].speed,
                    edges[ei].surface.clone(),
                    edges[ei].class.clone(),
                    edges[ei].road_ref.clone(),
                );
                let (a, b) = (edges[ei].a, edges[ei].b);
                let pts = std::mem::take(&mut edges[ei].pts);
                let k = s_pos.floor() as usize;
                let t = s_pos - k as f32;
                // full polyline: [a] + pts + [b]; split at full-position s_pos
                let left: Vec<(f64, f64)> = pts[..k.min(pts.len())].to_vec();
                let right: Vec<(f64, f64)> = pts[k.min(pts.len())..].to_vec();
                let mut left = left;
                if t > 0.01 {
                    left.push(foot);
                }
                let mut right2 = vec![foot];
                right2.extend_from_slice(&right);
                edges[ei] = Edge {
                    a,
                    b: newid,
                    pts: left,
                    lanes,
                    oneway,
                    speed,
                    surface: surface.clone(),
                    class: class.clone(),
                    road_ref: road_ref.clone(),
                };
                edges.push(Edge {
                    a: newid,
                    b,
                    pts: right2,
                    lanes,
                    oneway,
                    speed,
                    surface: surface.clone(),
                    class: class.clone(),
                    road_ref: road_ref.clone(),
                });
                edges.push(Edge {
                    a: i.min(newid),
                    b: i.max(newid),
                    pts: vec![],
                    lanes: 1,
                    oneway: false,
                    speed: 9,
                    surface: "unpaved".into(),
                    class: "service".into(),
                    road_ref: String::new(),
                });
                splits += 1;
                continue;
            }

            // 2) connect two dangling ends within 40 m
            if let Some(j) = nearest_dangler(&inters, &edges, i, 40.0) {
                edges.push(Edge {
                    a: i.min(j),
                    b: i.max(j),
                    pts: vec![],
                    lanes: 1,
                    oneway: false,
                    speed: 9,
                    surface: "unpaved".into(),
                    class: "service".into(),
                    road_ref: String::new(),
                });
                recons += 1;
            }
        }
        println!(
            "reconnect pass {}: {} dangling ends reconnected · free-intersection pass: {} T-junctions split",
            pass + 1,
            recons,
            splits
        );
    }

    let mut ref_hist: HashMap<&str, usize> = HashMap::new();
    for e in &edges {
        if !e.road_ref.is_empty() {
            *ref_hist.entry(e.road_ref.as_str()).or_default() += 1;
        }
    }
    let mut refs: Vec<_> = ref_hist.into_iter().collect();
    refs.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("route refs: {} edges tagged, top {:?}", edges.iter().filter(|e| !e.road_ref.is_empty()).count(), &refs[..refs.len().min(10)]);

    let deg = degree(&edges, inters.len());
    for (i, it) in inters.iter().enumerate() {
        if deg[i] == 1 {
            danglers.push((it.lon, it.lat));
        }
    }

    Joined {
        inters,
        edges,
        danglers,
    }
}

fn degree(edges: &[Edge], n: usize) -> Vec<u32> {
    let mut d = vec![0u32; n];
    for e in edges {
        d[e.a] += 1;
        d[e.b] += 1;
    }
    d
}

/// closest point on any edge (skipping edges touching skip_node) within max_d;
/// returns (edge idx, split position s along full polyline [a]+pts+[b], foot lon/lat)
fn project_onto_edges(
    edges: &[Edge],
    inters: &[Inter],
    p: (f64, f64),
    max_d: f64,
    skip_node: usize,
    m: &Mercator,
) -> Option<(usize, f32, (f64, f64))> {
    let mut best: Option<(usize, f32, (f64, f64), f64)> = None;
    for (ei, e) in edges.iter().enumerate() {
        if e.a == skip_node || e.b == skip_node {
            continue;
        }
        let mut full: Vec<(f64, f64)> = Vec::with_capacity(e.pts.len() + 2);
        full.push((inters[e.a].x, inters[e.a].y));
        for &(lo, la) in &e.pts {
            full.push(m.project(lo, la));
        }
        full.push((inters[e.b].x, inters[e.b].y));
        for (k, w) in full.windows(2).enumerate() {
            let (ax, ay) = w[0];
            let (bx, by) = w[1];
            let abx = bx - ax;
            let aby = by - ay;
            let l2 = abx * abx + aby * aby;
            if l2 < 1.0 {
                continue;
            }
            let t = ((p.0 - ax) * abx + (p.1 - ay) * aby) / l2;
            if !(0.02..=0.98).contains(&t) {
                continue;
            }
            let fx = ax + t * abx;
            let fy = ay + t * aby;
            let d2 = (p.0 - fx).powi(2) + (p.1 - fy).powi(2);
            if d2 < max_d * max_d && best.as_ref().map(|b| d2 < b.3).unwrap_or(true) {
                let (flon, flat) = m.unproject(fx, fy);
                best = Some((ei, k as f32 + t as f32, (flon, flat), d2));
            }
        }
    }
    best.map(|(ei, s, foot, _)| (ei, s, foot))
}

fn nearest_dangler(inters: &[Inter], edges: &[Edge], i: usize, max_d: f64) -> Option<usize> {
    let deg = degree(edges, inters.len());
    let p = &inters[i];
    let mut best = None;
    let mut bd = max_d * max_d;
    for (j, q) in inters.iter().enumerate() {
        if j == i || deg[j] != 1 {
            continue;
        }
        let d2 = (p.x - q.x).powi(2) + (p.y - q.y).powi(2);
        if d2 < bd {
            bd = d2;
            best = Some(j);
        }
    }
    best
}

// ── writers (bundle blob text formats) ───────────────────────────────────

/// roads layer, format `ego-map-txt-v2`: v1 edge columns plus trailing
/// `class ref` tokens (`-` when empty) so the engine can size roads per
/// type and draw route shields. Both tokens are whitespace-free.
fn write_map_txt(j: &Joined) -> String {
    let mut s = String::with_capacity(64 + (j.inters.len() + j.edges.len()) * 48);
    s.push_str(&format!("{} {}\n", j.inters.len(), j.edges.len()));
    for it in &j.inters {
        s.push_str(&format!("{:.7} {:.7}\n", it.lat, it.lon));
    }
    for e in &j.edges {
        s.push_str(&format!(
            "{} {} {} {} {} {} {} {}",
            e.a,
            e.b,
            e.lanes,
            if e.oneway { 1 } else { 0 },
            e.speed,
            e.pts.len(),
            if e.class.is_empty() { "-" } else { &e.class },
            if e.road_ref.is_empty() { "-" } else { &e.road_ref },
        ));
        for &(lo, la) in &e.pts {
            s.push_str(&format!(" {:.7} {:.7}", la, lo));
        }
        s.push('\n');
    }
    s
}

fn write_polys(polys: &[(Vec<Vec<(f64, f64)>>, String)]) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}\n", polys.len()));
    for (rings, cls) in polys {
        s.push_str(&format!("{} {}\n", rings.len(), cls));
        for r in rings {
            s.push_str(&format!("{} ", r.len()));
            for &(lo, la) in r {
                s.push_str(&format!("{:.6} {:.6} ", la, lo));
            }
            s.push('\n');
        }
    }
    s
}

fn write_lines(lines: &[(Vec<(f64, f64)>, String)]) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}\n", lines.len()));
    for (pts, cls) in lines {
        s.push_str(&format!("{} {}", pts.len(), cls));
        for &(lo, la) in pts {
            s.push_str(&format!(" {:.6} {:.6}", la, lo));
        }
        s.push('\n');
    }
    s
}

fn write_buildings(bs: &[mvt::BuildF]) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}\n", bs.len()));
    for b in bs {
        let ring = &b.rings[0];
        let n = ring.len().saturating_sub(1);
        s.push_str(&format!("{:.1} {:.1} {}\n", b.height, b.min_h, n));
        for &(lo, la) in &ring[..n] {
            s.push_str(&format!("{:.6} {:.6} ", la, lo));
        }
        s.push('\n');
    }
    s
}

fn write_landcover(lc: &[mvt::PolyF]) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}\n", lc.len()));
    for p in lc {
        for r in &p.rings {
            s.push_str(&format!("{} {}\n", r.len(), p.class));
            for &(lo, la) in r {
                s.push_str(&format!("{:.6} {:.6} ", la, lo));
            }
            s.push('\n');
        }
    }
    s
}
