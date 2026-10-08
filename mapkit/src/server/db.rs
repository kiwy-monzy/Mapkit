//! SQLite access (Diesel 2) — zone registry + tile index.
//!
//! The registry is the server's source of truth for "which zones exist".
//! On boot it is synced against `assets/collections/*.mapbin`: every bundle
//! on disk is probed (header-only) and upserted, so a bundle dropped by the
//! mapkit-cli (or by hand) is registered automatically.

use std::path::Path;

use diesel::prelude::*;
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

use super::models::{Zone, ZoneUpsert};
use super::schema::{tiles, zones};

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations/sqlite");

pub fn open(path: &Path) -> SqliteConnection {
    use diesel::Connection;
    SqliteConnection::establish(path.to_str().unwrap_or("mapkit.db"))
        .unwrap_or_else(|e| panic!("open sqlite {}: {e}", path.display()))
}

pub fn run_migrations(conn: &mut SqliteConnection) {
    conn.run_pending_migrations(MIGRATIONS)
        .unwrap_or_else(|e| panic!("sqlite migrations failed: {e}"));
}

fn now() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix-{t}")
}

/// Upsert a zone keyed by its slug (`ON CONFLICT (slug) DO UPDATE`).
pub fn upsert_zone(conn: &mut SqliteConnection, up: &ZoneUpsert) {
    let ts = now();
    diesel::insert_into(zones::table)
        .values(up)
        .on_conflict(zones::slug)
        .do_update()
        .set((
            zones::name.eq(&up.name),
            zones::kind.eq(&up.kind),
            zones::district.eq(&up.district),
            zones::region.eq(&up.region),
            zones::center_lat.eq(up.center_lat),
            zones::center_lon.eq(up.center_lon),
            zones::span_m.eq(up.span_m),
            zones::mode.eq(&up.mode),
            zones::vscale.eq(up.vscale),
            zones::terrain_size.eq(up.terrain_size),
            zones::n_inters.eq(up.n_inters),
            zones::n_roads.eq(up.n_roads),
            zones::n_buildings.eq(up.n_buildings),
            zones::n_rivers.eq(up.n_rivers),
            zones::bytes.eq(up.bytes),
            zones::status.eq(&up.status),
            zones::updated_at.eq(&ts),
        ))
        .execute(conn)
        .unwrap_or_else(|e| {
            eprintln!("[SERVER] zone upsert failed: {e}");
            0
        });
}

pub fn list_zones(conn: &mut SqliteConnection) -> Vec<Zone> {
    zones::table
        .order(zones::slug)
        .load(conn)
        .unwrap_or_default()
}

pub fn count_zones(conn: &mut SqliteConnection) -> i64 {
    zones::table
        .count()
        .get_result(conn)
        .unwrap_or(0)
}

pub fn count_tiles(conn: &mut SqliteConnection) -> i64 {
    tiles::table
        .count()
        .get_result(conn)
        .unwrap_or(0)
}

/// Record a freshly fetched/cached tile (best-effort; the disk is the real
/// cache, this index only powers /health stats).
pub fn record_tile(conn: &mut SqliteConnection, z: u32, x: u32, y: u32, source: &str, bytes: usize) {
    diesel::insert_into(tiles::table)
        .values((
            tiles::z.eq(z as i32),
            tiles::x.eq(x as i32),
            tiles::y.eq(y as i32),
            tiles::source.eq(source),
            tiles::bytes.eq(bytes as i32),
            tiles::fetched_at.eq(now()),
        ))
        .on_conflict_do_nothing()
        .execute(conn)
        .ok();
}

/// Sync the registry with every `.mapbin` in `dir` — header-only probe,
/// same parser the engine and mapkit-cli use (mapkit::bundle::Bundle).
/// Rows whose bundle no longer exists on disk are pruned, so the registry
/// (and /health + /api/collections counts) can never go stale.
/// Returns the number of registered zones.
pub fn sync_collections_dir(conn: &mut SqliteConnection, dir: &Path) -> usize {
    let mut zones_seen: Vec<ZoneUpsert> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut paths: Vec<_> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("mapbin"))
            .collect();
        paths.sort();
        for p in paths {
            let Ok(m) = crate::bundle::Bundle::probe_file(&p) else {
                eprintln!("[SERVER] skipping unparseable bundle {}", p.display());
                continue;
            };
            let slug = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if slug.is_empty() {
                continue;
            }
            zones_seen.push(ZoneUpsert {
                slug,
                name: m.name,
                kind: m.kind,
                district: m.district,
                region: m.region,
                center_lat: m.center_lat,
                center_lon: m.center_lon,
                span_m: m.span_m,
                mode: m.mode,
                vscale: m.vscale as f64,
                terrain_size: m.terrain_size as i32,
                n_inters: m.n_inters as i32,
                n_roads: m.n_roads as i32,
                n_buildings: m.n_buildings as i32,
                n_rivers: m.n_rivers as i32,
                bytes: std::fs::metadata(&p).map(|md| md.len() as i32).unwrap_or(0),
                status: "ready".into(),
            });
        }
    }
    for z in &zones_seen {
        upsert_zone(conn, z);
    }
    // prune rows whose bundle was deleted from disk (otherwise the UI
    // shows 1 collection while the API reports N)
    let live: std::collections::HashSet<&str> =
        zones_seen.iter().map(|z| z.slug.as_str()).collect();
    let stale: Vec<String> = zones::table
        .select(zones::slug)
        .load::<String>(conn)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !live.contains(s.as_str()))
        .collect();
    for s in &stale {
        eprintln!("[SERVER] pruning stale zone '{s}' (bundle gone from disk)");
        diesel::delete(zones::table.filter(zones::slug.eq(s)))
            .execute(conn)
            .ok();
    }
    count_zones(conn) as usize
}
