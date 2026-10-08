//! The `.mapbin` bundle format — ONE binary file that groups every layer of
//! a zone (terrain DEM, road graph, rivers, lakes, vegetation, buildings,
//! boundary, rails) behind an XML manifest.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! offset 0 : b"EGOMAPB1"   8-byte ASCII magic
//! offset 8 : version       u32  (1)
//! offset 12: xml_len       u64
//! offset 20: <xml_len bytes UTF-8 XML manifest>
//! offset 20+xml_len: concatenated layer blobs
//! ```
//!
//! The XML manifest is the single source of truth about what is inside:
//!
//! ```xml
//! <mapkit-bundle version="1" generator="mapkit 0.1.0" created="...">
//!   <zone name="..." kind="Region" district="" region=""
//!         center_lat="-6.7924" center_lon="39.2083" span_m="80000"
//!         mode="city" vscale="1.0" terrain_size="64"
//!         n_inters="..." n_roads="..." n_buildings="..." n_rivers="..."/>
//!   <layers>
//!     <layer id="terrain"   kind="terrain"        format="tzd1"              offset="0"  length="..."/>
//!     <layer id="roads"     kind="road-graph"     format="ego-map-txt-v2"    offset=".." length=".."/>
//!
//! `ego-map-txt-v2` edge lines are v1 (`a b lanes oneway speed npts [lat
//! lon]*`) plus trailing `class ref` tokens (`-` when empty, e.g.
//! `… primary T1`); v1 bundles without them still load.
//!     <layer id="rivers"    kind="waterways"      format="ego-lines-txt-v1"  offset=".." length=".."/>
//!     <layer id="water"     kind="water-polygons" format="ego-rings-txt-v1"  offset=".." length=".."/>
//!     <layer id="landcover" kind="landcover"      format="ego-rings-txt-v1"  offset=".." length=".."/>
//!     <layer id="buildings" kind="buildings"      format="ego-buildings-txt" offset=".." length=".."/>
//!     <layer id="boundary"  kind="boundary"       format="ego-rings-txt-v1"  offset=".." length=".."/>
//!     <layer id="rails"     kind="railways"       format="ego-lines-txt-v1"  offset=".." length=".."/>
//!   </layers>
//! </mapkit-bundle>
//! ```
//!
//! `offset` values are relative to the start of the blob area.

pub const MAGIC: &[u8; 8] = b"EGOMAPB1";
pub const VERSION: u32 = 1;

/// Fixed layer ids used by the engine loader.
pub const LAYER_TERRAIN: &str = "terrain";
pub const LAYER_ROADS: &str = "roads";
pub const LAYER_RIVERS: &str = "rivers";
pub const LAYER_WATER: &str = "water";
pub const LAYER_LANDCOVER: &str = "landcover";
pub const LAYER_BUILDINGS: &str = "buildings";
pub const LAYER_BOUNDARY: &str = "boundary";
pub const LAYER_RAILS: &str = "rails";

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct BundleMeta {
    pub name: String,
    pub kind: String,
    pub district: String,
    pub region: String,
    /// zone-registry link: which admin pack and entry this collection was
    /// grabbed from ("geoBoundaries-TZA", ADM level, the query used) — so a
    /// .mapbin always knows its own zone
    pub zone_source: String,
    pub zone_adm: u8,
    pub zone_query: String,
    pub center_lat: f64,
    pub center_lon: f64,
    pub span_m: f64,
    pub mode: String,
    pub vscale: f32,
    pub terrain_size: u16,
    pub n_inters: usize,
    pub n_roads: usize,
    pub n_buildings: usize,
    pub n_rivers: usize,
    pub generator: String,
    pub created: String,
}

#[derive(Debug, Clone)]
pub struct BundleLayer {
    pub id: String,
    pub kind: String,
    pub format: String,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone)]
