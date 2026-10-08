import L from "leaflet";
import "leaflet/dist/leaflet.css";
import "./style.css";

type LngLat = [number, number]; // [lng, lat]

const STYLES = [
  "streets",
  "navigation",
  "satellite",
  "satellite-streets",
  "outdoors",
  "light",
  "dark",
] as const;

interface PreviewResponse {
  tiles: { z: number; x0: number; y0: number; x1: number; y1: number; hit: number };
  counts: Record<string, number>;
  classes: {
    roads: Record<string, number>;
    water: Record<string, number>;
    waterways: Record<string, number>;
    landcover: Record<string, number>;
  };
  buildingsIncomplete: boolean;
  layers: Record<string, GeoJSON.FeatureCollection>;
}

interface CollectionMeta {
  file: string;
  name: string;
  kind: string;
  district: string;
  region: string;
  centerLat: number;
  centerLon: number;
  spanM: number;
  roads: number;
  buildings: number;
  rivers: number;
  sizeBytes: number;
}

interface AdminZone {
  name: string;
  kind: string;
  district: string;
  region: string;
  bbox: [number, number, number, number];
  centerLat: number;
  centerLon: number;
  districts?: number;
  wards?: number;
}

// ── state ────────────────────────────────────────────────────────────────
let drawMode = false;
let draft: LngLat[] = [];
let polygon: LngLat[] | null = null;
let preview: PreviewResponse | null = null;
// admin-zone mode: canonical grab query ("region:X"/"district:Y"/"ward:Z").
// When set (and the admin pane is visible), export sends {name, admin} while
// `polygon` carries the boundary's outer ring for tile preview + map display.
let adminQuery: string | null = null;
// per-class selection: null = everything (pre-preview), else explicit sets
let roadSel = new Set<string>();
let waterSel = new Set<string>();
let waterwaySel = new Set<string>();
let landcoverSel = new Set<string>();

const $ = (id: string) => document.getElementById(id) as HTMLElement;
const statusEl = $("status");
const setStatus = (s: string) => (statusEl.textContent = s);

function polygonAreaM2(poly: LngLat[]): number {
  const R = 6378137;
  let s = 0;
  for (let i = 0; i < poly.length; i++) {
    const [lon1, lat1] = poly[i];
    const [lon2, lat2] = poly[(i + 1) % poly.length];
    const x1 = (lon1 * Math.PI) / 180;
    const x2 = (lon2 * Math.PI) / 180;
    s += (x2 - x1) * (2 + Math.sin((lat1 * Math.PI) / 180) + Math.sin((lat2 * Math.PI) / 180));
  }
  return Math.abs((s * R * R) / 2);
}
const fmtBytes = (n: number) =>
  n > 1e6 ? `${(n / 1e6).toFixed(1)} MB` : n > 1e3 ? `${(n / 1e3).toFixed(0)} KB` : `${n} B`;

// ── map ──────────────────────────────────────────────────────────────────
const map = L.map("map", { zoomControl: true }).setView([-6.79, 39.21], 12);
let tileLayer = L.tileLayer("/tiles/{z}/{x}/{y}?style=streets", {
  maxZoom: 19,
  tileSize: 512,
  zoomOffset: -1,
  attribution: "&copy; OpenStreetMap contributors &copy; Mapbox",
}).addTo(map);

const zoneLayer = L.layerGroup().addTo(map);
const draftLayer = L.layerGroup().addTo(map);
const previewGroup = L.layerGroup().addTo(map);

const styleSel = $("style") as HTMLSelectElement;
for (const s of STYLES) {
  const o = document.createElement("option");
  o.value = s;
  o.textContent = s;
  styleSel.appendChild(o);
}
styleSel.onchange = () => {
  map.removeLayer(tileLayer);
  tileLayer = L.tileLayer(`/tiles/{z}/{x}/{y}?style=${styleSel.value}`, {
    maxZoom: 19,
    tileSize: 512,
    zoomOffset: -1,
    attribution: "&copy; OpenStreetMap contributors &copy; Mapbox",
  }).addTo(map);
};

map.on("click", (e: L.LeafletMouseEvent) => {
  if (!drawMode) return;
  draft.push([e.latlng.lng, e.latlng.lat]);
  renderDraft();
  ($("finishBtn") as HTMLButtonElement).disabled = draft.length < 3;
  ($("undoBtn") as HTMLButtonElement).disabled = draft.length === 0;
  setStatus(`drawing — ${draft.length} point${draft.length === 1 ? "" : "s"} placed (click the map)`);
});

function renderDraft() {
  draftLayer.clearLayers();
  if (draft.length === 0) return;
  const ll = draft.map(([lng, lat]) => [lat, lng]) as L.LatLngExpression[];
  if (draft.length >= 2)
    L.polyline(ll, { color: "#fbbf24", weight: 2, dashArray: "6 6" }).addTo(draftLayer);
  for (const [lng, lat] of draft)
    L.circleMarker([lat, lng], { radius: 5, color: "#fbbf24", fillColor: "#fbbf24", fillOpacity: 0.9 }).addTo(draftLayer);
}

