//! Codecs for the text layer formats carried inside `.mapbin` bundles
//! (inherited from the original mapstudio exporter).
//!
//! All coordinates parsed here are raw `[lon, lat]` pairs — projection to
//! world metres happens in the engine loader / layer registry.

/// Open polylines — rivers.txt / rails.txt: count, then per line one record
/// `npts kind lat lon lat lon ...`
pub fn parse_lines(txt: &str) -> Vec<(String, Vec<[f64; 2]>)> {
    let mut lines = txt.lines();
    let _count: usize = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    let mut out = Vec::new();
    for line in lines {
        let mut tok = line.split_whitespace();
        let Some(npts) = tok.next().and_then(|t| t.parse::<usize>().ok()) else {
            continue;
        };
        let kind = tok.next().unwrap_or("stream").to_string();
        let mut pts = Vec::with_capacity(npts);
        for _ in 0..npts {
            // exporter pairs are "lat lon"
            let Some(lat) = tok.next().and_then(|t| t.parse::<f64>().ok()) else {
                break;
            };
            let Some(lon) = tok.next().and_then(|t| t.parse::<f64>().ok()) else {
                break;
            };
            pts.push([lon, lat]);
        }
        if pts.len() >= 2 {
            out.push((kind, pts));
        }
    }
    out
}

/// Closed rings with an optional kind tag.
///
/// water.txt: one polygon per record — a `nrings class` head line followed
/// by one line per ring `npts lat lon lat lon ...` (the head lines fail the
/// coordinate scan and are skipped; the ring lines carry the geometry).
/// landcover.txt: two lines per area — `npts kind` then the ring
/// coordinates on the following line.
pub fn parse_tagged_rings(txt: &str, with_kind: bool) -> Vec<(String, Vec<[f64; 2]>)> {
    let lines: Vec<&str> = txt.lines().collect();
    let mut idx = 0usize;
    let _count: usize = lines
        .get(idx)
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    idx += 1;

    let mut out = Vec::new();
    while idx < lines.len() {
        let head = lines[idx].trim().to_string();
        idx += 1;
        if head.is_empty() {
            continue;
        }
        let mut tok = head.split_whitespace();
        let npts: usize = match tok.next().and_then(|t| t.parse().ok()) {
            Some(n) => n,
            None => continue,
        };
        if npts == 0 {
            continue;
        }
        let kind = if with_kind {
            tok.next().unwrap_or("unknown").to_string()
        } else {
            String::new()
        };

        // gather the ring; coords are on the same line (water.txt) or on the
        // following line (landcover.txt)
        let mut pts: Vec<[f64; 2]> = Vec::with_capacity(npts);
        let mut rest = tok;
        let mut lines_left = if with_kind { 2 } else { 1 };
        'collect: while pts.len() < npts {
            match rest.next() {
                Some(v) => match v.parse::<f64>() {
                    // exporter pairs are "lat lon"
                    Ok(lat) => match rest.next().and_then(|l| l.parse::<f64>().ok()) {
                        Some(lon) => pts.push([lon, lat]),
                        None => break 'collect,
                    },
                    Err(_) => break 'collect,
                },
                None => {
                    lines_left -= 1;
                    if lines_left == 0 || idx >= lines.len() {
                        break 'collect;
                    }
                    rest = lines[idx].split_whitespace();
                    idx += 1;
                }
            }
        }

        // strict: only accept complete, sane rings
        if pts.len() >= 3 && pts.len() >= npts.saturating_sub(1) {
            out.push((kind, pts));
        }
    }
    out
}

/// buildings.txt: count, then per building: `height minz npts` line and a
/// coords line.
pub fn parse_buildings(txt: &str) -> Vec<(f32, Vec<[f64; 2]>)> {
    let lines: Vec<&str> = txt.lines().collect();
    let mut idx = 0usize;
    let _count: usize = lines
        .get(idx)
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    idx += 1;

    let mut out = Vec::new();
    while idx + 1 < lines.len() {
        let head = lines[idx].trim().to_string();
        idx += 1;
        let mut tok = head.split_whitespace();
        let height: f32 = tok.next().and_then(|t| t.parse().ok()).unwrap_or(3.0);
        let _minz: f32 = tok.next().and_then(|t| t.parse().ok()).unwrap_or(0.0);
        let npts: usize = match tok.next().and_then(|t| t.parse().ok()) {
            Some(n) => n,
            None => continue,
        };

        let mut pts = Vec::with_capacity(npts);
        'pts: while pts.len() < npts && idx < lines.len() {
            let mut t = lines[idx].split_whitespace();
            idx += 1;
            while pts.len() < npts {
                // exporter pairs are "lat lon"
                let Some(lat) = t.next().and_then(|v| v.parse::<f64>().ok()) else {
                    break 'pts;
                };
                let Some(lon) = t.next().and_then(|v| v.parse::<f64>().ok()) else {
                    break 'pts;
                };
                pts.push([lon, lat]);
            }
        }
        if pts.len() >= 3 {
            out.push((height, pts));
        }
    }
    out
}

/// boundary.txt: count; per boundary: `n_rings label` line, then per ring a
/// `npts lat lon lat lon ...` line. Returns the first ring plus the label.
pub fn parse_boundary(txt: &str) -> Option<(String, Vec<[f64; 2]>)> {
    let lines: Vec<&str> = txt.lines().collect();
    let _count: usize = lines.first().and_then(|l| l.trim().parse().ok())?;
    let head = lines.get(1)?.split_whitespace().collect::<Vec<_>>();
    let _n_rings: usize = head.first()?.parse().ok()?;
    let label = head
        .get(1)
        .copied()
        .unwrap_or("boundary")
        .to_string();

    // ring lines follow: first token = npts, then npts "lat lon" pairs
    for line in &lines[2..] {
        let mut t = line.split_whitespace();
        let Some(npts) = t.next().and_then(|v| v.parse::<usize>().ok()) else {
            continue;
        };
        let mut pts = Vec::with_capacity(npts);
        while pts.len() < npts {
            // exporter pairs are "lat lon"
            let Some(lat) = t.next().and_then(|v| v.parse::<f64>().ok()) else {
                break;
            };
            let Some(lon) = t.next().and_then(|v| v.parse::<f64>().ok()) else {
                break;
            };
            pts.push([lon, lat]);
        }
        if pts.len() >= 3 {
            return Some((label, pts));
        }
    }
    None
}
