//! Upstream tile fetching (ureq, blocking) + atomic cache writes.
//!
//! All remote traffic goes through here: Mapbox raster style tiles, vector
//! MVT tiles and terrain-RGB PNGs — the same upstream hosts and token the
//! mapkit-cli grabber uses, so the server and the grabber share one cache.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Blocking HTTP GET with 2 attempts and a hard timeout. Returns the body.
pub fn fetch(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    fetch_referer(url, None, timeout)
}

fn fetch_referer(url: &str, referer: Option<&str>, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for attempt in 0..2 {
        let mut req = ureq::get(url).timeout(timeout);
        if let Some(r) = referer {
            req = req.set("Referer", r);
        }
        match req.call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                match resp
                    .into_reader()
                    .take(32 * 1024 * 1024)
                    .read_to_end(&mut buf)
                {
                    Ok(_) if !buf.is_empty() => return Ok(buf),
                    Ok(_) => last = format!("empty body from {url}"),
                    Err(e) => last = format!("read {url}: {e}"),
                }
            }
            Err(e) => last = format!("get {url}: {e}"),
        }
        if attempt == 0 {
            std::thread::sleep(Duration::from_millis(400));
        }
    }
    Err(last)
}

/// Mapbox GET: tries `url` as built (round-robin key), then falls back to
/// every other configured key by swapping `access_token=` (sending each
/// key's referer). Keys share nothing else, so one revoked/rate-limited
/// key never sinks the request.
pub fn fetch_mapbox(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(url.to_string());
    // first attempt: url as given (already carries a round-robin key)
    let mut candidates = vec![(url.to_string(), None)];
    for (tok, referer) in crate::grab::tokens() {
        let swapped = swap_token(url, &tok);
        if seen.insert(swapped.clone()) {
            candidates.push((swapped, referer));
        }
    }
    for (u, referer) in candidates {
        match fetch_referer(&u, referer.as_deref(), timeout) {
            Ok(b) => return Ok(b),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Replace the `access_token=` query value with `token`.
fn swap_token(url: &str, token: &str) -> String {
    match url.find("access_token=") {
        Some(i) => {
            let head = &url[..i + "access_token=".len()];
            let tail = url[i + "access_token=".len()..]
                .find('&')
                .map(|j| &url[i + "access_token=".len() + j..])
                .unwrap_or("");
            format!("{head}{token}{tail}")
        }
        None => url.to_string(),
    }
}

/// Write bytes to a temp file next to `path`, then rename over it —
/// readers never see a half-written tile.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("part");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(path.to_path_buf())
}
