//! mapkit — the map data kit for the Egregoria map editor.
//!
//! ONE crate, three roles (the standalone mapstudio-rs exporter and the
//! mapkit-server service were both folded in here):
//!
//! 1. **mapstudio pipeline** — boundaries → zone polygon → Mapbox tiles
//!    (vector streets-v8 + terrain-RGB DEM) → THE one Mercator → joined
//!    road graph + water-carved DEM.
//! 2. **the kit** — `.mapbin` bundle packing + the layer-source registry
//!    the engine's editor consumes.
//! 3. **the server** — [`server`]: a Rocket + Diesel HTTP data plane that
//!    streams XYZ tiles (basemap / vector / terrain) out of the shared
//!    `tilecache/`, owns the zone registry and grabs new zones. The
//!    Egregoria native app boots it INSIDE the game process
//!    (`mapkit::server::spawn_embedded`) — no standalone service needed.

#[macro_use]
extern crate rocket;

pub mod boundaries;
pub mod bundle;
pub mod formats;
pub mod grab;
pub mod layers;
pub mod mercator;
pub mod mvt;
pub mod pbf;
pub mod server;
pub mod style;
pub mod terrain;

pub const VERSION: &str = concat!("mapkit ", env!("CARGO_PKG_VERSION"));