function renderZone(fit = true) {
  zoneLayer.clearLayers();
  if (!polygon || polygon.length < 3) return;
  const ll = polygon.map(([lng, lat]) => [lat, lng]) as L.LatLngExpression[];
  L.polygon(ll, { color: "#fbbf24", weight: 2.5, fillColor: "#fbbf24", fillOpacity: 0.08 }).addTo(zoneLayer);
  if (fit) map.fitBounds(L.polygon(ll).getBounds().pad(0.15));
}

// ── preview rendering: one GeoJSON sublayer per coarse group, filtered ───
const ROAD_STYLE: Record<string, { color: string; weight: number }> = {
  motorway: { color: "#fb923c", weight: 4 },
  trunk: { color: "#fb923c", weight: 3.5 },
  primary: { color: "#facc15", weight: 3 },
  secondary: { color: "#e7e5e4", weight: 2.5 },
  tertiary: { color: "#d6d3d1", weight: 2 },
};
const roadStyle = (f?: GeoJSON.Feature) => {
  const c = (f?.properties?.["class"] as string) ?? "";
  return ROAD_STYLE[c] ?? { color: "#a8a29e", weight: 1.5 };
};

function filtered(fc: GeoJSON.FeatureCollection, sel: Set<string> | null): GeoJSON.FeatureCollection {
  if (!sel) return fc;
  return {
    type: "FeatureCollection",
    features: fc.features.filter((f) => sel.has(String((f.properties ?? {} as Record<string, unknown>)["class"] ?? ""))),
  };
}

function renderPreview() {
  previewGroup.clearLayers();
  if (!preview) return;
  const L2 = preview.layers;
  if (roadSel.size || !preview.classes.roads || Object.keys(preview.classes.roads).length === 0) {
    const fc = filtered(L2.roads, preview.classes.roads ? roadSel : null);
    if (fc.features.length) L.geoJSON(fc, { style: roadStyle }).addTo(previewGroup);
  }
  const rails = L2.rails;
  if ((($("incRails") as HTMLInputElement).checked) && rails?.features.length)
    L.geoJSON(rails, { style: { color: "#78787f", weight: 2, dashArray: "4 4" } }).addTo(previewGroup);
  if (waterSel.size) {
    const fc = filtered(L2.water, waterSel);
    if (fc.features.length)
      L.geoJSON(fc, { style: { color: "#60a5fa", weight: 2, fillColor: "#3b82f6", fillOpacity: 0.4 } }).addTo(previewGroup);
  }
  const showWaterways = ($("incWaterways") as HTMLInputElement).checked;
  if (showWaterways && waterwaySel.size) {
    const fc = filtered(L2.waterways, waterwaySel);
    if (fc.features.length) L.geoJSON(fc, { style: { color: "#38bdf8", weight: 2 } }).addTo(previewGroup);
  }
  if (landcoverSel.size) {
    const fc = filtered(L2.landcover, landcoverSel);
    if (fc.features.length)
      L.geoJSON(fc, { style: { color: "#4ade80", weight: 1, fillColor: "#22c55e", fillOpacity: 0.3 } }).addTo(previewGroup);
  }
  if (($("incBuildings") as HTMLInputElement).checked && L2.buildings?.features.length)
    L.geoJSON(L2.buildings, {
      style: { color: "#a8a29e", weight: 1, fillColor: "#d6d3d1", fillOpacity: 0.45 },
    }).addTo(previewGroup);
}

// checkbox panel: every fetched class becomes selectable
function renderLayerPanel() {
  const box = $("layers");
  box.innerHTML = "";
  if (!preview) {
    box.innerHTML = `<p class="muted">After closing a zone, fetch its data — every road/water/landcover class from the cached vector tiles becomes a checkbox. Only checked classes are packed on export.</p>`;
    ($("selBar") as HTMLElement).hidden = true;
    return;
  }
  ($("selBar") as HTMLElement).hidden = false;
  const groups: { title: string; classes: Record<string, number>; sel: Set<string>; onFlip: () => void }[] = [
    { title: "Roads & streets", classes: preview.classes.roads, sel: roadSel, onFlip: renderPreview },
    { title: "Water polygons", classes: preview.classes.water, sel: waterSel, onFlip: renderPreview },
    { title: "Rivers / waterways", classes: preview.classes.waterways, sel: waterwaySel, onFlip: renderPreview },
    { title: "Vegetation & land use", classes: preview.classes.landcover, sel: landcoverSel, onFlip: renderPreview },
  ];
  for (const g of groups) {
    const entries = Object.entries(g.classes).sort((a, b) => b[1] - a[1]);
    if (!entries.length) continue;
    const div = document.createElement("div");
    div.className = "layergroup";
    const head = document.createElement("div");
    head.className = "ghead";
    head.innerHTML = `<span>${g.title}</span><span class="badge">${entries.reduce((a, [, n]) => a + n, 0)}</span>`;
    div.appendChild(head);
    for (const [cls, n] of entries) {
      const row = document.createElement("div");
      row.className = "cls";
      const label = document.createElement("label");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = g.sel.has(cls);
      cb.onchange = () => {
        if (cb.checked) g.sel.add(cls);
        else g.sel.delete(cls);
        g.onFlip();
      };
      label.appendChild(cb);
      label.appendChild(document.createTextNode(cls || "(unclassified)"));
      row.appendChild(label);
      const badge = document.createElement("span");
      badge.className = "badge";
      badge.textContent = String(n);
      row.appendChild(badge);
      div.appendChild(row);
    }
    box.appendChild(div);
  }
  const info = document.createElement("p");
  info.className = "muted";
  const extra = [
    `${preview.counts.buildings ?? 0} buildings`,
    `${preview.counts.rails ?? 0} rails`,
    `tiles z${preview.tiles.z} (${preview.tiles.hit} hit)`,
  ].join(" · ");
  info.textContent = extra + (preview.buildingsIncomplete ? " · buildings partial (zone too wide for z15 backfill — zoom in for full footprints)" : "");
  box.appendChild(info);
}

