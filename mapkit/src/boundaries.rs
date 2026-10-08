//! geoBoundaries ADM1/2/3 (region → district → ward) loader + zone resolution.

use serde_json::Value;

#[derive(Clone)]
pub struct Zone {
    pub name: String,
    pub adm: u8, // 1=region 2=district 3=ward
    /// outer rings (lon, lat)
    pub rings: Vec<Vec<(f64, f64)>>,
    pub bbox: (f64, f64, f64, f64), // min_lon, min_lat, max_lon, max_lat
}

#[derive(Clone)]
pub struct AdminIndex {
    pub zones: Vec<Zone>, // all levels
}

/// true when `v` is a coordinate pair ([lon, lat(, z)])
fn is_point(v: &Value) -> bool {
    v.as_array()
        .map(|a| a.first().map(Value::is_number).unwrap_or(false) && a.len() >= 2)
        .unwrap_or(false)
}

/// collect every ring from arbitrarily nested polygon coordinates. Some
/// exports wrap Polygon coordinates one level too deep (`[[ring]]` instead
/// of `[ring]`) — descending until lists-of-points are found reads both the
/// declared type and the wrapped variant, and MultiPolygons besides.
fn collect_rings(v: &Value, out: &mut Vec<Vec<(f64, f64)>>) {
    let Some(arr) = v.as_array() else { return };
    if arr.first().map(is_point).unwrap_or(false) {
        let ring: Vec<(f64, f64)> = arr
            .iter()
            .filter_map(|p| {
                let a = p.as_array()?;
                Some((
                    a.first()?.as_f64().unwrap_or(0.0),
                    a.get(1)?.as_f64().unwrap_or(0.0),
                ))
            })
            .collect();
        if ring.len() >= 3 {
            out.push(ring);
        }
        return;
    }
    for x in arr {
        collect_rings(x, out);
    }
}

fn rings_of(geom: &Value) -> Vec<Vec<(f64, f64)>> {
    let mut out = vec![];
    let t = geom["type"].as_str().unwrap_or("");
    match t {
        "Polygon" | "MultiPolygon" => {
            if let Some(coords) = geom["coordinates"].as_array() {
                for x in coords {
                    collect_rings(x, &mut out);
                }
            }
        }
        _ => {}
    }
    out
}

fn coord(p: &Value) -> (f64, f64) {
    let a = p.as_array().cloned().unwrap_or_default();
    (
        a.first().and_then(|v| v.as_f64()).unwrap_or(0.0),
        a.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
    )
}

