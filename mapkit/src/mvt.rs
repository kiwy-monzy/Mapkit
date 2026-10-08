//! MVT (Mapbox Vector Tiles, mapbox-streets-v8) parser → typed Tanzania features.
//! All output geometry is real lat/lon (WGS84).

use crate::pbf::{zigzag, Reader};
use std::collections::HashMap;

#[derive(Clone, PartialEq)]
enum Geom {
    Point,
    Line,
    Polygon,
}

#[derive(Clone)]
pub struct RoadF {
    pub pts: Vec<(f64, f64)>,
    pub class: String,
    pub surface: String,
    pub oneway: bool,
    pub rail: bool,
    /// OSM-style route reference, e.g. "T1;T23" (empty when untagged)
    pub road_ref: String,
    /// road name, e.g. "Morogoro Road" (empty when untagged)
    pub name: String,
}

#[derive(Clone)]
pub struct BuildF {
    pub rings: Vec<Vec<(f64, f64)>>,
    pub height: f32,
    pub min_h: f32,
}

#[derive(Clone)]
pub struct PolyF {
    pub rings: Vec<Vec<(f64, f64)>>,
    pub class: String,
}

#[derive(Clone)]
pub struct LineF {
    pub pts: Vec<(f64, f64)>,
    pub class: String,
}

#[derive(Default)]
pub struct TileData {
    pub roads: Vec<RoadF>,
    pub buildings: Vec<BuildF>,
    pub water: Vec<PolyF>,
    pub waterways: Vec<LineF>,
    pub landcover: Vec<PolyF>,
}

pub struct TileRef {
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

fn tile_bounds(t: &TileRef) -> (f64, f64, f64, f64) {
    // (lon0, lat0_top, lon1, lat1_bottom)
    let n = (1u64 << t.z) as f64;
    let lon0 = t.x as f64 / n * 360.0 - 180.0;
    let lon1 = (t.x as f64 + 1.0) / n * 360.0 - 180.0;
    let lat_top = y2lat(t.y as f64, n);
    let lat_bot = y2lat(t.y as f64 + 1.0, n);
    (lon0, lat_top, lon1, lat_bot)
}

fn y2lat(y: f64, n: f64) -> f64 {
    let p = (1.0 - 2.0 * y / n) * std::f64::consts::PI;
    p.sinh().atan().to_degrees()
}

fn decode_geom(
    g: &[u32],
    tr: &TileRef,
    extent: f64,
    area: bool,
) -> Vec<Vec<(f64, f64)>> {
    let (lon0, lat_top, lon1, lat_bot) = tile_bounds(tr);
    let dx = (lon1 - lon0) / extent;
    let dy = (lat_top - lat_bot) / extent;

    let mut rings: Vec<Vec<(f64, f64)>> = vec![];
    let mut cur: Vec<(f64, f64)> = vec![];
    let mut cx: i64 = 0;
    let mut cy: i64 = 0;
    let mut i = 0usize;
    while i < g.len() {
        let cmd = g[i] & 7;
        let count = (g[i] >> 3) as usize;
        i += 1;
        match cmd {
            1 => {
                // MoveTo
                for _ in 0..count {
                    if !cur.is_empty() {
                        rings.push(std::mem::take(&mut cur));
                    }
                    if i + 1 >= g.len() {
                        break;
                    }
                    cx += zigzag(g[i] as u64) as i64;
                    cy += zigzag(g[i + 1] as u64) as i64;
                    i += 2;
                    let lon = lon0 + cx as f64 * dx;
                    let lat = lat_top - cy as f64 * dy;
                    cur.push((lon, lat));
                }
            }
            2 => {
                // LineTo
                for _ in 0..count {
                    if i + 1 >= g.len() {
                        break;
                    }
                    cx += zigzag(g[i] as u64) as i64;
                    cy += zigzag(g[i + 1] as u64) as i64;
                    i += 2;
                    let lon = lon0 + cx as f64 * dx;
                    let lat = lat_top - cy as f64 * dy;
                    cur.push((lon, lat));
                }
            }
            7 => {
                // ClosePath
                if let Some(&first) = cur.first() {
                    cur.push(first);
                }
            }
            _ => {}
        }
    }
    if !cur.is_empty() {
        rings.push(cur);
    }
    let _ = area;
    rings
}

fn val_str(v: &Val) -> String {
    match v {
        Val::S(s) => s.clone(),
        Val::F(f) => format!("{}", f),
        Val::D(d) => format!("{}", d),
        Val::I(i) => format!("{}", i),
        Val::U(u) => format!("{}", u),
        Val::B(b) => format!("{}", b),
    }
}

enum Val {
    S(String),
    F(f32),
    D(f64),
    I(i64),
    U(u64),
    B(bool),
}

/// Parse a (possibly gzipped) MVT tile into typed features.
pub fn parse(raw: &[u8], t: &TileRef) -> Option<TileData> {
    let bytes: Vec<u8> = if raw.len() > 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        let mut d = flate2::read::GzDecoder::new(raw);
        use std::io::Read;
        let mut out = vec![];
        d.read_to_end(&mut out).ok()?;
        out
    } else {
        raw.to_vec()
    };

