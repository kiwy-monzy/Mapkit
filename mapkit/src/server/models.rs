//! Diesel row types for the zone registry + tile index.

use diesel::prelude::*;

/// One registered zone = one `.mapbin` collection in `assets/collections/`.
#[derive(Queryable, Selectable, Identifiable, Debug, Clone)]
#[diesel(table_name = crate::server::schema::zones)]
pub struct Zone {
    pub id: i32,
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub district: String,
    pub region: String,
    pub center_lat: f64,
    pub center_lon: f64,
    pub span_m: f64,
    pub mode: String,
    pub vscale: f64,
    pub terrain_size: i32,
    pub n_inters: i32,
    pub n_roads: i32,
    pub n_buildings: i32,
    pub n_rivers: i32,
    pub bytes: i32,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Everything we can write from a bundle manifest probe (no id/timestamps).
#[derive(Insertable, AsChangeset)]
#[diesel(table_name = crate::server::schema::zones)]
pub struct ZoneUpsert {
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub district: String,
    pub region: String,
    pub center_lat: f64,
    pub center_lon: f64,
    pub span_m: f64,
    pub mode: String,
    pub vscale: f64,
    pub terrain_size: i32,
    pub n_inters: i32,
    pub n_roads: i32,
    pub n_buildings: i32,
    pub n_rivers: i32,
    pub bytes: i32,
    pub status: String,
}

/// One cached XYZ tile (raster/vector/terrain — `source` names the kind).
#[derive(Queryable, Debug)]
#[diesel(table_name = crate::server::schema::tiles)]
#[allow(dead_code)]
pub struct Tile {
    pub id: i32,
    pub z: i32,
    pub x: i32,
    pub y: i32,
    pub source: String,
    pub bytes: i32,
    pub fetched_at: String,
}
