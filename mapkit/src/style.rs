//! Basemap raster styles — the selectable XYZ looks for the 2D map view
//! and the 3D terrain drape.
//!
//! Every style is a Mapbox style id fetched with the grabber token, with a
//! matching Esri fallback (OSM/CARTO block some datacenter IPs). Served at
//! `GET /tiles/<z>/<x>/<y>?style=<id>`, cached per style as
//! `tilecache/basemap/{id}_{z}_{x}_{y}.png`.

/// A selectable basemap look. `Streets` is the default.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BasemapStyle {
    Streets,
    NavigationNight,
    Satellite,
    SatelliteStreets,
    Outdoors,
    Light,
    Dark,
}

impl BasemapStyle {
    /// all styles, in picker order
    pub fn all() -> [BasemapStyle; 7] {
        [
            BasemapStyle::Streets,
            BasemapStyle::NavigationNight,
            BasemapStyle::Satellite,
            BasemapStyle::SatelliteStreets,
            BasemapStyle::Outdoors,
            BasemapStyle::Light,
            BasemapStyle::Dark,
        ]
    }

    /// URL slug + tilecache filename prefix
    pub fn id(self) -> &'static str {
        match self {
            BasemapStyle::Streets => "streets",
            BasemapStyle::NavigationNight => "navigation",
            BasemapStyle::Satellite => "satellite",
            BasemapStyle::SatelliteStreets => "satellite-streets",
            BasemapStyle::Outdoors => "outdoors",
            BasemapStyle::Light => "light",
            BasemapStyle::Dark => "dark",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BasemapStyle::Streets => "Streets",
            BasemapStyle::NavigationNight => "Navigation night",
            BasemapStyle::Satellite => "Satellite",
            BasemapStyle::SatelliteStreets => "Satellite + streets",
            BasemapStyle::Outdoors => "Outdoors",
            BasemapStyle::Light => "Light",
            BasemapStyle::Dark => "Dark",
        }
    }

    /// Mapbox style id for the raster tile endpoint
    pub fn mapbox_style(self) -> &'static str {
        match self {
            BasemapStyle::Streets => "streets-v12",
            BasemapStyle::NavigationNight => "navigation-night-v1",
            BasemapStyle::Satellite => "satellite-v9",
            BasemapStyle::SatelliteStreets => "satellite-streets-v12",
            BasemapStyle::Outdoors => "outdoors-v12",
            BasemapStyle::Light => "light-v11",
            BasemapStyle::Dark => "dark-v11",
        }
    }

    /// Esri fallback matching the look (no token needed)
    pub fn esri_url(self, z: u32, x: u32, y: u32) -> String {
        // Esri tile order is z/y/x
        let svc = match self {
            BasemapStyle::Streets | BasemapStyle::Outdoors | BasemapStyle::Light => {
                "World_Street_Map"
            }
            BasemapStyle::Satellite | BasemapStyle::SatelliteStreets => "World_Imagery",
            BasemapStyle::NavigationNight | BasemapStyle::Dark => "Canvas/World_Dark_Gray_Base",
        };
        format!("https://server.arcgisonline.com/ArcGIS/rest/services/{svc}/MapServer/tile/{z}/{y}/{x}")
    }

    /// Esri flavour name for tile-index bookkeeping
    pub fn esri_kind(self) -> &'static str {
        match self {
            BasemapStyle::Streets | BasemapStyle::Outdoors | BasemapStyle::Light => "street",
            BasemapStyle::Satellite | BasemapStyle::SatelliteStreets => "imagery",
            BasemapStyle::NavigationNight | BasemapStyle::Dark => "esri",
        }
    }

    pub fn parse(s: &str) -> Option<BasemapStyle> {
        BasemapStyle::all()
            .into_iter()
            .find(|v| v.id() == s.to_lowercase())
    }
}

impl Default for BasemapStyle {
    fn default() -> Self {
        BasemapStyle::Streets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse_back() {
        for s in BasemapStyle::all() {
            assert_eq!(BasemapStyle::parse(s.id()), Some(s));
        }
        assert_eq!(BasemapStyle::parse("STREETS"), Some(BasemapStyle::Streets));
        assert_eq!(BasemapStyle::parse("navigation"), Some(BasemapStyle::NavigationNight));
        assert_eq!(BasemapStyle::parse("nope"), None);
    }
}
