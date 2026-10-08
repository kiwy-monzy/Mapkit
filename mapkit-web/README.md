# mapkit-web — Vite + Leaflet zone picker (served by mapkit-server at `/`)

Remake of the old Next.js mapkit page (`map-canvas.tsx` + `mapkit-app.tsx`) as a
buildable Vite app whose `dist/` the Rocket server serves directly.

## Routes it talks to (same origin, no `/api/mapkit` prefix anymore)

| Call | Server |
|---|---|
| `GET /tiles/{z}/{x}/{y}?style=` | `mapkit::server::tiles::raster` (7 styles in the picker) |
| `POST /api/collections/preview` | per-class GeoJSON decoded from the cached MVT tiles |
| `POST /api/collections/grab` | allow-lists + toggles → `mapkit-cli` flags |
| `GET /api/collections/layers?file=` | one bundle's layers as GeoJSON with engine colors |
| `GET /api/admin/regions|districts|wards|search|geometry` | full Tanzania ADM1/2/3 pack |
| `GET /api/collections`, `GET /api/collections/download?file=`, `GET /health` | as before |

## Zone sources: Draw + Admin zones tabs

* **Draw**: click corners on the map → `POST /api/collections/preview {polygon}`
  → per-class checkboxes → `POST /api/collections/grab {name, polygon, …}`.
* **Admin zones** (free, data-driven — no hardcoded names): the tab loads the
  full Tanzania pack live from the server —
  `GET /api/admin/regions` (30 regions + district counts) →
  `GET /api/admin/districts?region=` → `GET /api/admin/wards?district=&region=`,
  plus `GET /api/admin/search?q=` across all 3644 wards. **Preview boundary**
  draws `GET /api/admin/geometry?query=` (canonical `region:`/`district:`/
  `ward:` query) and derives a preview polygon for the per-class checkboxes;
  export sends `{name, admin}` and the server grabs with `mapkit-cli --admin`
  at full boundary resolution (including district/ward sub-boundaries).

## Selectable preview → grab contract

Preview returns `layers.{roads,water,waterways,landcover,buildings,rails}` as
GeoJSON plus `classes.{roads,water,waterways,landcover}` histograms. Every class
renders as a checkbox; only checked classes are sent back:

* `roadClasses / waterClasses / waterwayClasses / landcoverClasses` — omitted
  (or all-selected) means keep-all; a shorter array becomes the grabber's
  `--road-classes`, `--water-classes`, `--waterway-classes`,
  `--landcover-classes` csv allow-lists (`mapkit::grab::GrabSelection`).
* `includeBuildings / includeRails / includeWaterways / includeBoundarySub`
  map to `--no-buildings`, `--no-rails`, `--no-waterways`, `--no-boundary-sub`.

## Build

```bash
npm --prefix mapkit-web install
npm --prefix mapkit-web run build   # → mapkit-web/dist/ (committed)
```

`npm run dev` proxies `/tiles /vector /terrain /api /health` to
`http://127.0.0.1:5150` for local work. The server logs whether `dist/` was
found; without it `/` falls back to the API index at `/docs`.