pub struct Bundle {
    pub meta: BundleMeta,
    pub layers: Vec<BundleLayer>,
    /// concatenated layer blobs (offsets in `layers` index into this)
    pub data: Vec<u8>,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[derive(Default)]
pub struct BundleBuilder {
    meta: BundleMeta,
    layers: Vec<(BundleLayer, Vec<u8>)>,
}

impl BundleBuilder {
    pub fn new(meta: BundleMeta) -> Self {
        BundleBuilder {
            meta,
            layers: Vec::new(),
        }
    }

    /// Add a layer blob; the offset is assigned on `finish`.
    pub fn add_layer(&mut self, id: &str, kind: &str, format: &str, data: Vec<u8>) {
        self.layers.push((
            BundleLayer {
                id: id.to_string(),
                kind: kind.to_string(),
                format: format.to_string(),
                offset: 0,
                length: data.len() as u64,
            },
            data,
        ));
    }

    pub fn has_layer(&self, id: &str) -> bool {
        self.layers.iter().any(|(l, _)| l.id == id)
    }

    pub fn finish(mut self) -> Bundle {
        let mut data = Vec::with_capacity(self.layers.iter().map(|(_, d)| d.len()).sum());
        for (l, d) in &mut self.layers {
            l.offset = data.len() as u64;
            data.extend_from_slice(d);
        }
        let layers = self.layers.into_iter().map(|(l, _)| l).collect();
        Bundle {
            meta: self.meta,
            layers,
            data,
        }
    }
}

impl Bundle {
    // ── XML manifest ────────────────────────────────────────────────────

