//! Server launch — route table, CORS, health endpoint and the two ways
//! to run the service:
//!
//! * [`spawn_embedded`] — used by the Egregoria native app at boot: starts
//!   the server on a plain background thread INSIDE the game process.
//! * [`run_blocking`] — blocks the calling thread until shutdown; the
//!   standalone entry point (`mapkit-server-standalone` bin) wraps this.

use std::time::Instant;

use rocket::fairing::{Fairing, Info, Kind};
use rocket::fs::FileServer;
use rocket::http::{Header, Status};
use rocket::{Request, Response};

use super::db;
use super::state::{resolve_root, ServerState};

pub struct Cors;

#[rocket::async_trait]
impl Fairing for Cors {
    fn info(&self) -> Info {
        Info {
            name: "CORS headers",
            kind: Kind::Response,
        }
    }
    async fn on_response<'r>(&self, _req: &'r Request<'_>, res: &mut Response<'r>) {
        res.set_header(Header::new("Access-Control-Allow-Origin", "*"));
        res.set_header(Header::new("Access-Control-Allow-Methods", "GET, POST, OPTIONS"));
        res.set_header(Header::new("Access-Control-Allow-Headers", "Content-Type"));
    }
}

/// CORS preflight for every path.
#[options("/<_..>")]
fn preflight() -> Status {
    Status::Ok
}

/// Every route of the map data plane (mounted at "/").
pub fn route_table() -> Vec<rocket::Route> {
    rocket::routes![
        health,
        preflight,
        crate::server::collections::list,
        crate::server::collections::grab,
        crate::server::collections::download,
        crate::server::layers::layers,
        crate::server::prefetch::prefetch,
        crate::server::preview::preview,
        crate::server::admin::overview,
        crate::server::admin::regions,
        crate::server::admin::districts,
        crate::server::admin::wards,
        crate::server::admin::search,
        crate::server::admin::geometry,
        crate::server::tiles::raster,
        crate::server::tiles::vector,
        crate::server::tiles::terrain,
        crate::server::docs::index,
        crate::server::docs::swagger,
        crate::server::docs::openapi,
    ]
}

/// Build (but do not ignite) the Rocket instance for `port`.
fn build_rocket(port: u16) -> rocket::Rocket<rocket::Build> {
    // Embedded mode never passes through a binary `main`, so load `.env`
    // here too (idempotent — the CLI/standalone mains already did it).
    crate::grab::load_dotenv();
    let started = Instant::now();
    let root = resolve_root();
    let collections = root.join("assets/collections");
    let tilecache = root.join("tilecache");
    let db_path = std::env::var("MAPKIT_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| root.join("mapkit.db"));

    eprintln!("[SERVER] mapkit-server v{} (embedded in mapkit)", env!("CARGO_PKG_VERSION"));
    eprintln!("[SERVER] root        = {}", root.display());
    eprintln!("[SERVER] collections = {}", collections.display());
    eprintln!("[SERVER] tilecache   = {}", tilecache.display());
    eprintln!("[SERVER] db          = {}", db_path.display());

    if !collections.is_dir() {
        eprintln!(
            "[SERVER] WARNING: collections dir missing — pass MAPKIT_ROOT or run from the Egregoria root"
        );
    }

    let mut conn = db::open(&db_path);
    db::run_migrations(&mut conn);
    let zones = db::sync_collections_dir(&mut conn, &collections);
    let state = ServerState {
        conn: std::sync::Mutex::new(conn),
        root: root.clone(),
        started,
        admin: std::sync::Mutex::new(super::state::AdminCache::default()),
    };

    eprintln!("[SERVER] zone registry synced — {zones} zone(s) registered");

    let web_dist = std::env::var("MAPKIT_WEB_DIST")
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| root.join("mapkit-web/dist"));
    if web_dist.join("index.html").is_file() {
        eprintln!("[SERVER] web UI      = {} (served at /)", web_dist.display());
    } else {
        eprintln!(
            "[SERVER] web UI      = {} MISSING (build it: npm --prefix mapkit-web install && npm --prefix mapkit-web run build; / falls back to /docs)",
            web_dist.display()
        );
    }

    let figment = rocket::Config::figment()
        .merge(("address", "127.0.0.1"))
        .merge(("port", port))
        .merge(("log_level", "off"))
        // the game process owns signal handling — never install ctrl-c
        .merge(("ctrlc", false));

    let rocket = rocket::custom(figment)
        .mount("/", route_table())
        .manage(state);
    // Vite zone-picker at `/` (rank 10: every explicit API/tile route wins;
    // FileServer only answers when a dist file matches, else it forwards).
    // Missing dist/ → /docs index still answers; `cargo run` never breaks.
    let rocket = if web_dist.join("index.html").is_file() {
        rocket.mount(
            "/",
            FileServer::new(web_dist, rocket::fs::Options::Index).rank(10),
        )
    } else {
        rocket
    };
    rocket.attach(Cors)
}

/// Run the server on the CALLING thread until shutdown. Used by the
/// embedded game thread and the standalone bin.
pub fn run_blocking(port: u16) -> Result<(), rocket::Error> {
    // Rocket re-exports tokio — no extra runtime dependency needed.
    let rt = rocket::tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("mapkit-server: build tokio runtime");

    rt.block_on(async move {
        let rocket = build_rocket(port).ignite().await?;
        let cfg = rocket.config();
        eprintln!("[SERVER] Listening on http://{}:{}", cfg.address, cfg.port);
        let launched = rocket.launch().await?;
        eprintln!("[SERVER] shut down cleanly");
        drop(launched);
        Ok(())
    })
}

/// Start the embedded server on a background thread. Called by the game
/// at boot; never panics on its own — failures are logged.
pub fn spawn_embedded(port: u16) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("mapkit-server".into())
        .spawn(move || {
            eprintln!("[MAPKIT] embedded mapkit-server starting on port {port}");
            match run_blocking(port) {
                Ok(()) => eprintln!("[MAPKIT] embedded mapkit-server stopped"),
                Err(e) => eprintln!("[MAPKIT] embedded mapkit-server failed: {e}"),
            }
        })
        .expect("spawn mapkit-server thread")
}

/// GET /health — liveness + counts. Consumed by the Egregoria native app
/// (MAPKIT SERVER status block) and the docs page.
#[get("/health")]
fn health(state: &rocket::State<ServerState>) -> rocket::serde::json::Json<serde_json::Value> {
    use rocket::serde::json::Json;
    let st = state.inner();
    let mut conn = st.conn.lock().unwrap();
    let zones = db::count_zones(&mut conn);
    let tiles = db::count_tiles(&mut conn);
    drop(conn);
    Json(serde_json::json!({
        "status": "ok",
        "service": "mapkit-server",
        "version": env!("CARGO_PKG_VERSION"),
        "zones": zones,
        "collections": zones, // native probe compat
        "tiles": tiles,
        "uptime_s": st.started.elapsed().as_secs(),
    }))
}
