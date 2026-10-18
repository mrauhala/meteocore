"use strict";

// API base, in priority order:
//   1. ?base=…              a path on THIS origin (e.g. a proxy-prefixed
//                           mount). Another origin is ignored: a crafted link
//                           must not make a served viewer fetch and render some
//                           other server's "radar volumes" as ours (CodeQL
//                           js/client-side-request-forgery). The host part is
//                           always location.origin, never taken from the query.
//   2. same origin          when served at /3dtiles/viewer (the normal path) —
//                           so it works on any deployment with no config.
//   3. the public instance  last resort, e.g. when this file is opened directly
//                           from disk (origin "null": no override applies).
//                           Intentional (the page targets the public API by
//                           default); it is a fixed constant, never user input.
const params = new URLSearchParams(location.search);
const here = location.pathname.replace(/\/+$/, "");
const sameOrigin = here.endsWith("/viewer")
  ? location.origin + here.slice(0, -"/viewer".length)
  : null;
const rawBase = params.get("base");
let override = null;
if (rawBase && location.origin !== "null") {
  try {
    // Resolve against this origin (normalises `..`, throws on malformed
    // input) and keep only its path.
    const u = new URL(rawBase, location.origin);
    if (u.origin === location.origin) {
      override = location.origin + u.pathname.replace(/\/+$/, "");
    } else {
      console.warn(`ignoring ?base=${rawBase}: the viewer only reads its own origin`);
    }
  } catch { /* ignore a malformed ?base override */ }
}
const BASE = override || sameOrigin || "https://meteocore.app.meteo.fi/3dtiles";

// The view a link opens (#1137): collection, quantity, representation, the
// numeric field (`min_value` for points/voxels, `threshold` for the meshes),
// resolution and time. Applied once, to the first collection loaded; every
// later change rewrites the URL with history.replaceState, so the address bar
// is always a link to what is on screen. `?collection=` alone lets another
// page link straight to a volume.
const VIEW_KEYS = ["collection", "quantity", "representation", "min_value", "threshold", "resolution", "datetime"];
let pendingView = Object.fromEntries(VIEW_KEYS.map(k => [k, params.get(k)]));

const $bar = document.getElementById("bar");
const $menu = document.getElementById("menu");
const $cesium = document.getElementById("cesium");
const $coll = document.getElementById("coll");
const $qty = document.getElementById("qty");
const $rep = document.getElementById("rep");
const $res = document.getElementById("res");
const $resField = document.getElementById("resField");
const $num = document.getElementById("num");
const $numLabel = document.getElementById("numLabel");
const $status = document.getElementById("status");
const $fallback = document.getElementById("fallback");
const $fallbackTitle = document.getElementById("fallbackTitle");
const $fallbackText = document.getElementById("fallbackText");
const $fallbackList = document.getElementById("fallbackList");
const $legend = document.getElementById("legend");
const $legendTitle = $legend.querySelector(".ltitle");
const $legendBar = $legend.querySelector(".lbar");
const $legendTicks = $legend.querySelector(".lticks");
const $timebar = document.getElementById("timebar");
const $play = document.getElementById("play");
const $time = document.getElementById("time");
const $timeLabel = document.getElementById("timeLabel");
const $timeCount = document.getElementById("timeCount");
const setStatus = (msg, err) => { $status.textContent = msg; $status.title = msg; $status.className = err ? "err" : ""; };

// Each representation's label for the numeric field + its reflectivity default:
// point clouds drop echoes below `min_value`; isosurfaces draw nested shells AT
// the comma-separated `threshold` list (#363 — translucent envelopes around an
// opaque core; a single value is one opaque shell); echo-top columns rise to
// where reflectivity drops below `threshold`. The field is relabelled with the
// quantity's unit; `multi` switches it to a free-text field accepting the
// comma list. `grid` products (isosurface/echotop/voxels) are resampled to a
// voxel grid, so they take the low/med/high Resolution selector; the point
// cloud is native-res and hides it. `voxels` is the CesiumJS VoxelPrimitive
// ray-march (its own load path, not a tileset of tiles).
// `legend` = coloured by the quantity's ramp (points + voxels show the colour
// scale; isosurface is coloured per-shell threshold, echo-top a height ramp →
// no legend). `reflectivityOnly`: the server seals the grid products at the
// −32 dBZ no-echo floor whatever the quantity, which reads as data for a
// velocity, so they are offered for reflectivity only.
const REP_META = {
  points:     { label: "Point cloud", numLabel: "min",       numDefault: "5",        mesh: false, voxel: false, legend: true  },
  isosurface: { label: "Isosurface",  numLabel: "shells",    numDefault: "20,35,50", mesh: true,  voxel: false, legend: false, multi: true, reflectivityOnly: true },
  echotop:    { label: "Echo top",    numLabel: "threshold", numDefault: "18",       mesh: true,  voxel: false, legend: false, reflectivityOnly: true },
  voxels:     { label: "Voxels",      numLabel: "min",       numDefault: "5",        mesh: true,  voxel: true,  legend: true,  reflectivityOnly: true },
};

// ── CesiumJS ───────────────────────────────────────────────────────────────
// The pinned release, with Subresource Integrity on its entry point and
// stylesheet: a CDN compromise/MITM can't silently swap the bundle. (Cesium
// then loads workers/WASM from the CDN without SRI — full mitigation would
// self-host CesiumJS.) MANDATORY when bumping the version: recompute both
// sha384 hashes with `scripts/check-cesium-sri.sh` (it reads them from this
// file, script first), and move the CDN prefix in the page's
// Content-Security-Policy (`VIEWER_CSP`, crates/api-3dtiles/src/handlers.rs)
// to the new release. A stale hash or prefix keeps CesiumJS from loading; the
// page then shows the fallback panel instead of a 3D view.
const CESIUM_SCRIPT = {
  src: "https://cesium.com/downloads/cesiumjs/releases/1.142/Build/Cesium/Cesium.js",
  integrity: "sha384-PCryXL7CqnqM5KijUKfJgy+y9b4vE7GzIHvb5J/MSSnIkFptiGOUrlt4s9uBUdtO",
};
const CESIUM_STYLE = {
  href: "https://cesium.com/downloads/cesiumjs/releases/1.142/Build/Cesium/Widgets/widgets.css",
  integrity: "sha384-ghEeMdcWWzRv/BPeUcX835vcKDGrxvROXisl/Btpv3GeekBUXTSPVcFJpI1Tcrgp",
};
// How long CesiumJS may take before the page says it has not loaded. The
// script is inserted from here, not as a parser-blocking tag in the page, so
// the page renders and lists the collections meanwhile; a host that silently
// drops packets would otherwise keep the page blank until the connection gives
// up, a minute or more (#1137). A script that arrives later still starts the
// 3D view.
const CESIUM_TIMEOUT_MS = 12000;

