//! The layer-source registry trait — any data provider (a `.mapbin`
//! bundle, a live GeoJSON feed, anything) can describe its layers through
//! this trait and be registered into the map editor's layer view.

use crate::bundle::Bundle;
use crate::formats;
use std::collections::BTreeMap;

/// One polyline of a layer (lat/lon pairs, kind tag).
#[derive(Debug, Clone)]
pub struct SourceLine {
    pub kind: String,
    pub pts: Vec<[f64; 2]>,
}

/// One filled polygon of a layer (first = outer ring, lat/lon).
#[derive(Debug, Clone)]
pub struct SourcePoly {
    pub kind: String,
    pub ring: Vec<[f64; 2]>,
    /// extrusion height for building footprints (0 = flat)
    pub height: f32,
}

/// A fully described layer, ready to register in the layer view.
#[derive(Debug, Clone, Default)]
pub struct SourceLayer {
    /// stable id (used by the registry to merge/replace)
    pub id: String,
    /// display name
    pub name: String,
    /// semantic kind: rivers | water | landcover | buildings | boundary | rails
    pub kind: String,
    pub visible: bool,
    pub source: String,
    pub lines: Vec<SourceLine>,
    pub polys: Vec<SourcePoly>,
}

/// A provider of map layers. Implement this and register it in the
/// editor's layer registry.
pub trait LayerSource {
    /// human-readable name of the source (bundle file, feed URL, …)
    fn source_name(&self) -> String;
    /// enumerate every layer this source contributes
    fn build_layers(&self) -> Vec<SourceLayer>;
}

/// Registry of layer sources — the trait-based front door of the layer
/// system. The map editor consumes `build_layers()` from every registered
/// source and merges the result into its renderable layer list.
#[derive(Default)]
pub struct LayerSourceRegistry {
    pub sources: Vec<Box<dyn LayerSource>>,
}

impl LayerSourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, source: Box<dyn LayerSource>) {
        log_sources(&*source);
        self.sources.push(source);
    }

    /// Every layer of every registered source, keyed by layer id.
    pub fn collect(&self) -> BTreeMap<String, SourceLayer> {
        let mut out = BTreeMap::new();
        for s in &self.sources {
            for l in s.build_layers() {
                out.insert(l.id.clone(), l);
            }
        }
        out
    }
}

fn log_sources(s: &dyn LayerSource) {
    // no log crate dependency in mapkit: stdout is fine for the CLI, the
    // engine side uses its own logging when consuming the layers
    let n = s.build_layers().len();
    println!("mapkit: layer source '{}' registered ({} layers)", s.source_name(), n);
}

/// A layer source backed by one loaded `.mapbin` bundle: emits a layer for
/// every layer blob it contains (rivers, water, landcover, buildings,
/// boundary, rails).
pub struct BundleLayerSource {
    pub label: String,
    pub bundle: Bundle,
}

impl BundleLayerSource {
    pub fn new(label: impl Into<String>, bundle: Bundle) -> Self {
        BundleLayerSource {
            label: label.into(),
            bundle,
        }
    }
}

impl LayerSource for BundleLayerSource {
    fn source_name(&self) -> String {
        self.label.clone()
    }

    fn build_layers(&self) -> Vec<SourceLayer> {
        let b = &self.bundle;
        let mut out = Vec::new();
        let meta_name = b.meta.name.clone();

        if let Some(txt) = b.layer_text(crate::bundle::LAYER_RIVERS) {
            out.push(SourceLayer {
                id: "rivers".into(),
                name: "Rivers".into(),
                kind: "rivers".into(),
                visible: true,
                source: self.label.clone(),
                lines: formats::parse_lines(&txt)
                    .into_iter()
                    .map(|(kind, pts)| SourceLine { kind, pts })
                    .collect(),
                polys: vec![],
            });
        }
        if let Some(txt) = b.layer_text(crate::bundle::LAYER_WATER) {
            out.push(SourceLayer {
                id: "water".into(),
                name: "Lakes / water".into(),
                kind: "water".into(),
                visible: false,
                source: self.label.clone(),
                lines: vec![],
                polys: formats::parse_tagged_rings(&txt, false)
                    .into_iter()
                    .map(|(kind, ring)| SourcePoly { kind, ring, height: 0.0 })
                    .collect(),
            });
        }
        if let Some(txt) = b.layer_text(crate::bundle::LAYER_LANDCOVER) {
            out.push(SourceLayer {
                id: "landcover".into(),
                name: "Vegetation & land use".into(),
                kind: "landcover".into(),
                visible: false,
                source: self.label.clone(),
                lines: vec![],
                polys: formats::parse_tagged_rings(&txt, true)
                    .into_iter()
                    .map(|(kind, ring)| SourcePoly { kind, ring, height: 0.0 })
                    .collect(),
            });
        }
        if let Some(txt) = b.layer_text(crate::bundle::LAYER_BUILDINGS) {
            out.push(SourceLayer {
                id: "buildings".into(),
                name: "Building footprints".into(),
                kind: "buildings".into(),
                visible: false,
                source: self.label.clone(),
                lines: vec![],
                polys: formats::parse_buildings(&txt)
                    .into_iter()
                    .map(|(height, ring)| SourcePoly {
                        kind: "building".into(),
                        ring,
                        height,
                    })
                    .collect(),
            });
        }
        if let Some(txt) = b.layer_text(crate::bundle::LAYER_BOUNDARY) {
            if let Some((label, ring)) = formats::parse_boundary(&txt) {
                out.push(SourceLayer {
                    id: "boundary".into(),
                    name: format!("Boundary — {} {}", meta_name, b.meta.kind),
                    kind: "boundary".into(),
                    visible: true,
                    source: self.label.clone(),
                    lines: vec![SourceLine { kind: label, pts: ring }],
                    polys: vec![],
                });
            }
        }
        if let Some(txt) = b.layer_text(crate::bundle::LAYER_RAILS) {
            out.push(SourceLayer {
                id: "rails".into(),
                name: "Railways".into(),
                kind: "rails".into(),
                visible: true,
                source: self.label.clone(),
                lines: formats::parse_lines(&txt)
                    .into_iter()
                    .map(|(kind, pts)| SourceLine { kind, pts })
                    .collect(),
                polys: vec![],
            });
        }
        out
    }
}
