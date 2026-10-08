//! Standalone mapkit-server entry point (the server now lives inside the
//! game build via `mapkit::server::spawn_embedded`; this bin exists for
//! headless/scripting use so the game does not have to run).
//!
//! Env (see `.env.example` at the repo root — copy it to `.env`):
//! MAPKIT_PORT (default 5150) · MAPKIT_ROOT · MAPKIT_DB · MAPKIT_WEB_DIST
//! MAPBOX_TOKEN · MAPBOX_TOKEN_2

fn main() {
    // Load `.env` first so MAPBOX_TOKEN / MAPKIT_* resolve identically in
    // CLI, standalone and embedded runs.
    mapkit::grab::load_dotenv();
    let port: u16 = std::env::var("MAPKIT_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5150);
    if let Err(e) = mapkit::server::run_blocking(port) {
        eprintln!("[SERVER] rocket error: {e}");
        std::process::exit(1);
    }
}