// Token-free CesiumJS: CARTO Dark Matter basemap, no Cesium Ion. Startup is
// guarded (#1137): while CesiumJS is loading, and when the CDN is blocked or
// slow, the SRI hash or CSP is stale, or WebGL is off, `viewer` stays null and
// the page still lists the collections with links to their data.
let viewer = null;
let startupError = null;     // null | "missing" (did not load) | "timeout" | the Viewer's error
let collectionList = null;   // the `/collections` list once fetched (the fallback lists it)

function loadCesium() {
  if (typeof Cesium !== "undefined") { startViewer(); return; } // already on the page
  const link = document.createElement("link");
  link.rel = "stylesheet";
  link.crossOrigin = "anonymous";
  link.integrity = CESIUM_STYLE.integrity;
  link.href = CESIUM_STYLE.href;
  // Before the page's own <style>, so the page's rules win ties as they did
  // when the stylesheet was a tag above them.
  document.head.insertBefore(link, document.head.querySelector("style"));
  const script = document.createElement("script");
  script.crossOrigin = "anonymous";
  script.integrity = CESIUM_SCRIPT.integrity;
  script.src = CESIUM_SCRIPT.src;
  const timer = setTimeout(() => cesiumFailed("timeout"), CESIUM_TIMEOUT_MS);
  script.addEventListener("load", () => {
    clearTimeout(timer);
    // Loaded but not defined: the bundle threw while evaluating (a CSP that
    // forbids its eval, for one).
    if (typeof Cesium === "undefined") cesiumFailed("missing");
    else startViewer();
  });
  script.addEventListener("error", () => { clearTimeout(timer); cesiumFailed("missing"); });
  document.head.appendChild(script);
}

function cesiumFailed(reason) {
  if (viewer) return;
  startupError = reason;
  renderFallback();
}

function startViewer() {
  if (viewer) return;
  try {
    viewer = new Cesium.Viewer("cesium", {
      baseLayer: new Cesium.ImageryLayer(new Cesium.UrlTemplateImageryProvider({
        url: "https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png",
        subdomains: "abcd", maximumLevel: 19,
        credit: "© OpenStreetMap contributors © CARTO",
      })),
      baseLayerPicker: false, timeline: false, animation: false, geocoder: false,
      homeButton: false, sceneModePicker: false, navigationHelpButton: false, infoBox: false, selectionIndicator: false,
    });
    viewer.scene.backgroundColor = Cesium.Color.BLACK;
    viewer.scene.globe.depthTestAgainstTerrain = false;
  } catch (e) {
    console.error("3D view could not start", e);
    startupError = e;
    viewer = null;
    $cesium.hidden = true;
    renderFallback();
    return;
  }
  FrameResource = frameResourceClass();
  // CesiumJS may arrive after the fallback was shown and the collection's
  // metadata loaded: take the panel down and draw what the controls select.
  startupError = null;
  $fallback.hidden = true;
  buildLegend();
  if (metaCollection && metaCollection === $coll.value) loadFrames(true);
}

// Time-dynamic playback: one preloaded tileset per available volume timestamp
// (#350). All frames are kept hidden except the active one, so scrubbing/playing
// just toggles `.show` — no per-frame network/reload stall. A non-time-dynamic
// collection is just the degenerate 1-frame case. Frames load progressively
// (active frame first, then newest→oldest, bounded concurrency) and every loaded
// tileset is kept in an LRU cache keyed by its tileset URL, so toggling a
// control and back is a pure visibility flip, not a refetch (#379).
let frames = [];          // Cesium3DTileset per animated timestamp (parallel to `frameTimes`; null = failed/not yet loaded)
let times = [];           // full volume-time manifest (ISO Z) from the collection doc
let frameTimes = [];      // the timestamps actually animated = most-recent MAX_FRAMES of `times`
let frameIdx = 0;         // active index into `frameTimes`
let playbackIntent = 0; // user play/pause/seek changes invalidate deferred auto-resume
let playing = false;      // animation running?
let playTimer = null;     // setInterval handle while playing
const FRAME_MS = 600;     // per-frame dwell when playing
// Safety cap on preloaded frames: a source with `max_files` unset (the default)
// can retain hundreds of volumes (a day ≈ 288 at 5-min cadence), and preloading
// all of them would mean hundreds of concurrent fetches + GPU-resident tilesets.
// Animate the most-recent MAX_FRAMES; a true sliding window for longer runs is a
// follow-up. Operators bound the source itself with `[odim] max_files`/`time_window`.
const MAX_FRAMES = 48;
let loadToken = 0;        // guards against out-of-order async (re)builds
let currentRep = null;    // representation of the active frames
let pointsFetchFloor = null; // the min_value the point-cloud frames were fetched at
let pointsStyleMin = null;   // the min currently applied to point styles (client-side filter)
let legendData = null;    // colour-scale legend {title,unit,min,max,stops} of the active quantity
let collectionLegend = null; // the collection doc's top-level legend (servers without per-quantity legends)
let quantities = new Map(); // quantity id → {id,label,unit,legend,min_value} from the collection doc
let voxelPrimitive = null; // active Cesium VoxelPrimitive (the `voxels` representation)
let metadataToken = 0;
let metadataController = null;
let metaCollection = null;
let frameController = null;
let refreshingTimes = false;

async function loadCollections() {
  setStatus("loading collections…");
  try {
    const data = await (await fetch(`${BASE}/collections`)).json();
    const cols = data.collections || [];
    $coll.innerHTML = "";
    for (const c of cols) {
      const o = document.createElement("option");
      o.value = c.id; o.textContent = c.title || c.id;
      $coll.appendChild(o);
    }
    collectionList = cols;
    renderFallback();
    if (!cols.length) { setStatus("no 3D-Tiles collections at " + BASE, true); return; }
    const wanted = pendingView && pendingView.collection;
    if (wanted && cols.some(c => c.id === wanted)) $coll.value = wanted;
    await loadCollectionMeta(true); // first view → frame the volume
  } catch (e) {
    setStatus("can't reach " + BASE + " — " + e, true);
  }
}

// The 3D view could not start (or CesiumJS has not arrived in time): say why,
// and list every collection with links to its 3D Tiles document and its page
// in the other APIs, so the data is one click away. Called whenever the
// startup state or the collection list changes; shows nothing while CesiumJS
// is still within its time. Built with DOM nodes and textContent only.
function renderFallback() {
  if (viewer || !startupError) return;
  let title, text;
  if (startupError === "missing") {
    title = "3D library could not be loaded";
    text = "The 3D view needs CesiumJS from cesium.com, which did not load: the host may be blocked on this network. The volumes are still served:";
  } else if (startupError === "timeout") {
    title = "3D library has not loaded";
    text = `The 3D view needs CesiumJS from cesium.com, which has not arrived after ${CESIUM_TIMEOUT_MS / 1000} s: the host may be blocked or slow on this network. The view starts by itself if it arrives. The volumes are still served:`;
  } else {
    title = "3D view could not start";
    text = `This browser could not start the 3D view (${startupError.message || startupError}). WebGL may be off. The volumes are still served:`;
  }
  $fallbackTitle.textContent = title;
  $fallbackText.textContent = text;
  $fallbackList.textContent = "";
  for (const c of collectionList || []) {
    const li = document.createElement("li");
    const name = document.createElement("span");
    name.className = "fname";
    name.textContent = c.title || c.id;
    li.appendChild(name);
    li.appendChild(fallbackLink(`${BASE}/collections/${encodeURIComponent(c.id)}`, "3D Tiles JSON"));
    const page = (c.links || []).find(l => l && l.rel === "related" && l.type === "text/html");
    if (page) li.appendChild(fallbackLink(page.href, page.title || "Collection page"));
    $fallbackList.appendChild(li);
  }
  $fallback.hidden = false;
  $legend.hidden = true;
  setStatus(title, true);
}

