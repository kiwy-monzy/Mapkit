//! mapkit::server — the embedded map data plane (formerly the standalone
//! `mapkit-server` crate).
//!
//! A Rocket + Diesel (SQLite) HTTP service that:
//!   * streams XYZ tiles (raster basemap / vector MVT / terrain DEM)
//!     straight out of the shared `tilecache/`, backfilling from Mapbox
//!     (and an Esri fallback) on miss,
//!   * owns the zone registry for every collection in
//!     `assets/collections/*.mapbin` (re-synced from disk on boot and on
//!     every listing),
//!   * grabs new zones by spawning the prebuilt `mapkit-cli`,
//!   * serves /openapi.json + /swagger-ui (only real routes documented).
//!
//! The native app runs this INSIDE the game process — see
//! [`spawn_embedded`]. It can still be run standalone for scripting:
//!
//!   MAPKIT_PORT=5150 cargo run --release -p mapkit --bin mapkit-server-standalone
//!
//! (or simply launch the game: the server comes up with it on port 5150).

mod admin;
mod collections;
mod db;
mod docs;
mod launch;
mod layers;
mod models;
mod prefetch;
mod preview;
mod schema;
mod state;
mod tiles;
mod upstream;

pub use launch::{run_blocking, spawn_embedded};
pub use state::{resolve_root, ServerState};