$("allBtn").onclick = () => {
  if (!preview) return;
  for (const k of Object.keys(preview.classes.roads)) roadSel.add(k);
  for (const k of Object.keys(preview.classes.water)) waterSel.add(k);
  for (const k of Object.keys(preview.classes.waterways)) waterwaySel.add(k);
  for (const k of Object.keys(preview.classes.landcover)) landcoverSel.add(k);
  renderLayerPanel();
  renderPreview();
};
$("noneBtn").onclick = () => {
  roadSel.clear(); waterSel.clear(); waterwaySel.clear(); landcoverSel.clear();
  renderLayerPanel();
  renderPreview();
};
for (const id of ["incRails", "incWaterways", "incBuildings"]) {
  $(id).addEventListener("change", renderPreview);
}

// ── toolbar ──────────────────────────────────────────────────────────────
$("drawBtn").onclick = () => {
  drawMode = true;
  draft = [];
  polygon = null;
  polygonFromBrush = false;
  brushDabsAccum = [];
  preview = null;
  adminQuery = null;
  setTab(false);
  zoneLayer.clearLayers();
  previewGroup.clearLayers();
  renderLayerPanel();
  ($("drawBtn") as HTMLElement).style.display = "none";
  ($("drawCtl") as HTMLElement).hidden = false;
  ($("drawHint") as HTMLElement).hidden = false;
  ($("previewBtn") as HTMLButtonElement).disabled = true;
  ($("exportBtn") as HTMLButtonElement).disabled = true;
  ($("zoneInfo") as HTMLElement).textContent = "Drawing… click the map to place corners.";
  setStatus("drawing — click the map to place zone corners");
  map.getContainer().style.cursor = "crosshair";
};
const endDrawUI = () => {
  drawMode = false;
  ($("drawBtn") as HTMLElement).style.display = "";
  ($("drawCtl") as HTMLElement).hidden = true;
  ($("drawHint") as HTMLElement).hidden = true;
  map.getContainer().style.cursor = "";
};
$("finishBtn").onclick = () => {
  if (draft.length < 3) return;
  polygon = draft;
  adminQuery = null;
  draft = [];
  renderDraft();
  renderZone();
  endDrawUI();
  const area = polygonAreaM2(polygon);
  ($("zoneInfo") as HTMLElement).textContent =
    `${polygon.length} corners · ${(area / 1e6).toFixed(2)} km²`;
  ($("previewBtn") as HTMLButtonElement).disabled = false;
  ($("exportBtn") as HTMLButtonElement).disabled = false;
  setStatus(`zone ready · ${(area / 1e6).toFixed(2)} km²`);
};
$("undoBtn").onclick = () => {
  draft = draft.slice(0, -1);
  renderDraft();
  ($("finishBtn") as HTMLButtonElement).disabled = draft.length < 3;
  ($("undoBtn") as HTMLButtonElement).disabled = draft.length === 0;
};
$("cancelBtn").onclick = () => {
  draft = [];
  renderDraft();
  endDrawUI();
  setStatus('click "Draw zone" to trace a map area');
};

// ── preview + export ─────────────────────────────────────────────────────
$("previewBtn").onclick = async () => {
  if (!polygon) return;
  const btn = $("previewBtn") as HTMLButtonElement;
  btn.disabled = true;
  const label = btn.textContent;
  btn.textContent = "Fetching…";
  setStatus("fetching tile features for the zone…");
  try {
    const res = await fetch("/api/collections/preview", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ polygon }),
    });
    const json = await res.json();
    if (!res.ok) throw new Error(json.error ?? res.statusText);
    preview = json as PreviewResponse;
    roadSel = new Set(Object.keys(preview.classes.roads ?? {}));
    waterSel = new Set(Object.keys(preview.classes.water ?? {}));
    waterwaySel = new Set(Object.keys(preview.classes.waterways ?? {}));
    landcoverSel = new Set(Object.keys(preview.classes.landcover ?? {}));
    renderLayerPanel();
    renderPreview();
    const c = preview.counts;
    setStatus(`preview: ${c.roads} roads · ${c.water} water · ${c.landcover} vegetation · ${c.buildings} buildings · ${c.rails} rails`);
  } catch (e) {
    setStatus(`preview failed: ${String(e)}`);
  } finally {
    btn.disabled = false;
    btn.textContent = label;
  }
};