    let mut data = TileData::default();
    let mut r = Reader::new(&bytes);

    while let Some((f, wire)) = r.key() {
        if f == 3 && wire == 2 {
            let n = r.len_varint();
            let layer = r.bytes(n);
            parse_layer(layer, t, &mut data);
        } else {
            r.skip(wire);
        }
    }
    Some(data)
}

fn parse_layer(b: &[u8], t: &TileRef, out: &mut TileData) {
    let mut r = Reader::new(b);
    let mut name = String::new();
    let mut keys: Vec<String> = vec![];
    let mut values: Vec<Val> = vec![];
    let mut extent = 4096.0;
    // We need owned slices of features — collect offsets instead.
    let mut feat_ranges: Vec<(usize, usize)> = vec![];
    let base = r.i;

    while let Some((f, wire)) = r.key() {
        match (f, wire) {
            (1, 2) => {
                let n = r.len_varint();
                name = String::from_utf8_lossy(r.bytes(n)).to_string();
            }
            (3, 2) => {
                let n = r.len_varint();
                keys.push(String::from_utf8_lossy(r.bytes(n)).to_string());
            }
            (4, 2) => {
                let n = r.len_varint();
                let s = r.i;
                values.push(parse_value(r.bytes(n)));
                let _ = s;
            }
            (2, 2) => {
                let n = r.len_varint();
                let s = r.i;
                let _ = r.bytes(n);
                feat_ranges.push((s, r.i));
            }
            (5, 0) => extent = r.varint() as f64,
            _ => r.skip(wire),
        }
    }
    let _ = base;

    for (s, e) in feat_ranges {
        let feat = &b[s..e];
        parse_feature(feat, &name, &keys, &values, extent, t, out);
    }
}

fn parse_value(b: &[u8]) -> Val {
    let mut r = Reader::new(b);
    while let Some((f, wire)) = r.key() {
        match (f, wire) {
            (1, 2) => {
                let n = r.len_varint();
                return Val::S(String::from_utf8_lossy(r.bytes(n)).to_string());
            }
            (2, 5) => {
                let bs = r.bytes(4);
                return Val::F(f32::from_le_bytes([bs[0], bs[1], bs[2], bs[3]]));
            }
            (3, 1) => {
                let bs = r.bytes(8);
                let mut a = [0u8; 8];
                a.copy_from_slice(bs);
                return Val::D(f64::from_le_bytes(a));
            }
            (4, 0) => return Val::I(r.varint() as i64),
            (5, 0) => return Val::U(r.varint()),
            (6, 0) => return Val::I(zigzag(r.varint())),
            (7, 0) => return Val::B(r.varint() != 0),
            _ => r.skip(wire),
        }
    }
    Val::S(String::new())
}

