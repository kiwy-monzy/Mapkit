//! API docs — /openapi.json (exactly the real routes, nothing invented)
//! and /swagger-ui (a tiny self-contained docs page, no CDN, offline-safe).

use rocket::get;
use rocket::serde::json::Json;
use serde_json::json;

/// GET /openapi.json — OpenAPI 3.0.3 spec of this exact server.
#[get("/openapi.json")]
pub fn openapi() -> Json<serde_json::Value> {
    Json(spec())
}

pub fn spec() -> serde_json::Value {
    let ok = json!({"description": "ok"});
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "mapkit-server",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Rust map data plane for Egregoria — XYZ tile streaming (basemap / vector / terrain), zone registry, zone grabbing. Replaces the Next.js /api/mapkit service."
        },
        "paths": {
            "/health": {
                "get": {
                    "summary": "Liveness probe (consumed by the native app)",
                    "responses": {"200": {"description": "service status, zone + tile counts", "content": {"application/json": {}}}}
                }
            },
            "/api/collections": {
                "get": {
                    "summary": "List registered zones (synced from assets/collections/*.mapbin)",
                    "responses": {"200": ok}
                }
            },
            "/api/collections/grab": {
                "post": {
                    "summary": "Grab a drawn polygon or admin zone into a new .mapbin collection (runs in-process, blocking, up to 20 min)",
                    "requestBody": {
                        "required": true,
                        "content": {"application/json": {
                            "schema": {
                                "type": "object",
                                "required": ["name", "polygon"],
                                "properties": {
                                    "name": {"type": "string"},
                                    "polygon": {"type": "array", "minItems": 3, "items": {"type": "array", "minItems": 2, "maxItems": 2, "items": {"type": "number"}}},
                                    "admin": {"type": "string", "description": "admin source query instead of a polygon (region:/district:/ward:…)"},
                                    "forceCity": {"type": "boolean", "default": false},
                                    "minClass": {"type": "string"},
                                    "roadClasses": {"type": "array", "items": {"type": "string"}},
                                    "waterClasses": {"type": "array", "items": {"type": "string"}},
                                    "waterwayClasses": {"type": "array", "items": {"type": "string"}},
                                    "landcoverClasses": {"type": "array", "items": {"type": "string"}},
                                    "includeBuildings": {"type": "boolean", "default": true},
                                    "includeRails": {"type": "boolean", "default": true},
                                    "includeWaterways": {"type": "boolean", "default": true},
                                    "includeBoundarySub": {"type": "boolean", "default": true}
                                }
                            }
                        }}
                    },
                    "responses": {"200": ok, "400": {"description": "invalid request"}, "500": {"description": "grabber failed"}}
                }
            },
            "/api/collections/preview": {
                "post": {
                    "summary": "Selectable preview: decode cached MVT tiles for a polygon into per-class GeoJSON (same data the grabber packs)",
                    "requestBody": {
                        "required": true,
                        "content": {"application/json": {
                            "schema": {
                                "type": "object",
                                "required": ["polygon"],
                                "properties": {
                                    "polygon": {"type": "array", "minItems": 3, "items": {"type": "array", "minItems": 2, "maxItems": 2, "items": {"type": "number"}}},
                                    "zoom": {"type": "integer", "minimum": 9, "maximum": 15}
                                }
                            }
                        }}
                    },
                    "responses": {"200": ok, "400": {"description": "invalid request"}}
                }
            },
            "/api/collections/layers": {
                "get": {
                    "summary": "One bundle's layers as GeoJSON with display colors (stride-sampled when dense)",
                    "parameters": [{"name": "file", "in": "query", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": ok, "404": {"description": "not found"}}
                }
            },
            "/api/tiles/prefetch": {
                "post": {
                    "summary": "Brush backend: fetch vector (z15+z14) + terrain (z14+z13+z12) tiles for a disc, with Terrarium backfill",
                    "requestBody": {
                        "required": true,
                        "content": {"application/json": {
                            "schema": {
                                "type": "object",
                                "required": ["lat", "lon"],
                                "properties": {
                                    "lat": {"type": "number"},
                                    "lon": {"type": "number"},
                                    "radiusM": {"type": "number", "default": 500},
                                    "kinds": {"type": "array", "items": {"type": "string"}}
                                }
                            }
                        }}
                    },
                    "responses": {"200": ok, "400": {"description": "invalid request"}}
                }
            },
            "/api/collections/download": {
                "get": {
                    "summary": "Download one .mapbin bundle",
                    "parameters": [{"name": "file", "in": "query", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "bundle bytes"}, "404": {"description": "not found"}}
                }
            },
            "/tiles/{z}/{x}/{y}": {
                "get": {
                    "summary": "XYZ raster basemap tile (?style=streets|navigation|satellite|satellite-streets|outdoors|light|dark, default streets; Mapbox style + matching Esri fallback; cached per style in tilecache/basemap)",
                    "parameters": basemap_params(),
                    "responses": {"200": {"description": "image/png or image/jpeg"}, "400": {"description": "bad coords or style"}, "504": {"description": "all sources failed"}}
                }
            },
            "/vector/{z}/{x}/{y}": {
                "get": {
                    "summary": "XYZ Mapbox streets-v8 vector tile (MVT; shares the grabber's cache files)",
                    "parameters": tile_params(),
                    "responses": {"200": {"description": "application/octet-stream MVT"}, "400": {"description": "bad coords"}}
                }
            },
            "/terrain/{z}/{x}/{y}": {
                "get": {
                    "summary": "XYZ Mapbox terrain-RGB DEM PNG (shares the grabber's cache files)",
                    "parameters": tile_params(),
                    "responses": {"200": {"description": "image/png"}, "400": {"description": "bad coords"}}
                }
            },
            "/api/admin/overview": {"get": {"summary": "Admin pack presence + ADM1/2/3 counts", "responses": {"200": ok}}},
            "/api/admin/regions": {"get": {"summary": "Every region (bbox, centre, district count)", "responses": {"200": ok}}},
            "/api/admin/districts": {"get": {"summary": "Districts of ?region= (ward counts)", "responses": {"200": ok}}},
            "/api/admin/wards": {"get": {"summary": "Wards of ?district= (&region= to disambiguate)", "responses": {"200": ok}}},
            "/api/admin/search": {"get": {"summary": "Fuzzy find across regions/districts/wards (?q=, max 50)", "responses": {"200": ok}}},
            "/api/admin/geometry": {"get": {"summary": "One zone's rings as GeoJSON (?query=region:/district:/ward:, &district=&region= scope, &maxPoints=)", "responses": {"200": ok}}},
            "/openapi.json": {"get": {"summary": "This spec", "responses": {"200": ok}}},
            "/swagger-ui": {"get": {"summary": "Human-readable API docs page", "responses": {"200": ok}}},
            "/docs": {"get": {"summary": "Server index + docs links (the Vite zone-picker UI lives at / when mapkit-web/dist is built)", "responses": {"200": ok}}},
            "/": {"get": {"summary": "Vite zone-picker UI (mapkit-web/dist/index.html) when built, else API index", "responses": {"200": ok}}}
        }
    })
}