// A link from server metadata: http(s) only, so a document can never inject
// a `javascript:` URL into the page.
function fallbackLink(href, text) {
  const a = document.createElement("a");
  try {
    const u = new URL(href, location.href);
    if (u.protocol === "http:" || u.protocol === "https:") a.href = u.href;
  } catch { /* leave it as plain text */ }
  a.textContent = text;
  return a;
}

// `reframe` controls whether the camera is moved to frame the volume. Frame on a
// collection switch (the volume may be elsewhere); preserve the camera for
// in-place changes (quantity/representation/threshold/resolution) so adjusting a
// filter doesn't yank the view.
async function loadCollectionMeta(reframe) {
  const id = $coll.value;
  if (!id) return;
  const token = ++metadataToken;
  metadataController?.abort();
  metadataController = new AbortController();
  ++loadToken;
  frameController?.abort();
  stopPlaying();
  try {
    const response = await fetch(`${BASE}/collections/${encodeURIComponent(id)}`, { signal: metadataController.signal });
    if (!response.ok) throw new Error(`collection request failed (${response.status})`);
    const data = await response.json();
    if (token !== metadataToken || id !== $coll.value) return;
    metaCollection = id;
    collectionLegend = data.legend || null;
    // The volume time axis (ascending). Default to the latest frame so the static
    // initial view matches the no-`datetime` default (latest volume); play loops.
    times = Array.isArray(data.times) ? data.times : [];
    frameIdx = times.length ? times.length - 1 : 0;
    const qs = data.quantities || [];
    quantities = new Map(qs.map(q => [q.id, q]));
    $qty.innerHTML = "";
    for (const q of qs) {
      const o = document.createElement("option");
      o.value = q.id; o.textContent = q.label || q.id;
      $qty.appendChild(o);
    }
    // Prefer a reflectivity quantity for the default view.
    for (const pref of ["DBZH", "TH"]) {
      const i = [...$qty.options].findIndex(o => o.value === pref);
      if (i >= 0) { $qty.selectedIndex = i; break; }
    }
    // Representations the collection actually serves (always at least points).
    const reps = (data.representations && data.representations.length) ? data.representations : ["points"];
    $rep.innerHTML = "";
    for (const r of reps) {
      const o = document.createElement("option");
      o.value = r; o.textContent = (REP_META[r] && REP_META[r].label) || r;
      $rep.appendChild(o);
    }
    const view = pendingView;
    pendingView = null;
    if (view) applyView(view, reps);
    else { syncRepOptions(); syncNumField(); }
    // Drop the previous collection's cached frames only now that the new
    // collection's metadata is in hand — if the fetch above failed, the old
    // collection stays on screen instead of going blank. (On the initial load
    // the cache is empty and this is a no-op.)
    clearFrameCache();
    await loadFrames(reframe);
  } catch (e) {
    if (token === metadataToken && e.name !== "AbortError") setStatus("error loading collection — " + e, true);
  }
}

// Restore a linked view onto the freshly loaded collection: each part the
// collection has, the defaults for the rest.
function applyView(view, reps) {
  if (view.quantity && quantities.has(view.quantity)) $qty.value = view.quantity;
  if (view.representation && reps.includes(view.representation)) $rep.value = view.representation;
  if (view.resolution && [...$res.options].some(o => o.value === view.resolution)) $res.value = view.resolution;
  syncRepOptions();
  syncNumField();
  const rep = $rep.value || "points";
  const linked = rep === "isosurface" || rep === "echotop" ? view.threshold : view.min_value;
  if (linked !== null && linked !== undefined) $num.value = linked;
  if (view.datetime) {
    const want = Date.parse(view.datetime);
    const i = times.slice(-MAX_FRAMES).findIndex(t => Date.parse(t) === want);
    if (i >= 0) frameIdx = i;
  }
}

// Refresh only the manifest: retain controls, camera, and ready frame objects.
// Follow newly arriving volumes when viewing latest; preserve a historical seek.
async function refreshCollectionTimes() {
  const id = metaCollection, token = metadataToken;
  if (!viewer || !id || id !== $coll.value || refreshingTimes || document.hidden) return;
  refreshingTimes = true;
  try {
    const response = await fetch(`${BASE}/collections/${encodeURIComponent(id)}`, { cache: "no-cache" });
    if (!response.ok) throw new Error(`collection request failed (${response.status})`);
    const data = await response.json();
    if (token !== metadataToken || id !== $coll.value) return;
    const updated = Array.isArray(data.times) ? data.times : [];
    if (updated.length === times.length && updated.every((t, i) => t === times[i])) return;
    const selected = frameTimes[frameIdx];
    const followLatest = currentRep === "voxels" || !selected || selected === times[times.length - 1];
    const resumePlayback = playing;
    times = updated;
    const window = times.slice(-MAX_FRAMES);
    frameIdx = followLatest ? Math.max(0, window.length - 1) : Math.max(0, window.indexOf(selected));
    await loadFrames(false, resumePlayback, true);
  } catch (e) {
    console.warn("volume manifest refresh", e);
  } finally {
    refreshingTimes = false;
  }
}

// ── Quantity ───────────────────────────────────────────────────────────────
// The collection doc describes each quantity (#1137): its unit, the legend of
// the ramp its points are painted with, and a suggested floor (`min_value`,
// null when the quantity has no weak end to drop).
function quantityMeta(id = $qty.value) { return quantities.get(id) || null; }
// A server that predates per-quantity units drew every quantity on the dBZ
// ramp: read a quantity without `unit` as reflectivity, as before.
function quantityUnit(q = quantityMeta()) { return q && typeof q.unit === "string" ? q.unit : "dBZ"; }
function isReflectivity(q = quantityMeta()) { return quantityUnit(q) === "dBZ"; }

// The grid products are reflectivity products: disable them for any other
// quantity (and leave one for the point cloud).
function syncRepOptions() {
  const refl = isReflectivity();
  for (const o of $rep.options) {
    const meta = REP_META[o.value];
    o.disabled = !refl && !!(meta && meta.reflectivityOnly);
    o.title = o.disabled ? "Reflectivity only" : "";
  }
  const current = REP_META[$rep.value];
  if (!refl && current && current.reflectivityOnly) $rep.value = "points";
}