fn bbox_of(rings: &[Vec<(f64, f64)>]) -> (f64, f64, f64, f64) {
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

impl AdminIndex {
    pub fn load(dir: &str) -> Option<AdminIndex> {
        let mut zones = vec![];
        for (adm, file) in [
            (1u8, "geoBoundaries-TZA-ADM1.geojson"),
            (2, "geoBoundaries-TZA-ADM2.geojson"),
            (3, "geoBoundaries-TZA-ADM3.geojson"),
        ] {
            let path = format!("{}/{}", dir, file);
            let Ok(txt) = std::fs::read_to_string(&path) else {
                eprintln!("warning: missing {}", path);
                continue;
            };
            let v: Value = serde_json::from_str(&txt).ok()?;
            for f in v["features"].as_array()? {
                let name = f["properties"]["shapeName"]
                    .as_str()
                    .unwrap_or("??")
                    .to_string();
                let rings = rings_of(&f["geometry"]);
                if rings.is_empty() {
                    continue;
                }
                let bbox = bbox_of(&rings);
                zones.push(Zone { name, adm, rings, bbox });
            }
        }
        if zones.is_empty() {
            return None;
        }
        Some(AdminIndex { zones })
    }

    pub fn regions(&self) -> impl Iterator<Item = &Zone> {
        self.zones.iter().filter(|z| z.adm == 1)
    }

    /// find zone by exact-ish name (case-insensitive), searching wards → districts → regions
    /// a "kind:Name" prefix (ward:/district:/region:) restricts the level.
    /// NOTE: same-name zones exist in different parents (two "Mbezi" wards
    /// in Kinondoni and Mkuranga) — use [`find_scoped`] when the caller
    /// knows the district/region.
    pub fn find(&self, q: &str) -> Option<&Zone> {
        let mut ql = q.trim().to_lowercase();
        let mut adm_filter: Option<u8> = None;
        for (pre, adm) in [("ward:", 3u8), ("district:", 2u8), ("region:", 1u8)] {
            if let Some(rest) = ql.strip_prefix(pre) {
                adm_filter = Some(adm);
                ql = rest.trim().to_string();
                break;
            }
        }
        let cand = |adm: u8| -> Vec<&Zone> {
            let ql2 = ql.clone();
            self.zones
                .iter()
                .filter(move |z| z.adm == adm && z.name.to_lowercase().contains(&ql2))
                .collect()
        };
        let rank = |z: &Zone| -> (bool, usize) {
            (z.name.to_lowercase() != ql, z.name.len())
        };
        let pick = |adm: u8| -> Option<&Zone> {
            let exact: Option<&Zone> = self
                .zones
                .iter()
                .find(|z| z.adm == adm && z.name.to_lowercase() == ql);
            exact.or_else(|| cand(adm).into_iter().min_by_key(|z| rank(z)))
        };
        let levels: &[u8] = match adm_filter {
            Some(a) => &[a],
            None => &[3, 2, 1],
        };
        for a in levels {
            if let Some(z) = pick(*a) {
                return Some(z);
            }
        }
        None
    }

    /// Scoped lookup: exact name at `adm`, optionally restricted to a
    /// district and/or region parent (case-insensitive). Parenthood is
    /// tested by vertex overlap (a solid share of the zone's boundary
    /// vertices inside the parent ring) — NOT by centroid, which fails for
    /// coastal zones whose point average falls in the sea. Falls back to
    /// the unscoped [`find`] when nothing matches in scope.
    pub fn find_scoped(
        &self,
        adm: u8,
        name: &str,
        district: Option<&str>,
        region: Option<&str>,
    ) -> Option<&Zone> {
        let name = name.trim().to_lowercase();
        if district.is_some() || region.is_some() {
            let cands: Vec<&Zone> = self
                .zones
                .iter()
                .filter(|z| z.adm == adm && z.name.to_lowercase() == name)
                .collect();
            // most specific scope first: (district+region) → district → region
            if let (Some(dq), Some(rq)) = (district, region) {
                if let Some(z) = cands
                    .iter()
                    .find(|z| self.child_in(dq, 2, z) && self.child_in(rq, 1, z))
                {
                    return Some(z);
                }
            }
            if let Some(dq) = district {
                if let Some(z) = cands.iter().find(|z| self.child_in(dq, 2, z)) {
                    return Some(z);
                }
            }
            if let Some(rq) = region {
                if let Some(z) = cands.iter().find(|z| self.child_in(rq, 1, z)) {
                    return Some(z);
                }
            }
        }
        self.find(&format!(
            "{}:{}",
            match adm {
                1 => "region",
                2 => "district",
                _ => "ward",
            },
            name
        ))
    }

    /// true when a solid share of `child`'s boundary vertices lies inside the
    /// named parent zone at `parent_adm` (bbox reject first for high-res rings).
    fn child_in(&self, parent_name: &str, parent_adm: u8, child: &Zone) -> bool {
        let Some(parent) = self.zones.iter().find(|z| {
            z.adm == parent_adm && z.name.eq_ignore_ascii_case(parent_name.trim())
        }) else {
            return false;
        };
        if child.bbox.2 < parent.bbox.0
            || child.bbox.0 > parent.bbox.2
            || child.bbox.3 < parent.bbox.1
            || child.bbox.1 > parent.bbox.3
        {
            return false;
        }
        let mut total = 0usize;
        let mut inside = 0usize;
        for r in &child.rings {
            for &(lo, la) in r {
                total += 1;
                if parent.rings.iter().any(|rg| point_in_ring(lo, la, rg)) {
                    inside += 1;
                }
            }
        }
        total > 0 && inside as f64 / total as f64 >= 0.3
    }

    /// which ward/district/region polygon contains a point (by ring test)
    pub fn zone_at(&self, lon: f64, lat: f64, adm: u8) -> Option<&Zone> {
        self.zones
            .iter()
            .filter(|z| z.adm == adm)
            .find(|z| bbox_contains(&z.bbox, lon, lat) && z.rings.iter().any(|r| point_in_ring(lon, lat, r)))
    }

    /// parent names for a zone (district + region for a ward)
    pub fn parents(&self, z: &Zone) -> (String, String) {
        let c = centroid(&z.rings);
        match z.adm {
            3 => {
                let d = self.zone_at(c.0, c.1, 2);
                let r = self.zone_at(c.0, c.1, 1);
                (
                    d.map(|d| d.name.clone()).unwrap_or_default(),
                    r.map(|r| r.name.clone()).unwrap_or_default(),
                )
            }
            2 => {
                let r = self.zone_at(c.0, c.1, 1);
                (String::new(), r.map(|r| r.name.clone()).unwrap_or_default())
            }
            _ => (String::new(), String::new()),
        }
    }
}

pub fn bbox_contains(b: &(f64, f64, f64, f64), lon: f64, lat: f64) -> bool {
    lon >= b.0 && lon <= b.2 && lat >= b.1 && lat <= b.3
}

pub fn point_in_ring(lon: f64, lat: f64, ring: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let mut j = ring.len().saturating_sub(1);
    for i in 0..ring.len() {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if ((yi > lat) != (yj > lat))
            && (lon < (xj - xi) * (lat - yi) / (yj - yi + 1e-12) + xi)
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

pub fn centroid(rings: &[Vec<(f64, f64)>]) -> (f64, f64) {
    let mut sx = 0.0;
    let mut sy = 0.0;
    let mut n = 0.0;
    for r in rings {
        for (x, y) in r {
            sx += x;
            sy += y;
            n += 1.0;
        }
    }
    if n == 0.0 {
        (0.0, 0.0)
    } else {
        (sx / n, sy / n)
    }
}

/// write the full admin index (one line per zone) for search / in-game browser
pub fn write_index(a: &AdminIndex, path: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    for z in &a.zones {
        let (d, r) = a.parents(z);
        let kind = match z.adm {
            1 => "region",
            2 => "district",
            _ => "ward",
        };
        writeln!(f, "{}|{}|{}|{}|{}", kind, z.name, d, r, z.bbox.0)?;
    }
    Ok(())
}

pub fn slugify(s: &str) -> String {
    s.to_lowercase()
        .replace('\'', "")
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the shipped tanzania-pack: some features carry Polygon coordinates
    /// wrapped one level too deep ([[ring]] instead of [ring]) — every
    /// district/ward must still parse, and Dar's own children must resolve
    #[test]
    fn tanzania_pack_parses_despite_nested_polygon_coordinates() {
        let index = AdminIndex::load("../assets/tanzania-pack/boundaries")
            .expect("pack must load");
        let regions: Vec<_> = index.regions().map(|z| z.name.clone()).collect();
        assert_eq!(regions.len(), 31, "31 regions (post-Songwe)");

        let dar = index.find("region:Dar es Salaam").expect("Dar resolves");
        let wards_inside = index
            .zones
            .iter()
            .filter(|z| z.adm == 3)
            .filter(|z| {
                let c = centroid(&z.rings);
                dar.rings.iter().any(|r| point_in_ring(c.0, c.1, r))
            })
            .count();
        assert!(
            wards_inside >= 80,
            "Dar es Salaam holds ~90 wards; nested-coordinate parsing recovered {wards_inside}"
        );
        // a wrapped Polygon-typed feature keeps real coordinates (not (0,0))
        let degenerate = index
            .zones
            .iter()
            .filter(|z| z.rings.iter().any(|r| r.iter().any(|p| p.0 == 0.0 && p.1 == 0.0)))
            .count();
        assert_eq!(degenerate, 0, "no zone may contain (0,0) junk points");
    }
}