fn parse_feature(
    b: &[u8],
    layer: &str,
    keys: &[String],
    values: &[Val],
    extent: f64,
    t: &TileRef,
    out: &mut TileData,
) {
    let mut r = Reader::new(b);
    let mut tags: Vec<u32> = vec![];
    let mut gtype = Geom::Point;
    let mut geom: Vec<u32> = vec![];

    while let Some((f, wire)) = r.key() {
        match (f, wire) {
            (2, 2) => {
                let n = r.len_varint();
                let bs = r.bytes(n);
                let mut rr = Reader::new(bs);
                while !rr.eof() {
                    tags.push(rr.varint() as u32);
                }
            }
            (3, 0) => {
                gtype = match r.varint() {
                    2 => Geom::Line,
                    3 => Geom::Polygon,
                    _ => Geom::Point,
                }
            }
            (4, 2) => {
                let n = r.len_varint();
                let bs = r.bytes(n);
                let mut rr = Reader::new(bs);
                while !rr.eof() {
                    geom.push(rr.varint() as u32);
                }
            }
            _ => r.skip(wire),
        }
    }

    let mut props: HashMap<&str, String> = HashMap::new();
    let mut i = 0;
    while i + 1 < tags.len() {
        let ki = tags[i] as usize;
        let vi = tags[i + 1] as usize;
        if ki < keys.len() && vi < values.len() {
            props.insert(keys[ki].as_str(), val_str(&values[vi]));
        }
        i += 2;
    }
    let num = |k: &str| props.get(k).and_then(|v| values_lookup(v)).unwrap_or(0.0);
    let _ = num;

    match layer {
        "road" => {
            if gtype != Geom::Line {
                return;
            }
            let class = props.get("class").cloned().unwrap_or_default();
            let rings = decode_geom(&geom, t, extent, false);
            for pts in rings {
                if pts.len() < 2 {
                    continue;
                }
                let is_rail = class.contains("rail");
                out.roads.push(RoadF {
                    pts,
                    class: class.clone(),
                    surface: props.get("surface").cloned().unwrap_or_default(),
                    oneway: props.get("oneway").map(|s| s == "true").unwrap_or(false)
                        || matches!(class.as_str(), "motorway" | "motorway_link"),
                    rail: is_rail,
                    road_ref: props.get("ref").cloned().unwrap_or_default(),
                    name: props.get("name").cloned().unwrap_or_default(),
                });
            }
        }
        "building" => {
            if gtype != Geom::Polygon {
                return;
            }
            let rings = decode_geom(&geom, t, extent, true);
            if rings.is_empty() {
                return;
            }
            let h = str_num(props.get("height"));
            let mh = str_num(props.get("min_height"));
            out.buildings.push(BuildF {
                rings,
                height: if h > 0.0 { h } else { 4.0 },
                min_h: mh,
            });
        }
        "water" => {
            if gtype != Geom::Polygon {
                return;
            }
            let rings = decode_geom(&geom, t, extent, true);
            out.water.push(PolyF {
                rings,
                class: props.get("class").cloned().unwrap_or_default(),
            });
        }
        "waterway" => {
            if gtype != Geom::Line {
                return;
            }
            let rings = decode_geom(&geom, t, extent, false);
            for pts in rings {
                if pts.len() >= 2 {
                    out.waterways.push(LineF {
                        pts,
                        class: props.get("class").cloned().unwrap_or_default(),
                    });
                }
            }
        }
        "landcover" | "landuse" | "park" => {
            if gtype != Geom::Polygon {
                return;
            }
            let rings = decode_geom(&geom, t, extent, true);
            let cls = props
                .get("class")
                .or_else(|| props.get("type"))
                .cloned()
                .unwrap_or_default();
            out.landcover.push(PolyF { rings, class: cls });
        }
        _ => {}
    }
}

fn str_num(v: Option<&String>) -> f32 {
    v.and_then(|s| s.trim().parse::<f32>().ok()).unwrap_or(0.0)
}

fn values_lookup(_s: &str) -> Option<f64> {
    None
}