// Relabel + default the numeric field for the active representation and
// quantity: "min <unit>" and the reflectivity defaults for dBZ, empty (no
// filter) for every other quantity. `keepValue` (a quantity change) keeps
// what the field holds when the unit stays the same, so a filter typed for
// DBZH still applies to TH. Show the Resolution selector only for the
// (resampled) mesh products, and the quantity's legend for the coloured ones.
let numUnit = null; // the unit the field's value is in
function syncNumField(keepValue = false) {
  const rep = $rep.value || "points";
  const meta = REP_META[rep] || REP_META.points;
  const q = quantityMeta();
  const unit = quantityUnit(q);
  $numLabel.textContent = `${meta.numLabel} ${unit || "value"}`;
  // A multi-value field (isosurface shell list, e.g. "20,35,50") needs a text
  // input — type=number silently rejects commas.
  $num.type = meta.multi ? "text" : "number";
  if (!(keepValue && unit === numUnit)) $num.value = numDefault(rep, q);
  numUnit = unit;
  $resField.hidden = !meta.mesh;
  legendData = (q && q.legend) || collectionLegend;
  buildLegend();
}

function numDefault(rep, q) {
  if (!isReflectivity(q)) return "";
  const meta = REP_META[rep] || REP_META.points;
  if (!meta.multi && rep !== "echotop" && q && Number.isFinite(q.min_value)) return String(q.min_value);
  return meta.numDefault;
}

// Build the colour-scale legend from the active quantity's `legend` (a sampled
// gradient of the ramp its points are painted with). A vertical bar, low value
// at the bottom, with tick labels at round values across the range.
function buildLegend() {
  const lg = legendData;
  const meta = REP_META[$rep.value || "points"] || REP_META.points;
  if (!viewer || !meta.legend || !lg || !lg.stops || !lg.stops.length
      || !Number.isFinite(lg.min) || !Number.isFinite(lg.max)) { $legend.hidden = true; return; }
  $legend.hidden = false;
  $legendTitle.textContent = (lg.title || "") + (lg.unit ? ` (${lg.unit})` : "");
  const span = (lg.max - lg.min) || 1;
  const frac = (v) => Math.max(0, Math.min(1, (v - lg.min) / span));
  // CSS gradient bottom→top, each stop placed at its value's fraction.
  const css = lg.stops.map(s => `${s.color} ${(frac(s.value) * 100).toFixed(1)}%`).join(", ");
  $legendBar.style.background = `linear-gradient(to top, ${css})`;
  $legendTicks.innerHTML = "";
  for (const val of legendTicks(lg.min, lg.max)) {
    const el = document.createElement("div");
    el.className = "tick";
    el.style.bottom = (frac(val) * 100) + "%";
    el.textContent = String(val);
    $legendTicks.appendChild(el);
  }
}

// Round tick values across [min, max], ~7 or fewer: a 1/2/2.5/5 × 10ⁿ step, so
// 0–70 dBZ ticks every 10, ±48 m/s every 20 and a 0–1 coefficient every 0.2.
function legendTicks(min, max) {
  const span = max - min;
  if (!(span > 0)) return [min];
  const raw = span / 7;
  const mag = Math.pow(10, Math.floor(Math.log10(raw)));
  const step = [1, 2, 2.5, 5, 10].map(m => m * mag).find(s => s >= raw);
  const decimals = Math.max(0, -Math.floor(Math.log10(step)) + (Number.isInteger(step / mag) ? 0 : 1));
  const ticks = [];
  for (let k = Math.ceil(min / step - 1e-9); k * step <= max + 1e-9 * span; k++) {
    ticks.push(Number((k * step).toFixed(decimals)));
  }
  return ticks;
}

// The tileset.json URL for the active params at a given volume timestamp
// (`time` = null → the server's latest-volume default, no `datetime`).
function tilesetUrl(time, pointFloor = null) {
  const id = $coll.value, q = $qty.value, rep = $rep.value || "points";
  const num = rep === "points" && pointFloor !== null
    ? (Number.isFinite(pointFloor) ? String(pointFloor) : "") : $num.value.trim();
  let url = `${BASE}/collections/${encodeURIComponent(id)}/tileset.json?quantity=${encodeURIComponent(q)}`;
  if (rep === "isosurface" || rep === "echotop") {
    url += `&representation=${rep}`;
    if (num !== "") url += `&threshold=${encodeURIComponent(num)}`;
    url += `&resolution=${encodeURIComponent($res.value)}`;
  } else if (num !== "") {
    url += `&min_value=${encodeURIComponent(num)}`;
  }
  if (time) url += `&datetime=${encodeURIComponent(time)}`;
  return url;
}

// Write the view on screen to the address bar (see VIEW_KEYS). The time is
// written only for a historical frame: the latest frame stays "latest", so a
// shared link follows new volumes. Never called per playback frame.
function syncUrl() {
  if (typeof history === "undefined" || !history.replaceState) return;
  const p = new URLSearchParams(location.search);
  for (const k of VIEW_KEYS) p.delete(k);
  if ($coll.value) p.set("collection", $coll.value);
  if ($qty.value) p.set("quantity", $qty.value);
  const rep = $rep.value || "points";
  p.set("representation", rep);
  p.set(rep === "isosurface" || rep === "echotop" ? "threshold" : "min_value", $num.value.trim());
  if ((REP_META[rep] || {}).mesh) p.set("resolution", $res.value);
  const t = frameTimes[frameIdx];
  if (t && rep !== "voxels" && frameIdx < frameTimes.length - 1) p.set("datetime", t);
  try {
    history.replaceState(history.state, "", `${location.pathname}?${p}`);
  } catch (e) {
    console.warn("view not written to the URL", e);
  }
}

// Frame cache (#379): every loaded tileset is keyed by its full tileset URL —
// which encodes collection, quantity, representation, threshold/min_value,
// resolution AND datetime — and kept in the scene hidden, so toggling a control
// and back reuses the already-downloaded tileset instead of refetching every
// frame. LRU-capped at the active frame set plus one full previous set; eviction
// removes the tileset from the scene (the primitive collection's
// destroyPrimitives:true destroys its GPU buffers on remove()).
const readyFrames = new WeakSet();
function isFrameReady(ts) { return !!ts && !ts.isDestroyed() && readyFrames.has(ts); }
const tileCache = new Map(); // tileset URL → Cesium3DTileset (in scene.primitives, hidden)
const TILE_CACHE_MAX = 2 * MAX_FRAMES;

// Remove a tileset from the scene AND make sure its GPU buffers are freed.
// remove() destroys only when the primitive collection has
// destroyPrimitives:true (the CesiumJS default, but nothing asserts it) — the
// explicit destroy() makes eviction leak-proof regardless of the flag.
function dropTileset(ts) {
  viewer.scene.primitives.remove(ts);
  if (!ts.isDestroyed()) ts.destroy();
}

