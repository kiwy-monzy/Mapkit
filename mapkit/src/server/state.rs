//! Shared server state + root resolution.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use diesel::prelude::*;

pub struct ServerState {
    pub conn: Mutex<SqliteConnection>,
    /// Egregoria root (contains assets/collections + tilecache + target/).
    pub root: PathBuf,
    pub started: Instant,
    /// lazily-loaded Tanzania admin pack (ADM1/2/3 boundaries), shared by
    /// the /api/admin/* endpoints so the 12 MB of GeoJSON is parsed once.
    pub admin: Mutex<AdminCache>,
}

#[derive(Default)]
pub struct AdminCache {
    dir: PathBuf,
    pub index: Option<crate::boundaries::AdminIndex>,
}

impl AdminCache {
    /// Borrow the loaded index, (re)loading when the pack dir changed.
    /// Holds the server mutex only for the load — callers clone what they
    /// need (AdminIndex is Clone) or build JSON under the lock.
    pub fn get_or_load(
        cache: &Mutex<AdminCache>,
        dir: &Path,
    ) -> Option<crate::boundaries::AdminIndex> {
        let mut guard = cache.lock().unwrap();
        let stale = guard.dir != dir || guard.index.is_none();
        if stale {
            let loaded = crate::boundaries::AdminIndex::load(&dir.to_string_lossy());
            if let Some(idx) = loaded {
                eprintln!(
                    "[SERVER] admin pack loaded from {} ({} zones)",
                    dir.display(),
                    idx.zones.len()
                );
                guard.dir = dir.to_path_buf();
                guard.index = Some(idx);
            } else {
                eprintln!("[SERVER] admin pack missing at {}", dir.display());
                return None;
            }
        }
        guard.index.clone()
    }
}

impl ServerState {
    pub fn collections_dir(&self) -> PathBuf {
        self.root.join("assets/collections")
    }

    pub fn tilecache_dir(&self) -> PathBuf {
        self.root.join("tilecache")
    }

    /// Vite zone-picker build output served at `/` (env override wins).
    pub fn web_dist_dir(&self) -> PathBuf {
        if let Ok(p) = std::env::var("MAPKIT_WEB_DIST") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        self.root.join("mapkit-web/dist")
    }
}

/// Resolve the Egregoria root: MAPKIT_ROOT env, else the current working
/// directory (the game and the documented run mode both run from the
/// root), else walk up from the executable (embedded mode when the game
/// was launched from elsewhere).
pub fn resolve_root() -> PathBuf {
    if let Ok(p) = std::env::var("MAPKIT_ROOT") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("assets/collections").is_dir() {
        return cwd;
    }
    // running from a workspace member directory inside the workspace
    let parent = cwd.parent().map(Path::to_path_buf);
    if let Some(p) = parent {
        if p.join("assets/collections").is_dir() {
            return p;
        }
    }
    // embedded in the game binary launched from an arbitrary cwd: walk up
    // from the executable (…/Egregoria/target/release/mapkit-server-standalone)
    if let Ok(exe) = std::env::current_exe() {
        for anc in exe.ancestors().skip(1) {
            if anc.join("assets/collections").is_dir() {
                return anc.to_path_buf();
            }
        }
    }
    cwd
}