    pub fn manifest_xml(&self) -> String {
        let m = &self.meta;
        let mut x = String::with_capacity(2048);
        x.push_str(&format!(
            "<mapkit-bundle version=\"{VERSION}\" generator=\"{}\" created=\"{}\">\n",
            xml_escape(&m.generator),
            xml_escape(&m.created)
        ));
        x.push_str(&format!(
            "  <zone name=\"{}\" kind=\"{}\" district=\"{}\" region=\"{}\" zone_source=\"{}\" zone_adm=\"{}\" zone_query=\"{}\" center_lat=\"{:.7}\" center_lon=\"{:.7}\" span_m=\"{:.0}\" mode=\"{}\" vscale=\"{:.4}\" terrain_size=\"{}\" n_inters=\"{}\" n_roads=\"{}\" n_buildings=\"{}\" n_rivers=\"{}\"/>\n",
            xml_escape(&m.name),
            xml_escape(&m.kind),
            xml_escape(&m.district),
            xml_escape(&m.region),
            xml_escape(&m.zone_source),
            m.zone_adm,
            xml_escape(&m.zone_query),
            m.center_lat,
            m.center_lon,
            m.span_m,
            xml_escape(&m.mode),
            m.vscale,
            m.terrain_size,
            m.n_inters,
            m.n_roads,
            m.n_buildings,
            m.n_rivers
        ));
        x.push_str("  <layers>\n");
        for l in &self.layers {
            x.push_str(&format!(
                "    <layer id=\"{}\" kind=\"{}\" format=\"{}\" offset=\"{}\" length=\"{}\"/>\n",
                xml_escape(&l.id),
                xml_escape(&l.kind),
                xml_escape(&l.format),
                l.offset,
                l.length
            ));
        }
        x.push_str("  </layers>\n");
        x.push_str("</mapkit-bundle>\n");
        x
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let xml = self.manifest_xml();
        let mut out = Vec::with_capacity(20 + xml.len() + self.data.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(xml.len() as u64).to_le_bytes());
        out.extend_from_slice(xml.as_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    pub fn write_to(&self, path: &Path) -> Result<u64, String> {
        let bytes = self.to_bytes();
        let n = bytes.len() as u64;
        std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(n)
    }

    // ── reading ─────────────────────────────────────────────────────────

    pub fn from_bytes(b: &[u8]) -> Result<Bundle, String> {
        if b.len() < 20 {
            return Err("file too small for a mapkit bundle".into());
        }
        if &b[0..8] != MAGIC {
            return Err("bad magic — not an EGOMAPB1 bundle".into());
        }
        let version = u32::from_le_bytes(b[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(format!("unsupported bundle version {version}"));
        }
        let xml_len = u64::from_le_bytes(b[12..20].try_into().unwrap()) as usize;
        if b.len() < 20 + xml_len {
            return Err("truncated XML manifest".into());
        }
        let xml = std::str::from_utf8(&b[20..20 + xml_len])
            .map_err(|_| "XML manifest is not UTF-8".to_string())?;
        let data_start = 20 + xml_len;
        let data = b[data_start..].to_vec();

        let (meta, layers) = parse_manifest(xml)
            .ok_or_else(|| "could not parse the XML manifest".to_string())?;
        Ok(Bundle { meta, layers, data })
    }

    pub fn from_file(path: &Path) -> Result<Bundle, String> {
        let b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        Bundle::from_bytes(&b)
    }

    /// Read only the header (magic + XML manifest) of a bundle file —
    /// cheap enough for listing big collections.
    pub fn probe_file(path: &Path) -> Result<BundleMeta, String> {
        let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        use std::io::Read;
        let mut head = [0u8; 20];
        f.read_exact(&mut head)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if &head[0..8] != MAGIC {
            return Err("bad magic — not an EGOMAPB1 bundle".into());
        }
        let version = u32::from_le_bytes(head[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(format!("unsupported bundle version {version}"));
        }
        let xml_len = u64::from_le_bytes(head[12..20].try_into().unwrap()) as usize;
        if xml_len > 1 << 20 {
            return Err("manifest implausibly large".into());
        }
        let mut xml = vec![0u8; xml_len];
        f.read_exact(&mut xml)
            .map_err(|e| format!("read manifest: {e}"))?;
        let xml = std::str::from_utf8(&xml).map_err(|_| "manifest is not UTF-8".to_string())?;
        parse_manifest(xml)
            .map(|(m, _)| m)
            .ok_or_else(|| "could not parse the XML manifest".to_string())
    }

    /// Raw bytes of one layer blob.
    pub fn layer(&self, id: &str) -> Option<&[u8]> {
        let l = self.layers.iter().find(|l| l.id == id)?;
        let s = l.offset as usize;
        let e = s + l.length as usize;
        self.data.get(s..e)
    }

    /// A layer blob decoded as UTF-8 text.
    pub fn layer_text(&self, id: &str) -> Option<String> {
        self.layer(id)
            .map(|b| String::from_utf8_lossy(b).to_string())
    }
}

// ── tiny XML manifest parser ────────────────────────────────────────────
// The manifest is machine-written (see manifest_xml) with a fixed shape:
// one <mapkit-bundle> root, one self-closing <zone .../> and a <layers>
// block of self-closing <layer .../> elements with key="value" attributes.
// A ~60-line scanner is fully sufficient and keeps mapkit dependency-free.

fn parse_attrs(tag: &str) -> Option<HashMap<String, String>> {
    let mut out = HashMap::new();
    let b = tag.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        // find next non-space run = attribute name
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < b.len() && b[i] != b'=' && !(b[i] as char).is_whitespace() {
            i += 1;
        }
        if name_start == i {
            break;
        }
        let name = tag[name_start..i].to_string();
        // skip to '='
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            continue;
        }
        i += 1;
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'"' {
            continue;
        }
        i += 1;
        let val_start = i;
        while i < b.len() && b[i] != b'"' {
            i += 1;
        }
        if i > b.len() {
            break;
        }
        let value = xml_unescape(&tag[val_start..i.min(tag.len())]);
        i += 1; // closing quote
        out.insert(name, value);
    }
    Some(out)
}

/// find the body of the first tag `<name ...>` and return (attrs, end index of tag)
fn find_tag(xml: &str, name: &str, from: usize) -> Option<(HashMap<String, String>, usize)> {
    let pat = format!("<{name} ");
    let start = xml[from..].find(&pat)? + from;
    let end = xml[start..].find(['>', '/'])? + start;
    let attrs = parse_attrs(&xml[start + pat.len() - 1..end])?;
    Some((attrs, end))
}

fn get<T: std::str::FromStr>(a: &HashMap<String, String>, k: &str) -> Option<T> {
    a.get(k)?.trim().parse().ok()
}

fn get_s(a: &HashMap<String, String>, k: &str) -> String {
    a.get(k).cloned().unwrap_or_default()
}

/// Parse a manifest into (zone meta, layer list).
pub fn parse_manifest(xml: &str) -> Option<(BundleMeta, Vec<BundleLayer>)> {
    let (za, _) = find_tag(xml, "zone", 0)?;
    let meta = BundleMeta {
        name: get_s(&za, "name"),
        kind: get_s(&za, "kind"),
        district: get_s(&za, "district"),
        region: get_s(&za, "region"),
        zone_source: get_s(&za, "zone_source"),
        zone_adm: get(&za, "zone_adm").unwrap_or(0),
        zone_query: get_s(&za, "zone_query"),
        center_lat: get(&za, "center_lat")?,
        center_lon: get(&za, "center_lon")?,
        span_m: get(&za, "span_m")?,
        mode: get_s(&za, "mode"),
        vscale: get(&za, "vscale").unwrap_or(1.0),
        terrain_size: get(&za, "terrain_size")?,
        n_inters: get(&za, "n_inters").unwrap_or(0),
        n_roads: get(&za, "n_roads").unwrap_or(0),
        n_buildings: get(&za, "n_buildings").unwrap_or(0),
        n_rivers: get(&za, "n_rivers").unwrap_or(0),
        generator: get_s(&za, "generator"),
        created: String::new(),
    };

    // root attributes (generator/created live on <mapkit-bundle>)
    let mut meta = meta;
    if let Some((ra, _)) = find_tag(xml, "mapkit-bundle", 0) {
        meta.generator = get_s(&ra, "generator");
        meta.created = get_s(&ra, "created");
    }

    let layers_start = xml.find("<layers>")?;
    let layers_end = xml.find("</layers>").unwrap_or(xml.len());
    let block = &xml[layers_start..layers_end];
    let mut layers = Vec::new();
    let mut cursor = 0usize;
    while let Some((a, end)) = find_tag(block, "layer", cursor) {
        layers.push(BundleLayer {
            id: get_s(&a, "id"),
            kind: get_s(&a, "kind"),
            format: get_s(&a, "format"),
            offset: get(&a, "offset").unwrap_or(0),
            length: get(&a, "length").unwrap_or(0),
        });
        cursor = end;
    }
    Some((meta, layers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut meta = BundleMeta::default();
        meta.name = "Test \"zone\" <&>".into();
        meta.zone_source = "geoBoundaries-TZA".into();
        meta.zone_adm = 1;
        meta.zone_query = "region:Dar es Salaam".into();
        meta.center_lat = -6.8;
        meta.center_lon = 39.2;
        meta.span_m = 1000.0;
        meta.terrain_size = 16;
        let mut b = BundleBuilder::new(meta);
        b.add_layer("terrain", "terrain", "tzd1", vec![1, 2, 3]);
        b.add_layer("roads", "road-graph", "ego-map-txt-v1", b"hello world".to_vec());
        let bundle = b.finish();
        let bytes = bundle.to_bytes();
        let back = Bundle::from_bytes(&bytes).unwrap();
        assert_eq!(back.meta.name, "Test \"zone\" <&>");
        assert_eq!(back.meta.zone_source, "geoBoundaries-TZA");
        assert_eq!(back.meta.zone_adm, 1);
        assert_eq!(back.layer("roads").unwrap(), b"hello world");
        assert_eq!(back.layer("terrain").unwrap(), &[1, 2, 3]);
        assert!(Bundle::from_bytes(&bytes[..10]).is_err());
    }
}