function cacheGet(url) {
  const ts = tileCache.get(url);
  if (!ts) return null;
  if (!isFrameReady(ts)) {
    tileCache.delete(url);
    if (!ts.isDestroyed()) dropTileset(ts);
    return null;
  }
  tileCache.delete(url); tileCache.set(url, ts); // LRU bump
  return ts;
}
function cachePut(url, ts) {
  // Pinned = the active frames plus the entry being inserted (`frames[i] = ts`
  // only happens after cachePut returns, so `ts` isn't in `frames` yet).
  // Deliberately reads the LIVE `frames`, not a per-build snapshot — a stale
  // worker must see the NEWER build's frames to detect the A→B→A dup below;
  // threading a snapshot through loadFrames would break that guard.
  const pinned = new Set(frames);
  pinned.add(ts);
  // Two overlapping builds can both fetch the same URL (the first superseded
  // mid-flight), so the slower one finds the winner already in the map. If the
  // existing entry is hidden, it's the orphan — drop it and overwrite. But a
  // stale worker can also land AFTER a newer build cached and displayed the
  // same URL (rapid A→B→A toggling): then `prev` is the live, pinned frame and
  // the incoming `ts` is the stale duplicate — destroy `ts` (it was added to
  // the scene just before this call), keep the map pointing at `prev`, and
  // return false so the caller drops its now-dead reference; overwriting would
  // orphan the live tileset out of the cache, beyond the reach of both LRU
  // eviction and clearFrameCache.
  const prev = tileCache.get(url);
  if (prev && prev !== ts) {
    if (pinned.has(prev)) { dropTileset(ts); return false; }
    dropTileset(prev);
  }
  tileCache.set(url, ts);
  // Soft cap: pinned entries are skipped, not evicted, so the size can sit
  // above TILE_CACHE_MAX until frames unpin (harmless — pins ≤ MAX_FRAMES + 1,
  // and the cap is 2× that).
  for (const [u, old] of tileCache) {
    if (tileCache.size <= TILE_CACHE_MAX) break;
    if (pinned.has(old)) continue; // never evict a frame in active use
    tileCache.delete(u);
    dropTileset(old);
  }
  return true;
}
// Collection switch: the cached frames belong to another volume entirely —
// destroy them all rather than letting them age out of the LRU. Bumping
// `cacheGen` makes in-flight workers from the old build discard what they
// fetched instead of repopulating the just-cleared cache + scene (each build
// captures the generation it started under).
let cacheGen = 0;
function clearFrameCache() {
  frameController?.abort();
  stopPlaying(); // don't leave the play timer ticking over an emptied `frames`
  cacheGen++;
  frames = [];
  for (const ts of tileCache.values()) dropTileset(ts);
  tileCache.clear();
}

// Hide the current frame set. The tilesets stay in the scene + cache, so coming
// back to the same params is a visibility flip; only LRU eviction or a
// collection switch (clearFrameCache) actually destroys them.
function hideFrames() {
  for (const ts of frames) if (ts) ts.show = false;
  frames = [];
}
function clearVoxel() {
  if (voxelPrimitive) { viewer.scene.primitives.remove(voxelPrimitive); voxelPrimitive = null; }
}

// (Re)build the frame set for the current params: one hidden tileset per
// timestamp (or a single latest-volume frame when the collection has no time
// axis). Cached frames appear instantly; missing ones load progressively — the
// active frame first (shown + reframed as soon as it's ready), then the rest
// newest→oldest with bounded concurrency — instead of one unthrottled
// Promise.all over all 48 (#379).
const PRELOAD_CONCURRENCY = 6;
const FRAME_LOAD_TIMEOUT_MS = 120000;
const frameLoadQueue = [];
let activeFrameLoads = 0;
const frameAbort = () => new DOMException("Frame load superseded", "AbortError");

// Resource clones retain an abortable JSON loader; content URLs still resolve
// against the original tileset URL and use Cesium's normal binary loaders.
// Defined by startViewer, once CesiumJS has loaded (the page still lists data
// without it).
let FrameResource = null;
function frameResourceClass() {
  return class FrameResource extends Cesium.Resource {
    constructor(options, signal) { super(options); this.frameSignal = signal; }
    clone(result) {
      const copy = super.clone(result || new FrameResource({ url: this.url }, this.frameSignal));
      copy.frameSignal = this.frameSignal;
      return copy;
    }
    async fetchJson() {
      const response = await fetch(this.url, {
        signal: this.frameSignal, headers: { Accept: "application/json", ...this.headers },
      });
      if (!response.ok) throw new Error(`Tileset metadata HTTP ${response.status}`);
      return response.json();
    }
  };
}
async function loadFrameMetadata(url, options, signal) {
  if (signal.aborted) throw frameAbort();
  const controller = new AbortController();
  const abort = () => controller.abort(frameAbort());
  signal.addEventListener("abort", abort, { once: true });
  const timer = setTimeout(() => controller.abort(new Error("Tileset metadata load timed out")), FRAME_LOAD_TIMEOUT_MS);
  try {
    return await Cesium.Cesium3DTileset.fromUrl(new FrameResource({ url }, controller.signal), options);
  } finally {
    clearTimeout(timer);
    signal.removeEventListener("abort", abort);
  }
}

// One queue across ALL builds. Cancellation aborts metadata fetches before
// releasing their slots; a control change cannot multiply active requests.
function queueFrameLoad(signal, load) {
  return new Promise((resolve, reject) => {
    if (signal.aborted) { reject(frameAbort()); return; }
    const job = { signal, load, resolve, reject, cancel: null };
    job.cancel = () => {
      const index = frameLoadQueue.indexOf(job);
      if (index >= 0) { frameLoadQueue.splice(index, 1); reject(frameAbort()); }
    };
    signal.addEventListener("abort", job.cancel, { once: true });
    frameLoadQueue.push(job);
    drainFrameLoads();
  });
}
function drainFrameLoads() {
  while (activeFrameLoads < PRELOAD_CONCURRENCY && frameLoadQueue.length) {
    const job = frameLoadQueue.shift();
    job.signal.removeEventListener("abort", job.cancel);
    if (job.signal.aborted) { job.reject(frameAbort()); continue; }
    activeFrameLoads++;
    Promise.resolve().then(job.load).then(job.resolve, job.reject).finally(() => {
      activeFrameLoads--;
      drainFrameLoads();
    });
  }
}