$("exportBtn").onclick = async () => {
  const useAdmin = adminQuery && !($("adminPane") as HTMLElement).hidden;
  if (!polygon && !useAdmin) return;
  const name = (($("zoneName") as HTMLInputElement).value ?? "").trim();
  if (!name) {
    setStatus("name your zone before exporting");
    return;
  }
  const btn = $("exportBtn") as HTMLButtonElement;
  btn.disabled = true;
  const log = $("exportLog") as HTMLElement;
  log.hidden = true;
  setStatus("grabbing Mapbox tiles + DEM — this can take a few minutes…");
  // Only send allow-lists when the user actually deselected something;
  // omitted = keep-all on the server (old behaviour).
  const all = (sel: Set<string>, classes: Record<string, number> | undefined) =>
    !classes || sel.size === Object.keys(classes).length ? undefined : [...sel];
  try {
    const body: Record<string, unknown> = {
      name,
      forceCity: ($("forceCity") as HTMLInputElement).checked,
      minClass: (($("minClass") as HTMLSelectElement).value || undefined),
      roadClasses: all(roadSel, preview?.classes.roads),
      waterClasses: all(waterSel, preview?.classes.water),
      waterwayClasses: all(waterwaySel, preview?.classes.waterways),
      landcoverClasses: all(landcoverSel, preview?.classes.landcover),
      includeBuildings: ($("incBuildings") as HTMLInputElement).checked,
      includeRails: ($("incRails") as HTMLInputElement).checked,
      includeWaterways: ($("incWaterways") as HTMLInputElement).checked,
      includeBoundarySub: ($("incBoundary") as HTMLInputElement).checked,
    };
    if (useAdmin) body["admin"] = adminQuery;
    else body["polygon"] = polygon;
    const res = await fetch("/api/collections/grab", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    const json = await res.json();
    if (!res.ok) throw new Error(json.error ?? res.statusText);
    setStatus(`saved ${json.file} — ${json.meta?.roads ?? 0} roads, ${json.meta?.buildings ?? 0} buildings. Load it in Collections.`);
    refreshCollections();
  } catch (e) {
    log.textContent = String(e);
    log.hidden = false;
    setStatus(`export failed: ${String(e)}`);
  } finally {
    btn.disabled = false;
  }
};

// ── collections ──────────────────────────────────────────────────────────
const collectionGroup = L.layerGroup().addTo(map);
let shownLayersFor: string | null = null;

interface LayersResponse {
  file: string;
  meta: CollectionMeta & { centerLat: number; centerLon: number; spanM: number };
  layers: Record<string, { type: string; features?: GeoJSON.Feature[]; total?: number; truncated?: boolean; style?: Record<string, unknown>; stats?: Record<string, unknown> }>;
}

const LAYER_ORDER = ["roads", "rivers", "rails", "water", "landcover", "buildings", "boundary"] as const;

function fitCollection(c: { centerLat: number; centerLon: number; spanM: number }) {
  const half = Math.max(c.spanM, 500) / 2;
  const dLat = half / 111320;
  const dLng = half / (111320 * Math.cos((c.centerLat * Math.PI) / 180));
  map.fitBounds([
    [c.centerLat - dLat, c.centerLon - dLng],
    [c.centerLat + dLat, c.centerLon + dLng],
  ]);
}

function styleOf(f?: GeoJSON.Feature): L.PathOptions {
  const p = ((f?.properties ?? {}) as Record<string, unknown>);
  const o: L.PathOptions = {
    color: (p["color"] as string) ?? "#a8a29e",
    weight: (p["weight"] as number) ?? 2,
  };
  if (f && (f.geometry.type === "Polygon" || f.geometry.type === "MultiPolygon")) {
    o.fillColor = (p["fill"] as string) ?? o.color;
    o.fillOpacity = (p["fillOpacity"] as number) ?? 0.4;
  }
  if (p["dashArray"]) o.dashArray = p["dashArray"] as string;
  return o;
}

async function toggleCollectionLayers(file: string, panel: HTMLElement, btn: HTMLButtonElement) {
  if (shownLayersFor === file) {
    collectionGroup.clearLayers();
    shownLayersFor = null;
    panel.innerHTML = "";
    btn.textContent = "Layers";
    return;
  }
  // one collection's layers at a time — reset other cards' panels/buttons
  document.querySelectorAll(".layerpanel").forEach((p) => {
    if (p !== panel) p.innerHTML = "";
  });
  document.querySelectorAll(".layers-btn").forEach((b) => {
    if (b !== btn) (b as HTMLButtonElement).textContent = "Layers";
  });
  btn.textContent = "Loading…";
  setStatus(`loading collection layers: ${file} (big bundles take a while)…`);
  try {
    const res = await fetch(`/api/collections/layers?file=${encodeURIComponent(file)}`);
    const j = (await res.json()) as LayersResponse;
    if (!res.ok) throw new Error((j as unknown as { error?: string }).error ?? res.statusText);
    collectionGroup.clearLayers();
    shownLayersFor = file;
    panel.innerHTML = "";
    for (const key of LAYER_ORDER) {
      const lyr = j.layers[key];
      if (!lyr || !lyr.features || !lyr.features.length) continue;
      const label = (lyr.style?.["label"] as string) ?? key;
      const total = lyr.total ?? lyr.features.length;
      const count = lyr.truncated ? `~${total}` : `${total}`;
      const fc: GeoJSON.FeatureCollection = { type: "FeatureCollection", features: lyr.features };
      const g = L.geoJSON(fc, { style: styleOf });
      g.addTo(collectionGroup);
      const row = document.createElement("label");
      row.className = "cls";
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = key !== "buildings" && key !== "landcover";
      if (!cb.checked) collectionGroup.removeLayer(g);
      cb.onchange = () => {
        if (cb.checked) g.addTo(collectionGroup);
        else collectionGroup.removeLayer(g);
      };
      const dot = document.createElement("span");
      dot.className = "swatch";
      const first = lyr.features[0]?.properties as Record<string, unknown> | undefined;
      dot.style.background = (first?.["fill"] as string) ?? (first?.["color"] as string) ?? "#888";
      row.appendChild(cb);
      row.appendChild(dot);
      row.appendChild(document.createTextNode(`${label} `));
      const badge = document.createElement("span");
      badge.className = "badge";
      badge.textContent = count;
      row.appendChild(badge);
      panel.appendChild(row);
    }
    const terr = j.layers["terrain"]?.stats as Record<string, unknown> | undefined;
    if (terr && !terr["error"]) {
      const t = document.createElement("p");
      t.className = "muted";
      t.textContent = `DEM ${(terr["cells"] as number[] ?? []).join("×")} @ ${terr["cellM"]}m · ${Number(terr["minM"]).toFixed(0)}…${Number(terr["maxM"]).toFixed(0)}m`;
      panel.appendChild(t);
    }
    fitCollection(j.meta);
    btn.textContent = "Hide layers";
    setStatus(`collection layers: ${file} — toggles above, colors match the engine`);
  } catch (e) {
    btn.textContent = "Layers";
    setStatus(`layers failed: ${String(e)}`);
  }
}

async function refreshCollections() {
  try {
    const res = await fetch("/api/collections");
    const json = await res.json();
    const cols: CollectionMeta[] = json.collections ?? [];
    const def: string | null = json.defaultCollection ?? null;
    ($("colCount") as HTMLElement).textContent = String(cols.length);
    const box = $("collections");
    box.innerHTML = "";
    if (!cols.length) box.innerHTML = `<p class="muted">No collections yet — the native app boots an empty terrain.</p>`;
    for (const c of cols) {
      const div = document.createElement("div");
      div.className = "col";
      const isDef = def === c.file.replace(/\.mapbin$/, "");
      div.innerHTML =
        `<div class="nm">${c.name} ${isDef ? '<span class="badge">default</span>' : ""} <span class="badge">${c.kind || "Zone"}</span></div>` +
        `<div class="meta">${c.roads} roads · ${c.buildings} buildings · ${(c.spanM / 1000).toFixed(1)} km window · ${fmtBytes(c.sizeBytes)}</div>`;
      const ops = document.createElement("div");
      ops.className = "ops";
      const view = document.createElement("button");
      view.className = "ghost sm";
      view.textContent = "View";
      const layersBtn = document.createElement("button");
      layersBtn.className = "ghost sm layers-btn";
      layersBtn.textContent = "Layers";
      const dl = document.createElement("a");
      dl.href = `/api/collections/download?file=${encodeURIComponent(c.file)}`;
      dl.textContent = "Download";
      (dl as HTMLAnchorElement).download = c.file;
      ops.appendChild(view);
      ops.appendChild(layersBtn);
      ops.appendChild(dl);
      div.appendChild(ops);
      const layerPanel = document.createElement("div");
      layerPanel.className = "layerpanel";
      div.appendChild(layerPanel);
      // View = fit the zone AND load its layers onto the map (one click shows
      // the collection); Layers toggles the overlay back off.
      view.onclick = () => {
        fitCollection(c);
        if (shownLayersFor !== c.file) toggleCollectionLayers(c.file, layerPanel, layersBtn);
      };
      layersBtn.onclick = () => toggleCollectionLayers(c.file, layerPanel, layersBtn);
      box.appendChild(div);
    }
  } catch {
    setStatus("collections unreachable — is mapkit-server running?");
  }
}
$("refreshBtn").onclick = refreshCollections;

// ── admin zones tab (free, data-driven: full Tanzania ADM1/2/3) ──────────
function setTab(admin: boolean) {
  ($("drawPane") as HTMLElement).hidden = admin;
  ($("adminPane") as HTMLElement).hidden = !admin;
  ($("tabDraw") as HTMLButtonElement).className = admin ? "ghost sm" : "sm";
  ($("tabAdmin") as HTMLButtonElement).className = admin ? "sm" : "ghost sm";
}
$("tabDraw").onclick = () => setTab(false);
$("tabAdmin").onclick = () => setTab(true);

const fillSel = (el: HTMLSelectElement, items: { value: string; label: string }[], placeholder: string) => {
  el.innerHTML = "";
  const ph = document.createElement("option");
  ph.value = "";
  ph.textContent = placeholder;
  el.appendChild(ph);
  for (const it of items) {
    const o = document.createElement("option");
    o.value = it.value;
    o.textContent = it.label;
    el.appendChild(o);
  }
};

async function loadRegions() {
  const sel = $("regionSel") as HTMLSelectElement;
  try {
    const r = await fetch("/api/admin/regions");
    const j = await r.json();
    const regs: AdminZone[] = j.regions ?? [];
    fillSel(sel, regs.map((z) => ({ value: z.name, label: `${z.name} (${z.districts ?? 0} districts)` })), "pick a region…");
    setStatus(`admin pack: ${regs.length} regions — pick one or search above`);
  } catch {
    fillSel(sel, [], "admin pack unavailable");
  }
}

async function onRegion() {
  const region = ($("regionSel") as HTMLSelectElement).value;
  const dsel = $("districtSel") as HTMLSelectElement;
  const wsel = $("wardSel") as HTMLSelectElement;
  fillSel(wsel, [], "pick a district first");
  wsel.disabled = true;
  ($("adminPreviewBtn") as HTMLButtonElement).disabled = true;
  ($("adminPreviewDataBtn") as HTMLButtonElement).disabled = true;
  if (!region) {
    fillSel(dsel, [], "pick a region first");
    dsel.disabled = true;
    return;
  }
  dsel.disabled = false;
  fillSel(dsel, [], "loading…");
  try {
    const r = await fetch(`/api/admin/districts?region=${encodeURIComponent(region)}`);
    const j = await r.json();
    const ds: AdminZone[] = j.districts ?? [];
    fillSel(dsel, ds.map((z) => ({ value: z.name, label: `${z.name} (${z.wards ?? 0} wards)` })), `whole region (${ds.length} districts)…`);
    setAdminInfo({ name: region, kind: "region", district: "", region, bbox: [0, 0, 0, 0], centerLat: 0, centerLon: 0 });
    armAdminPreview(`region:${region}`, region);
  } catch {
    fillSel(dsel, [], "load failed");
  }
}

async function onDistrict() {
  const region = ($("regionSel") as HTMLSelectElement).value;
  const district = ($("districtSel") as HTMLSelectElement).value;
  const wsel = $("wardSel") as HTMLSelectElement;
  if (!district) {
    // whole region selected
    if (region) armAdminPreview(`region:${region}`, region);
    fillSel(wsel, [], "pick a district first");
    wsel.disabled = true;
    return;
  }
  wsel.disabled = false;
  fillSel(wsel, [], "loading…");
  try {
    const r = await fetch(
      `/api/admin/wards?district=${encodeURIComponent(district)}&region=${encodeURIComponent(region)}`
    );
    const j = await r.json();
    const ws: AdminZone[] = j.wards ?? [];
    fillSel(wsel, ws.map((z) => ({ value: z.name, label: z.name })), `whole district (${ws.length} wards)…`);
    armAdminPreview(`district:${district}`, district);
  } catch {
    fillSel(wsel, [], "load failed");
  }
}

function onWard() {
  const region = ($("regionSel") as HTMLSelectElement).value;
  const district = ($("districtSel") as HTMLSelectElement).value;
  const ward = ($("wardSel") as HTMLSelectElement).value;
  if (ward) armAdminPreview(`ward:${ward}`, ward);
  else if (district) armAdminPreview(`district:${district}`, district);
  else if (region) armAdminPreview(`region:${region}`, region);
}

function setAdminInfo(z: AdminZone) {
  const parts = [z.kind];
  if (z.district) parts.push(`district ${z.district}`);
  if (z.region && z.kind !== "region") parts.push(`region ${z.region}`);
  ($("adminInfo") as HTMLElement).textContent = parts.join(" · ");
}

/** enable the boundary-preview buttons for a canonical admin query */
function armAdminPreview(query: string, displayName: string) {
  adminQuery = query;
  ($("adminPreviewBtn") as HTMLButtonElement).disabled = false;
  ($("adminPreviewDataBtn") as HTMLButtonElement).disabled = false;
  const zb = $("zoneName") as HTMLInputElement;
  if (!zb.value.trim()) zb.value = displayName;
  setStatus(`admin zone armed: ${query} — preview the boundary, then grab`);
}

/** draw the admin boundary, fit the map, derive a preview polygon */
async function previewAdminBoundary() {
  if (!adminQuery) return;
  const btn = $("adminPreviewBtn") as HTMLButtonElement;
  btn.disabled = true;
  setStatus(`fetching boundary for ${adminQuery}…`);
  try {
    // scope same-name zones to the cascade selection (two "Mbezi" wards)
    const region = ($("regionSel") as HTMLSelectElement).value;
    const district = ($("districtSel") as HTMLSelectElement).value;
    const params = new URLSearchParams({ query: adminQuery });
    if (district) params.set("district", district);
    if (region) params.set("region", region);
    const r = await fetch(`/api/admin/geometry?${params.toString()}`);
    const feat = await r.json();
    if (!r.ok) throw new Error(feat.error ?? r.statusText);
    zoneLayer.clearLayers();
    previewGroup.clearLayers();
    L.geoJSON(feat, {
      style: { color: "#fbbf24", weight: 2.5, fillColor: "#fbbf24", fillOpacity: 0.08 },
    }).addTo(zoneLayer);
    const bounds = L.geoJSON(feat).getBounds();
    if (bounds.isValid()) map.fitBounds(bounds.pad(0.15));
    // largest outer ring → preview polygon (grab itself uses --admin full-res)
    const polys = feat.geometry.coordinates as number[][][][];
    let best: number[][] = [];
    for (const p of polys) {
      const outer = p[0] ?? [];
      if (outer.length > best.length) best = outer;
    }
    const stride = Math.max(1, Math.ceil(best.length / 400));
    polygon = best.filter((_, i) => i % stride === 0 || i === best.length - 1).map(([lng, lat]) => [lng, lat] as LngLat);
    preview = null;
    renderLayerPanel();
    ($("exportBtn") as HTMLButtonElement).disabled = false;
    const p = feat.properties;
    setAdminInfo({ name: p.name, kind: p.kind, district: p.district, region: p.region, bbox: [0, 0, 0, 0], centerLat: 0, centerLon: 0 });
    setStatus(`boundary: ${p.name} (${p.kind}) — get map data for per-class preview, or export directly`);
  } catch (e) {
    setStatus(`boundary failed: ${String(e)}`);
  } finally {
    btn.disabled = false;
  }
}

let searchTimer: number | undefined;
async function runSearch() {
  const q = (($("adminSearch") as HTMLInputElement).value ?? "").trim();
  const box = $("searchHits");
  box.innerHTML = "";
  if (q.length < 2) return;
  try {
    const r = await fetch(`/api/admin/search?q=${encodeURIComponent(q)}`);
    const j = await r.json();
    const hits: AdminZone[] = j.hits ?? [];
    for (const h of hits.slice(0, 12)) {
      const b = document.createElement("button");
      b.className = "hit";
      const sub = h.kind === "ward" ? `${h.district} · ${h.region}` : h.kind === "district" ? h.region : `${h.districts ?? 0} districts`;
      b.innerHTML = `<span>${h.name}</span><small>${h.kind} · ${sub}</small>`;
      b.onclick = () => selectSearchHit(h);
      box.appendChild(b);
    }
    if (!hits.length) box.innerHTML = `<p class="muted">no matches</p>`;
  } catch {
    /* offline is fine */
  }
}

async function selectSearchHit(h: AdminZone) {
  // drive the cascading selects to the hit (region → district → ward)
  const rsel = $("regionSel") as HTMLSelectElement;
  const region = h.kind === "region" ? h.name : h.region;
  if (region && [...rsel.options].some((o) => o.value === region)) {
    rsel.value = region;
    await onRegion();
    if (h.kind !== "region") {
      const dsel = $("districtSel") as HTMLSelectElement;
      if ([...dsel.options].some((o) => o.value === h.district)) {
        dsel.value = h.district;
        await onDistrict();
        if (h.kind === "ward") {
          const wsel = $("wardSel") as HTMLSelectElement;
          if ([...wsel.options].some((o) => o.value === h.name)) {
            wsel.value = h.name;
            onWard();
          }
        }
      }
    }
  } else {
    armAdminPreview(`${h.kind}:${h.name}`, h.name);
  }
  ($("adminSearch") as HTMLInputElement).value = "";
  ($("searchHits") as HTMLElement).innerHTML = "";
  previewAdminBoundary();
}

$("regionSel").addEventListener("change", onRegion);
$("districtSel").addEventListener("change", onDistrict);
$("wardSel").addEventListener("change", onWard);
$("adminPreviewBtn").onclick = previewAdminBoundary;
$("adminPreviewDataBtn").onclick = () => ($("previewBtn") as HTMLButtonElement).disabled ? setStatus("preview the boundary first") : ($("previewBtn") as HTMLButtonElement).click();
$("adminSearch").addEventListener("input", () => {
  window.clearTimeout(searchTimer);
  searchTimer = window.setTimeout(runSearch, 250);
});

// ── brush: paint to fetch missing tiles (maptools-style, for the web) ────
// Drag on the map to brush a spot: the client sends the stroke's bounding
// disc to POST /api/tiles/prefetch (vector z15+z14 for buildings, terrain
// z14+z13+z12 + Terrarium backfill for DEM gaps). The disc pulses amber
// while fetching, turns green when cached, red on failure — then any active
// zone preview auto-refreshes so the new data appears as layers.
const brushGroup = L.layerGroup().addTo(map);
let brushMode = false;
let brushing = false;
let brushDabs: L.LatLng[] = [];
let brushDots: L.CircleMarker[] = [];
// the brush builds a REAL zone: every dab accumulates (with its radius) and
// the zone polygon is the convex hull of the dab discs — so "Get map data"
// and "Export" work on a brushed area exactly like on a drawn one
let brushDabsAccum: { lat: number; lng: number; r: number }[] = [];
let polygonFromBrush = false;

function brushZonePolygon(): LngLat[] {
  const lat0 = brushDabsAccum[0].lat;
  const lng0 = brushDabsAccum[0].lng;
  const MX = Math.cos((lat0 * Math.PI) / 180) * 111320;
  const MY = 110540;
  const pts: [number, number][] = [];
  for (const d of brushDabsAccum) {
    const cx = (d.lng - lng0) * MX;
    const cy = (d.lat - lat0) * MY;
    for (let i = 0; i < 16; i++) {
      const a = (i / 16) * 2 * Math.PI;
      pts.push([cx + d.r * Math.cos(a), cy + d.r * Math.sin(a)]);
    }
  }
  // Andrew monotone chain over the disc rims
  pts.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const cross = (o: [number, number], a: [number, number], b: [number, number]) =>
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
  const lower: [number, number][] = [];
  const upper: [number, number][] = [];
  for (const p of pts) {
    while (lower.length >= 2 && cross(lower[lower.length - 2], lower[lower.length - 1], p) <= 0) lower.pop();
    lower.push(p);
  }
  for (let i = pts.length - 1; i >= 0; i--) {
    const p = pts[i];
    while (upper.length >= 2 && cross(upper[upper.length - 2], upper[upper.length - 1], p) <= 0) upper.pop();
    upper.push(p);
  }
  const hull = lower.slice(0, -1).concat(upper.slice(0, -1));
  const step = Math.max(1, Math.ceil(hull.length / 160));
  const out: LngLat[] = [];
  for (let i = 0; i < hull.length; i += step)
    out.push([lng0 + hull[i][0] / MX, lat0 + hull[i][1] / MY]);
  return out;
}

const brushRadiusM = () => Number(($("brushRadius") as HTMLInputElement).value ?? 500);

$("brushBtn").onclick = () => {
  if (drawMode) return;
  brushMode = !brushMode;
  ($("brushBtn") as HTMLButtonElement).className = brushMode ? "" : "ghost";
  ($("brushRadius") as HTMLElement).hidden = !brushMode;
  if (brushMode) {
    map.dragging.disable();
    map.getContainer().style.cursor = "cell";
    setStatus(`brush on — drag to fetch missing tiles (radius ${brushRadiusM()} m)`);
  } else {
    map.dragging.enable();
    map.getContainer().style.cursor = "";
    setStatus("brush off");
  }
};

map.on("mousedown", (e: L.LeafletMouseEvent) => {
  if (!brushMode || drawMode) return;
  brushing = true;
  brushDabs = [e.latlng];
  brushDots = [];
  pushDab(e.latlng);
});

map.on("mousemove", (e: L.LeafletMouseEvent) => {
  if (!brushMode || !brushing || drawMode) return;
  const last = brushDabs[brushDabs.length - 1];
  if (map.distance(last, e.latlng) >= brushRadiusM() / 3) {
    brushDabs.push(e.latlng);
    pushDab(e.latlng);
  }
});

map.on("mouseup", () => {
  if (!brushMode || !brushing) return;
  brushing = false;
  finishStroke();
});

function pushDab(ll: L.LatLng) {
  const dot = L.circleMarker(ll, {
    radius: 6,
    color: "#fbbf24",
    fillColor: "#fbbf24",
    fillOpacity: 0.5,
    className: "brush-dab",
    interactive: false,
  }).addTo(brushGroup);
  brushDots.push(dot);
}

async function finishStroke() {
  for (const d of brushDots) brushGroup.removeLayer(d);
  brushDots = [];
  if (!brushDabs.length) return;
  // bounding disc of the stroke
  let lat = 0;
  let lng = 0;
  for (const d of brushDabs) {
    lat += d.lat;
    lng += d.lng;
  }
  lat /= brushDabs.length;
  lng /= brushDabs.length;
  const center = L.latLng(lat, lng);
  let r = brushRadiusM();
  for (const d of brushDabs) r = Math.max(r, center.distanceTo(d) + brushRadiusM());
  r = Math.min(r, 5000);
  // every stroke joins the brush zone: hull grows, preview + export follow
  for (const d of brushDabs) brushDabsAccum.push({ lat: d.lat, lng: d.lng, r: brushRadiusM() });
  if (!polygon || polygonFromBrush) {
    polygon = brushZonePolygon();
    polygonFromBrush = true;
    adminQuery = null;
    renderZone(false);
    const area = polygonAreaM2(polygon);
    ($("zoneInfo") as HTMLElement).textContent = `brush zone · ${(area / 1e6).toFixed(2)} km²`;
    ($("previewBtn") as HTMLButtonElement).disabled = false;
    ($("exportBtn") as HTMLButtonElement).disabled = false;
  }
  brushDabs = [];
  const disc = L.circle(center, {
    radius: r,
    color: "#fbbf24",
    weight: 2,
    dashArray: "8 6",
    fillOpacity: 0.12,
    className: "brush-fetching",
    interactive: false,
  }).addTo(brushGroup);
  setStatus(`fetching tiles for r=${r.toFixed(0)} m…`);
  try {
    const res = await fetch("/api/tiles/prefetch", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ lat: center.lat, lon: center.lng, radiusM: r }),
    });
    const j = await res.json();
    if (!res.ok) throw new Error(j.error ?? res.statusText);
    const failed = (j.failed ?? []).length;
    disc.setStyle({ className: failed ? "brush-failed" : "brush-done" });
    setStatus(
      `brush: +${j.fetched} tiles, ${j.cached} already cached` +
        (j.terrariumReplaced ? `, ${j.terrariumReplaced} via Terrarium` : "") +
        (failed ? `, ${failed} failed` : "") +
        (j.capped ? " (capped — brush smaller spots)" : "")
    );
    // refresh the zone preview so the fetched data shows as layers
    if (polygon && !($("previewBtn") as HTMLButtonElement).disabled) {
      ($("previewBtn") as HTMLButtonElement).click();
    }
  } catch (e) {
    disc.setStyle({ className: "brush-failed" });
    setStatus(`brush fetch failed: ${String(e)}`);
  }
}

// ── boot ─────────────────────────────────────────────────────────────────
fetch("/health")
  .then((r) => r.json())
  .then((j) => setStatus(`mapkit-server ${j.version ?? ""} · ${j.zones ?? j.collections ?? 0} collections · draw a zone or pick an admin zone`))
  .catch(() => setStatus("mapkit-server offline — start it to stream tiles"));
refreshCollections();
loadRegions();