fn tile_params() -> serde_json::Value {
    json!([
        {"name": "z", "in": "path", "required": true, "schema": {"type": "integer"}},
        {"name": "x", "in": "path", "required": true, "schema": {"type": "integer"}},
        {"name": "y", "in": "path", "required": true, "schema": {"type": "integer"}}
    ])
}

fn basemap_params() -> serde_json::Value {
    json!([
        {"name": "z", "in": "path", "required": true, "schema": {"type": "integer"}},
        {"name": "x", "in": "path", "required": true, "schema": {"type": "integer"}},
        {"name": "y", "in": "path", "required": true, "schema": {"type": "integer"}},
        {"name": "style", "in": "query", "required": false, "schema": {"type": "string", "enum": ["streets", "navigation", "satellite", "satellite-streets", "outdoors", "light", "dark"], "default": "streets"}}
    ])
}

/// GET /swagger-ui — self-contained docs page (fetches /openapi.json).
#[get("/swagger-ui")]
pub fn swagger() -> rocket::response::content::RawHtml<&'static str> {
    rocket::response::content::RawHtml(include_str!("swagger.html"))
}

/// GET /docs — tiny index pointing at the docs + health.
/// (The Vite zone-picker UI lives at `/` when `mapkit-web/dist` is built;
/// this page is the API-only fallback index.)
#[get("/docs")]
pub fn index() -> rocket::response::content::RawHtml<&'static str> {
    rocket::response::content::RawHtml(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>mapkit-server</title></head>
<body style="font-family: monospace; max-width: 40em; margin: 3em auto; line-height: 1.6">
<h1>mapkit-server</h1>
<p>Rust map data plane for the Egregoria map editor.</p>
<ul>
<li><a href="/">/</a> — zone-picker web UI (Vite build, when present)</li>
<li><a href="/swagger-ui">/swagger-ui</a> — API docs</li>
<li><a href="/openapi.json">/openapi.json</a> — OpenAPI 3 spec</li>
<li><a href="/health">/health</a> — liveness + counts</li>
</ul>
<p>XYZ tiles: <code>/tiles/{z}/{x}/{y}</code> (basemap) ·
<code>/vector/{z}/{x}/{y}</code> (streets MVT) ·
<code>/terrain/{z}/{x}/{y}</code> (terrain-RGB DEM)</p>
<p>Selectable preview: <code>POST /api/collections/preview</code> →
per-class GeoJSON → <code>POST /api/collections/grab</code> with allow-lists.</p>
</body></html>"#,
    )
}