// These API tilesets have one root content tile. `tilesLoaded` / metadata
// readiness can be true when offscreen with NO content, so wait for tileLoad.
// Resolve only from the event; scene/cache mutations happen after traversal.
function waitForFrameContent(ts, signal) {
  return new Promise((resolve, reject) => {
    if (signal.aborted) { reject(frameAbort()); return; }
    if (isFrameReady(ts)) { resolve(); return; }
    let timer;
    const cleanup = () => {
      removeLoaded(); removeFailed();
      signal.removeEventListener("abort", aborted);
      clearTimeout(timer);
    };
    const loaded = (tile) => { if (tile === ts.root) { cleanup(); resolve(); } };
    const failed = (error) => { cleanup(); reject(new Error(error.message || "Tile content failed")); };
    const aborted = () => { cleanup(); reject(frameAbort()); };
    const removeLoaded = ts.tileLoad.addEventListener(loaded);
    const removeFailed = ts.tileFailed.addEventListener(failed);
    signal.addEventListener("abort", aborted, { once: true });
    timer = setTimeout(() => failed({ message: "Tile content load timed out" }), FRAME_LOAD_TIMEOUT_MS);
  });
}

async function loadFrames(reframe = false, resumePlayback = false, refresh = false) {
  const id = $coll.value, q = $qty.value, rep = $rep.value || "points";
  const num = $num.value.trim();
  if (!id || !q || id !== metaCollection) return;
  if (!viewer) {
    // No 3D view yet (startViewer draws once CesiumJS is in), or none at all:
    // the fallback panel then lists the data and owns the status line.
    syncUrl();
    if (!startupError) setStatus("loading the 3D library…");
    return;
  }
  frameController?.abort();
  frameController = new AbortController();
  const signal = frameController.signal;
  const token = ++loadToken;
  const playIntent = playbackIntent;
  const gen = cacheGen; // cache generation this build belongs to
  stopPlaying();
  hideFrames();
  clearVoxel();
  const retainedFloor = refresh && currentRep === "points" ? pointsFetchFloor : null;
  currentRep = rep;
  pointsFetchFloor = rep === "points" ? (retainedFloor ?? (num === "" ? -Infinity : parseFloat(num))) : null;

  // Voxels use one ray-marched primitive, without animation. Pin the latest
  // advertised time so the periodic manifest refresh advances this view too.
  if (rep === "voxels") {
    frameTimes = [times[times.length - 1] || null];
    frameIdx = 0;
    syncTimeBar();
    syncUrl();
    await loadVoxel(token);
    return;
  }

  // Animate the most-recent MAX_FRAMES of the manifest ([null] = the single
  // latest-volume frame when there's no time axis).
  frameTimes = times.length ? times.slice(-MAX_FRAMES) : [null];
  if (frameIdx >= frameTimes.length) frameIdx = frameTimes.length - 1;
  if (frameIdx < 0) frameIdx = 0;
  const capped = times.length > frameTimes.length ? ` (latest ${frameTimes.length} of ${times.length})` : "";
  syncUrl();

  // Satisfy what we can from the frame cache, then show it immediately — a
  // control toggled back to cached params renders with zero network.
  const urls = frameTimes.map((t) => tilesetUrl(t, pointsFetchFloor));
  frames = urls.map((u) => cacheGet(u));
  if (rep === "points") applyPointStyle(num === "" ? -Infinity : parseFloat(num));
  syncTimeBar();
  showFrame(frameIdx);
  if (resumePlayback) startPlaying();

  // Load order for the misses: the active frame first (it's what's on screen),
  // then the rest newest→oldest so recent frames fill in first.
  const pending = [];
  if (!frames[frameIdx]) pending.push(frameIdx);
  for (let i = frames.length - 1; i >= 0; i--) if (i !== frameIdx && !frames[i]) pending.push(i);
  const progress = () => {
    const done = frames.filter(isFrameReady).length;
    setStatus(`loading ${done}/${frameTimes.length} frames${capped} · ${statusLine()}`);
  };
  if (pending.length) progress();

  // Reframe the camera as soon as the active frame exists (cached, or first to
  // load) — don't hold the view hostage to the background fill.
  let needReframe = reframe;
  const maybeReframe = async (ts = frames[frameIdx]) => {
    if (!needReframe || !ts) return;
    needReframe = false;
    try {
      await viewer.zoomTo(ts, new Cesium.HeadingPitchRange(
        Cesium.Math.toRadians(45), Cesium.Math.toRadians(-28), 300000));
    } catch (_) {
      // A second collection switch during the ~1 s flight destroys `ts` via
      // clearFrameCache and zoomTo throws on it — a superseded build, not an
      // error to surface (the token check after each maybeReframe call exits).
    }
  };
  await maybeReframe();
  if (token !== loadToken) return; // superseded during the zoomTo animation

  // Only content-ready tilesets enter frames/cache. Failed or aborted loads
  // are destroyed, so playback never mistakes successful JSON for a frame.
  let next = 0;
  const worker = async () => {
    while (next < pending.length && token === loadToken) {
      const i = pending[next++];
      let ts = null;
      try {
        ts = await queueFrameLoad(signal, async () => {
          if (signal.aborted) throw frameAbort();
          const tile = await loadFrameMetadata(urls[i],
            { maximumScreenSpaceError: 1, show: false, preloadWhenHidden: true }, signal);
          try {
            if (signal.aborted) throw frameAbort();
            // Track readiness through public content events, including eviction.
            tile.tileLoad.addEventListener(root => { if (root === tile.root) readyFrames.add(tile); });
            tile.tileUnload.addEventListener(root => { if (root === tile.root) readyFrames.delete(tile); });
            const ready = waitForFrameContent(tile, signal);
            viewer.scene.primitives.add(tile);
            // The initial camera must see the radar before Cesium will request
            // content. Reframe from metadata, without advertising it as playable.
            await Promise.all([ready, i === frameIdx ? maybeReframe(tile) : Promise.resolve()]);
            if (signal.aborted) throw frameAbort();
            tile.preloadWhenHidden = false;
            return tile;
          } catch (e) {
            dropTileset(tile);
            throw e;
          }
        });
      } catch (e) {
        if (e.name !== "AbortError") console.error("frame", urls[i], e);
      }
      if (token !== loadToken || gen !== cacheGen) {
        if (ts) dropTileset(ts);
        return;
      }
      if (ts && !cachePut(urls[i], ts)) ts = cacheGet(urls[i]);
      frames[i] = ts;
      if (ts) {
        if (currentRep === "points") stylePointFrame(ts, pointsStyleMin);
        syncTimeBar();
        if (i === frameIdx) showFrame(frameIdx);
        if (resumePlayback && playIntent === playbackIntent) startPlaying();
      }
      progress();
    }
  };
  await Promise.all(Array.from({ length: Math.min(PRELOAD_CONCURRENCY, pending.length) }, () => worker()));
  if (token !== loadToken) return;

  if (!frames.some(isFrameReady)) setStatus("no data for this selection", true);
  else {
    updateStatus(filterLabel());
    if (resumePlayback && playIntent === playbackIntent) startPlaying();
  }
}

// Load the cylindrical voxel volume for the active time as a CesiumJS
// VoxelPrimitive (ray-marched), styled by a reflectivity transfer-function
// custom shader. EXT_primitive_voxels is CesiumGS-only + draft, so this whole
// path is best-effort — a load failure just reports, leaving the other
// representations untouched.
async function loadVoxel(token) {
  const id = $coll.value, q = $qty.value;
  setStatus(`loading ${statusLine()}…`);
  let url = `${BASE}/collections/${encodeURIComponent(id)}/voxel/tileset.json?quantity=${encodeURIComponent(q)}`;
  const time = frameTimes.length ? frameTimes[frameIdx] : null;
  if (time) url += `&datetime=${encodeURIComponent(time)}`;
  url += `&resolution=${encodeURIComponent($res.value)}`;
  try {
    if (typeof Cesium.Cesium3DTilesVoxelProvider !== "function") {
      throw new Error("this CesiumJS build has no voxel support");
    }
    const provider = await Cesium.Cesium3DTilesVoxelProvider.fromUrl(url);
    if (token !== loadToken) return; // superseded
    voxelPrimitive = viewer.scene.primitives.add(new Cesium.VoxelPrimitive({
      provider,
      customShader: voxelShader(q, legendData, voxelMin()),
    }));
    // Default ray-march step (1 sample/cell). Finer stepping was tried (0.4,
    // 0.12) to smooth the cell lattice but made no visible difference — the
    // lattice is a property of the DATA, now dissolved by the encoder's
    // multi-pass blur, not of the step size — so it was pure cost (a smaller
    // multiplier means proportionally more samples per ray). Left at the
    // default for ~2.5× faster ray-marching with no quality loss.
    voxelPrimitive.stepSizeMultiplier = 1.0;
    viewer.scene.requestRender();
    updateStatus(filterLabel());
  } catch (e) {
    if (token === loadToken) setStatus("voxels unavailable — " + e.message, true);
    console.error("voxel load", e);
  }
}

// The numeric field as a voxel floor (cells below it are fully transparent).
function voxelMin() { const m = parseFloat($num.value); return Number.isFinite(m) ? m : parseFloat(REP_META.voxels.numDefault); }

// The active filter for the status line, in the quantity's unit: "≥ 5 dBZ".
function filterLabel() {
  const rep = $rep.value || "points";
  if (rep !== "points" && rep !== "voxels") return "";
  const m = parseFloat($num.value);
  const unit = quantityUnit();
  return Number.isFinite(m) ? `≥ ${m}${unit ? " " + unit : ""}` : "";
}

// #rrggbb → "vec3(r,g,b)" (0..1) for GLSL.
function hexToGlslVec3(hex) {
  const r = parseInt(hex.slice(1, 3), 16) / 255;
  const g = parseInt(hex.slice(3, 5), 16) / 255;
  const b = parseInt(hex.slice(5, 7), 16) / 255;
  return `vec3(${r.toFixed(4)},${g.toFixed(4)},${b.toFixed(4)})`;
}

// Reflectivity transfer function for the voxel ray-march: the per-cell metadata
// value (dBZ, property = the quantity) → colour + alpha. The colour ramp is built
// from the SAME legend stops the colour-scale bar shows, so the voxels match the
// legend exactly; `min` (the `min dBZ` field) makes weaker cells (and the nodata
// sentinel) transparent; denser echo grows more opaque.
function voxelShader(quantity, legend, min) {
  // `quantity` is interpolated into GLSL source — guard the property name even
  // though it comes from server metadata (ODIM quantities are A-Z0-9_). Sandboxed
  // to our own GPU context, but cheap defence-in-depth.
  if (!/^[A-Z][A-Z0-9_]*$/.test(quantity)) throw new Error("invalid quantity: " + quantity);
  // Also guard non-finite min/max: `(NaN|Infinity).toFixed(2)` emits the
  // literal "NaN"/"Infinity" into the GLSL below → the shader fails to compile
  // and the volume goes dark. (serde can emit those for IEEE-754 specials.)
  const lg = (legend && legend.stops && legend.stops.length >= 2
              && Number.isFinite(legend.min) && Number.isFinite(legend.max))
    ? legend
    : { min: 0, max: 70, stops: [{ color: "#0000ff" }, { color: "#ff0000" }] };
  const n = lg.stops.length;
  const ramp = lg.stops.map(s => hexToGlslVec3(s.color)).join(",");
  const span = (lg.max - lg.min) || 1;
  const fs = [
    `vec3 voxRamp(float v){`,
    `  vec3 ramp[${n}]=vec3[${n}](${ramp});`,
    `  float t=clamp((v-(${lg.min.toFixed(2)}))/${span.toFixed(2)},0.0,1.0)*float(${n - 1});`,
    `  int i=int(floor(t));`,
    `  return mix(ramp[i],ramp[min(i+1,${n - 1})],fract(t));`,
    `}`,
    `void fragmentMain(FragmentInput fsInput, inout czm_modelMaterial material){`,
    `  float v=fsInput.metadata.${quantity};`,
    `  material.diffuse=voxRamp(v);`,
    // Opacity: a dead-band then a quadratic ramp to near-opaque (#383). The
    // encoder fills unmeasured cells with the −32 dBZ no-echo floor, so the
    // whole cylinder is a dense low-value field — the old slow linear ramp
    // (0.022/dBZ from `min`, capped at 0.55) let weak echo + interpolated floor
    // accumulate along every ray into a uniform milky haze while storm cores
    // never read solid. Instead: fully transparent until 3 dBZ above `min`
    // (kills the haze; still no hard alpha step — the quadratic rises smoothly
    // from 0, so no shell/"walls and floors" at the min isovalue), then
    // t² over the next 20 dBZ up to 0.95 so cores approach opacity.
    `  float t=clamp((v-(${(min + 3.0).toFixed(2)}))/20.0,0.0,1.0);`,
    `  material.alpha=t*t*0.95;`,
    `}`,
  ].join("\n");
  return new Cesium.CustomShader({ fragmentShaderText: fs });
}

// The voxel floor updates the transfer-function shader live (no reload):
// rebuild the customShader with the new threshold and reassign it.
function voxelClientFilter() {
  if (($rep.value || "points") !== "voxels" || currentRep !== "voxels" || !voxelPrimitive) return false;
  try { voxelPrimitive.customShader = voxelShader($qty.value, legendData, voxelMin()); } catch (e) { console.error(e); return false; }
  viewer.scene.requestRender();
  updateStatus(filterLabel());
  return true;
}

// Reveal frame `i` (wrapping), hide the rest — a pure visibility toggle, the
// no-stall core of playback.
function showFrame(i) {
  if (!frames.length) return;
  frameIdx = ((i % frames.length) + frames.length) % frames.length;
  for (let j = 0; j < frames.length; j++) if (frames[j]) frames[j].show = (j === frameIdx);
  $time.value = String(frameIdx);
  updateTimeLabel();
  updateStatus(filterLabel());
  viewer.scene.requestRender();
}

// Point-cloud style applied to EVERY frame so the whole animation filters/sizes
// consistently: drop points below `min` and, for reflectivity, scale each dot by
// its value (the per-point `value` batch property). Both read `${value}` (a mesh
// ignores both). Other quantities keep one dot size: their colour carries the
// value, and a velocity has no "strong" end to grow. `pointsStyleMin` remembers
// the applied min so frames still arriving from the progressive preload get
// the same style.
function stylePointFrame(ts, min) {
  const cond = Number.isFinite(min) ? "${value} >= " + min : "true";
  ts.style = new Cesium.Cesium3DTileStyle({
    show: cond,
    // Steep ramp so weak vs strong reads clearly: ~1 px at 5 dBZ up to 16 px at
    // 55 dBZ (slope 0.3). Weak clutter stays a speck; storm cores are fat dots.
    pointSize: isReflectivity() ? "clamp((${value} - 5.0) * 0.3 + 1.0, 1.0, 16.0)" : "2.0",
  });
}
function applyPointStyle(min) {
  pointsStyleMin = min;
  for (const ts of frames) if (ts) stylePointFrame(ts, min);
}

// The numeric field for a point cloud filters CLIENT-SIDE across all frames (no
// re-fetch) by restyling `show` — instant, since every point carries its value.
// We can only hide points we already have, so dropping below the fetched floor
// re-fetches the (larger) set; raising the threshold is always client-side.
function pointsClientFilter() {
  if (($rep.value || "points") !== "points" || currentRep !== "points" || !frames.some(isFrameReady)) return false;
  const min = parseFloat($num.value);
  if (!Number.isFinite(min) || pointsFetchFloor === null || min < pointsFetchFloor) return false;
  applyPointStyle(min);
  viewer.scene.requestRender();
  updateStatus(filterLabel());
  return true;
}

function onNumCommit() {
  syncUrl();
  // Points (client restyle) and voxels (shader rebuild) handle the field live →
  // nothing to fetch; everything else (mesh threshold/resolution) re-fetches.
  if (pointsClientFilter() || voxelClientFilter()) return;
  loadFrames(false);
}

// ── Time scrubber ──────────────────────────────────────────────────────────
// Show the bar only for a genuine time axis (>1 volume); size the slider to the
// frames and seek to the active one.
function syncTimeBar() {
  const n = frameTimes.length;
  // Hide for a single frame, and also when every frame failed to load (the
  // manifest had >1 time but nothing rendered) — no point showing a dead slider.
  $timebar.hidden = n <= 1 || !frames.some(isFrameReady);
  $time.max = String(Math.max(0, n - 1));
  $time.value = String(frameIdx);
  updateTimeLabel();
}

// Compact label for the active timestamp, e.g. `2026-05-15 00:05Z` and its
// position `(1/3)` (`2026-05-15T00:05:00Z` → drop the `T` and the trailing
// `:00` seconds). The position hides on a phone.
function updateTimeLabel() {
  const t = frameTimes[frameIdx];
  if (!t) { $timeLabel.textContent = ""; $timeCount.textContent = ""; return; }
  $timeLabel.textContent = t.replace("T", " ").replace(/:\d{2}Z$/, "Z");
  $timeCount.textContent = `(${frameIdx + 1}/${frameTimes.length})`;
}

// The selected option's text (a collection or quantity title), else its value.
function optionText($select) {
  const o = [...$select.options].find(o => o.value === $select.value);
  return (o && o.textContent) || $select.value;
}
// What is drawn first (quantity, view, then `extra`, e.g. the filter), the
// collection last: where the bar is narrow the ellipsis cuts the collection,
// which its select names, rather than the quantity and filter.
function statusLine(extra) {
  return [optionText($qty), optionText($rep) || "Point cloud", extra, optionText($coll)]
    .filter(Boolean).join(" · ");
}
function updateStatus(extra) {
  setStatus(statusLine(extra));
}

// Next loaded frame at-or-after `from` (wrapping): playback can start while the
// progressive preload is still filling, so skip slots that are null (failed or
// not yet loaded) rather than flashing empty frames.
function nextLoadedFrame(from) {
  for (let k = 0; k < frames.length; k++) {
    const i = (((from + k) % frames.length) + frames.length) % frames.length;
    if (isFrameReady(frames[i])) return i;
  }
  return from;
}

function startPlaying() {
  // Need >1 frame AND at least one that actually loaded — otherwise play would
  // loop forever showing nothing (every frame's fromUrl can fail → all null).
  if (playing || frames.length <= 1 || !frames.some(isFrameReady)) return;
  playing = true;
  $play.textContent = "⏸";
  playTimer = setInterval(() => showFrame(nextLoadedFrame(frameIdx + 1)), FRAME_MS);
}

function stopPlaying() {
  playing = false;
  $play.textContent = "▶";
  if (playTimer) { clearInterval(playTimer); playTimer = null; }
}

// Collection switch → drop the frame cache (inside loadCollectionMeta, once the
// new metadata fetch succeeds) and reframe. Every other control is an in-place
// change → preserve the camera AND the cache, so toggling back is instant.
// (Arrow wrappers are load-bearing: passing the handler bare would feed the DOM
// Event as `reframe`, which is truthy and would reframe on every filter tweak.)
$coll.addEventListener("change", () => loadCollectionMeta(true));
// A quantity carries its own unit, legend and default filter (#1137); the
// filter typed is kept while the unit stays the same.
$qty.addEventListener("change", () => { syncRepOptions(); syncNumField(true); loadFrames(false); });
// Switching representation relabels + re-defaults the numeric field, toggles the
// Resolution selector, then rebuilds the frames (camera preserved).
$rep.addEventListener("change", () => { syncNumField(); loadFrames(false); });
$res.addEventListener("change", () => loadFrames(false));
// Live: a point cloud re-filters client-side as you drag the field. Commit
// (`change`): re-fetch for meshes, or for points that need values below their
// fetched floor.
$num.addEventListener("input", () => { pointsClientFilter() || voxelClientFilter(); });
$num.addEventListener("change", onNumCommit);
// Time scrubber: play/pause toggles the loop; dragging the slider seeks (and
// pauses, so the frame you drag to stays put). Pausing or a finished seek
// writes the frame to the URL; playback does not. The URL follows the
// slider's `change` (released), not every `input` step of a drag: Safari
// throws once a page calls history.replaceState 100 times in 30 s.
$play.addEventListener("click", () => { ++playbackIntent; playing ? stopPlaying() : startPlaying(); if (!playing) syncUrl(); });
$time.addEventListener("input", () => { ++playbackIntent; stopPlaying(); showFrame(parseInt($time.value, 10) || 0); });
$time.addEventListener("change", () => syncUrl());
// Phone: the fields fold into a menu.
$menu.addEventListener("click", () => {
  const open = !$bar.classList.contains("open");
  $bar.classList.toggle("open", open);
  $menu.setAttribute("aria-expanded", String(open));
});
setInterval(refreshCollectionTimes, 60000);
document.addEventListener("visibilitychange", () => { if (!document.hidden) refreshCollectionTimes(); });
loadCesium();
loadCollections();
