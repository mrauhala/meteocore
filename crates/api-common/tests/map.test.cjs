// Run with: node --test crates/api-common/tests/map.test.cjs
// Exercise the shipped workbench map script with a stub DOM and MapLibre.
// No WebGL mock claims to validate drawing; these tests cover what the
// script prints and which sources, layers and paint properties it asks
// MapLibre for, in which order.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const script = fs.readFileSync(`${__dirname}/../src/workbench/map.js`, 'utf8');
// The map panel as the server renders it (api-common
// `map_panel_fixture_matches_the_rendered_markup` keeps it current).
const panel = fs.readFileSync(`${__dirname}/fixtures/map-panel.html`, 'utf8');
// Every query parameter the Maps map routes accept (api-maps).
const MAP_PARAMETERS = [...fs.readFileSync(`${__dirname}/../../api-maps/src/params.rs`, 'utf8')
  .match(/pub const MAP_PARAMETERS: &\[&str\] = &\[([\s\S]*?)\];/)[1].matchAll(/"([^"]+)"/g)].map(m => m[1]);

class ClassList {
  constructor() { this.names = new Set(); }
  add(...names) { names.forEach(n => this.names.add(n)); }
  remove(...names) { names.forEach(n => this.names.delete(n)); }
  toggle(name, force) { const on = force ?? !this.names.has(name); if (on) this.names.add(name); else this.names.delete(name); return on; }
  contains(name) { return this.names.has(name); }
}

class Element {
  constructor(tag) {
    this.tag = tag; this.tagName = tag.toUpperCase(); this.children = []; this.dataset = {}; this.attributes = {};
    this.className = ''; this.title = ''; this.textContent = ''; this.hidden = false; this.style = {};
    this.classList = new ClassList(); this.listeners = {}; this.parent = null; this.id = '';
  }
  append(...nodes) { nodes.forEach(node => { if (node instanceof Element) node.parent = this; }); this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = []; this.append(...nodes); }
  setAttribute(name, value) { this.attributes[name] = String(value); }
  removeAttribute(name) { delete this.attributes[name]; if (name === 'href') delete this.href; }
  getAttribute(name) { return name in this.attributes ? this.attributes[name] : null; }
  after(node) { this.next = node; }
  remove() {}
  addEventListener(type, listener) { (this.listeners[type] ||= []).push(listener); }
  dispatch(type) {
    const event = { type, target: this, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } };
    (this.listeners[type] || []).forEach(listener => listener(event));
    return event;
  }
  querySelector() { return null; }
  closest(selector) {
    for (let node = this; node; node = node.parent) {
      if (selector.startsWith('.') ? node.className.split(' ').includes(selector.slice(1)) : node.tag === selector) return node;
    }
    return null;
  }
  // Depth-first descendants, this element first.
  *walk() { yield this; for (const child of this.children) if (child instanceof Element) yield* child.walk(); }
}

class Select extends Element {
  constructor() { super('select'); this.selectedIndex = 0; }
  get options() { return this.children.filter(child => child.tag === 'option'); }
  get value() { return this.options[this.selectedIndex]?.value ?? ''; }
  set value(value) { this.selectedIndex = this.options.findIndex(option => option.value === value); }
  get selectedOptions() { const option = this.options[this.selectedIndex]; return option ? [option] : []; }
}

// A small HTML reader for the server's own markup: elements, attributes,
// text and entities. Enough for the panel, which has no comments or scripts.
function parse(html) {
  const root = new Element('#root');
  const VOID = new Set(['input', 'br', 'img', 'meta', 'link']);
  const decode = text => text.replace(/&(#x[0-9a-f]+|#\d+|amp|lt|gt|quot|apos);/gi, (_, e) =>
    e[0] === '#' ? String.fromCodePoint(e[1] === 'x' ? parseInt(e.slice(2), 16) : Number(e.slice(1)))
      : { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'" }[e.toLowerCase()]);
  let node = root;
  const token = /<(\/?)([a-z][a-z0-9-]*)([^>]*)>|([^<]+)/gi;
  for (let match; (match = token.exec(html));) {
    const [, close, tag, attrs, text] = match;
    if (text !== undefined) { node.children.push(decode(text)); continue; }
    if (close) { node = node.parent; continue; }
    const element = tag === 'select' ? new Select() : new Element(tag.toLowerCase());
    for (const [, name, , value = ''] of attrs.matchAll(/([a-z][a-z0-9-]*)(="([^"]*)")?/gi)) {
      const v = decode(value);
      if (name === 'class') element.className = v;
      else if (name === 'id') element.id = v;
      else if (name === 'hidden') element.hidden = true;
      else if (name === 'checked') element.checked = true;
      else if (name.startsWith('data-')) element.dataset[name.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = v;
      else if (['value', 'type', 'name', 'method', 'action'].includes(name)) element[name] = v;
      element.attributes[name] = v;
    }
    node.append(element);
    if (!VOID.has(element.tag)) node = element;
  }
  // Text content as the DOM reads it, fixed at load.
  const text = element => element.children.map(child => typeof child === 'string' ? child : text(child)).join('');
  for (const element of root.walk()) {
    element.textContent = text(element);
    if (element.tag === 'option' && !('value' in element.attributes)) element.value = element.textContent;
  }
  return root;
}

// MapLibre as far as the script uses it. Sources, layers (in draw order)
// and paint properties are kept; `log` lists every change in order, and
// `render()` stands for one frame: MapLibre draws, then fires `render`.
class StubMap {
  constructor(options) {
    this.options = options; this.handlers = {}; this.sources = new Map(); this.layers = [];
    this.loadedSources = new Set(); this.log = [];
    this.view = { west: 20, south: 55, east: 30, north: 65 }; this.zoom = 5; this.center = { lng: 25, lat: 60 };
    StubMap.last = this;
  }
  on(type, listener) { (this.handlers[type] ||= []).push(listener); return this; }
  off(type, listener) { this.handlers[type] = (this.handlers[type] || []).filter(l => l !== listener); return this; }
  once(type, listener) { const once = event => { this.off(type, once); listener(event); }; return this.on(type, once); }
  fire(type, event = {}) { [...(this.handlers[type] || [])].forEach(listener => listener(event)); }
  addControl() {}
  addSource(id, source) {
    assert.ok(!this.sources.has(id), `source ${id} added twice`);
    this.sources.set(id, { ...source, setTiles: () => this.log.push(['setTiles', id]) });
    this.log.push(['addSource', id]);
  }
  getSource(id) { return this.sources.get(id); }
  removeSource(id) {
    assert.ok(!this.layers.some(layer => layer.source === id), `source ${id} removed while a layer uses it`);
    this.sources.delete(id); this.log.push(['removeSource', id]);
  }
  addLayer(layer, before) {
    const copy = { ...layer, paint: { ...layer.paint } };
    const index = before ? this.layers.findIndex(l => l.id === before) : -1;
    if (index >= 0) this.layers.splice(index, 0, copy); else this.layers.push(copy);
    this.log.push(['addLayer', layer.id]);
  }
  getLayer(id) { return this.layers.find(layer => layer.id === id); }
  removeLayer(id) { this.layers = this.layers.filter(layer => layer.id !== id); this.log.push(['removeLayer', id]); }
  setPaintProperty(id, name, value) {
    const layer = this.getLayer(id);
    assert.ok(layer, `paint on missing layer ${id}`);
    layer.paint[name] = value; this.log.push(['paint', id, name, value]);
  }
  isSourceLoaded(id) { return this.loadedSources.has(id); }
  triggerRepaint() {}
  getBounds() { const v = this.view; return { getWest: () => v.west, getSouth: () => v.south, getEast: () => v.east, getNorth: () => v.north }; }
  getZoom() { return this.zoom; }
  getCenter() { return this.center; }
  jumpTo({ center, zoom }) { this.center = { lng: center[0], lat: center[1] }; this.zoom = zoom; this.log.push(['jumpTo', `${zoom}/${center[1]}/${center[0]}`]); }
  fitBounds() { this.log.push(['fitBounds']); }
  project() { return { x: 0, y: 0 }; }
  resize() {}
  queryRenderedFeatures() { return []; }
  render() { this.fire('render'); }
  // The data layers in draw order, with their opacity.
  rasters() { return this.layers.filter(layer => layer.type === 'raster'); }
  visible() { return this.rasters().filter(layer => layer.paint['raster-opacity'] > 0).map(layer => layer.id); }
  // A tile of `id` arrived, and every tile in view is in.
  tilesLoaded(id) { this.fire('sourcedata', { sourceId: id, tile: {} }); this.loadedSources.add(id); }
}

class LngLatBounds {
  constructor() { this.points = []; }
  extend(point) { this.points.push(point); return this; }
  isEmpty() { return !this.points.length; }
  getWest() { return Math.min(...this.points.map(p => p[0])); }
  getEast() { return Math.max(...this.points.map(p => p[0])); }
  toArray() { return [[this.getWest(), Math.min(...this.points.map(p => p[1]))], [this.getEast(), Math.max(...this.points.map(p => p[1]))]]; }
}

const settle = async () => { for (let i = 0; i < 20; i++) await new Promise(resolve => setImmediate(resolve)); };

function respond(status, { json, headers = {} } = {}) {
  const lower = Object.fromEntries(Object.entries(headers).map(([k, v]) => [k.toLowerCase(), v]));
  return { ok: status >= 200 && status < 300, status, headers: { get: name => lower[name.toLowerCase()] ?? null }, json: async () => json, blob: async () => ({}) };
}
const image = drawn => respond(200, { headers: { 'Content-Type': 'image/png', 'Content-Datetime': drawn } });

// A collection page with the fixture's map panel, the script run on it and
// MapLibre's style loaded. `maps` answers each Maps image request.
async function page({ hash = '', dpr = 1, mode, maps = () => image('2024-01-01T02:00:00Z'), edit } = {}) {
  const root = parse(panel);
  if (edit) edit(root);
  const byId = id => { for (const element of root.walk()) if (element.id === id) return element; return null; };
  const map = byId('feature-map');
  map.clientWidth = 800; map.clientHeight = 480;
  const requests = [];
  const timers = new Map();
  let clock = 0, timerId = 0;
  const location = { href: 'https://x/collections/nwp', origin: 'https://x', pathname: '/collections/nwp', search: '', hash };
  const writes = [];
  // The page's clock is the fake one the timers run on.
  const start = Date.parse('2026-10-19T00:00:00Z');
  class PageDate extends Date { static now() { return start + clock; } }
  const writeTimes = [];
  const history = { state: null, replaceState(state, title, url) { writes.push(url); writeTimes.push(clock); location.hash = url; } };
  const fetch = async (url, init) => {
    url = String(url); requests.push(url);
    if (url.endsWith('/map/tiles')) {
      const base = url.replace(/\/tiles$/, '');
      return respond(200, { json: { tilesets: [{ dataType: 'map', tileMatrixSetURI: 'http://www.opengis.net/def/tilematrixset/OGC/1.0/WebMercatorQuad',
        links: [{ rel: 'self', href: `${base}/tiles/WebMercatorQuad` }, { rel: 'item', href: `${base}/tiles/WebMercatorQuad/{tileMatrix}/{tileRow}/{tileCol}` }] }] } });
    }
    if (url.includes('/legend')) return respond(200, { json: { stops: [{ value: 0, color: '#000000' }, { value: 1, color: '#ffffff' }], min: 0, max: 1, parameter: 't', unit: 'K' } });
    if (/\/map\?/.test(url)) { if (init?.signal?.aborted) throw Object.assign(new Error('aborted'), { name: 'AbortError' }); return maps(new URL(url)); }
    return respond(404);
  };
  const document = {
    readyState: 'complete',
    documentElement: Object.assign(new Element('html'), { dataset: { theme: 'light' } }),
    getElementById: byId,
    createElement: tag => Object.assign(new Element(tag), { getContext: () => ({ drawImage() {}, measureText: () => ({ width: 10 }) }) }),
    querySelectorAll: selector => {
      const name = /^input\[name="([^"]+)"\]$/.exec(selector)?.[1];
      return name ? [...root.walk()].filter(element => element.tag === 'input' && element.name === name) : [];
    },
    addEventListener() {},
  };
  const context = vm.createContext({
    document, location, history, fetch, URL, URLSearchParams, AbortController, Date: PageDate,
    AbortSignal: { timeout: () => new AbortController().signal },
    maplibregl: { Map: StubMap, NavigationControl: class {}, ScaleControl: class {}, LngLatBounds, Marker: class {} },
    getComputedStyle: () => ({ getPropertyValue: () => '' }),
    ResizeObserver: class { observe() {} },
    createImageBitmap: async () => ({ width: 2, height: 2, close() {} }),
    devicePixelRatio: dpr,
    setTimeout: (fn, ms = 0) => { timers.set(++timerId, { fn, at: clock + ms }); return timerId; },
    clearTimeout: id => { timers.delete(id); },
  });
  vm.runInContext(script, context);
  const stub = StubMap.last;
  stub.fire('style.load');
  await settle();
  // Advance the fake clock, running each timer when it falls due.
  async function wait(ms) {
    const end = clock + ms;
    for (;;) {
      const due = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      timers.delete(due[0]); clock = Math.max(clock, due[1].at); due[1].fn(); await settle();
    }
    clock = end;
  }
  // The viewer picks a draw mode.
  if (mode) {
    const radios = [...root.walk()].filter(e => e.name === 'map-mode');
    radios.forEach(radio => { radio.checked = radio.value === mode; });
    radios.find(radio => radio.checked).dispatch('change');
    await settle();
  }
  const change = async (id, value) => { const element = byId(id); element.value = value; element.dispatch('change'); await settle(); };
  const mapRequests = () => requests.filter(url => /\/map\?/.test(url)).map(url => new URL(url));
  return { root, map: stub, byId, change, wait, requests, mapRequests, writes, writeTimes, location, status: byId('map-status'), legend: byId('map-legend') };
}

// The only frame staged, and the one drawn, after it has fully loaded.
function showFirst(p) {
  const [first] = p.map.rasters();
  assert.ok(first, 'nothing was staged');
  p.map.tilesLoaded(first.id); p.map.render();
  assert.deepEqual(p.map.visible(), [first.id]);
  return first.id;
}

// The swap: the old frame stays drawn until the new source has loaded;
// then one render sets the new layer to the data opacity and the old to 0,
// and the old is removed only after that paint.
function assertSwap(p, old) {
  const staged = p.map.rasters().map(l => l.id).find(id => id !== old);
  assert.ok(staged, 'no new frame was staged');
  const layer = p.map.getLayer(staged);
  assert.equal(layer.paint['raster-opacity'], 0);
  assert.equal(JSON.stringify(layer.paint['raster-opacity-transition']), '{"duration":0,"delay":0}');
  assert.equal(layer.paint['raster-fade-duration'], 0);
  assert.deepEqual(p.map.visible(), [old]);
  p.map.render();
  assert.deepEqual(p.map.visible(), [old], 'swapped before the new source loaded');
  p.map.tilesLoaded(staged);
  const mark = p.map.log.length;
  p.map.render();
  const swap = p.map.log.slice(mark).map(entry => entry.join(' '));
  assert.deepEqual(swap, [`paint ${staged} raster-opacity 0.85`, `paint ${old} raster-opacity 0`]);
  assert.deepEqual(p.map.visible(), [staged]);
  assert.equal(JSON.stringify(p.map.getLayer(old).paint['raster-opacity-transition']), '{"duration":0,"delay":0}');
  p.map.render();
  assert.ok(!p.map.getLayer(old) && !p.map.getSource(old), 'the old frame outlived the swap');
  assert.ok(!p.map.log.some(entry => entry[0] === 'setTiles'), 'a visible source was retargeted');
  return staged;
}

// The quick look of an item page whose one feature carries `facts`, as
// api-features embeds them: the JSON text the server writes, `135.0` included.
function quickLook(factsJson) {
  const status = new Element('p');
  const element = new Element('div');
  Object.assign(element.dataset, { quicklook: 'true', land: '', fallback: '' });
  const data = new Element('div');
  data.textContent = `{"type":"FeatureCollection","features":[{"type":"Feature","id":"a","geometry":{"type":"Point","coordinates":[24.9,60.2]},"properties":{"label":"Station","facts":${factsJson}}}]}`;
  const elements = { 'map-status': status, 'feature-map': element, 'map-data': data };
  const document = {
    readyState: 'complete',
    documentElement: new Element('html'),
    getElementById: id => elements[id] || null,
    createElement: tag => new Element(tag),
    querySelectorAll: () => [],
    addEventListener() {},
  };
  const maplibregl = { Map: class { on() {} addControl() {} }, NavigationControl: class {}, ScaleControl: class {} };
  const context = vm.createContext({
    document, maplibregl, URL,
    location: { href: 'https://x/items/a', origin: 'https://x', hash: '' },
    getComputedStyle: () => ({ getPropertyValue: () => '' }),
  });
  vm.runInContext(script, context);
  // status → selection → inspector
  const inspector = status.next.next;
  assert.equal(inspector.className, 'mini-inspector', 'no quick look was drawn');
  const facts = inspector.children.find(child => child.className === 'mini-facts');
  return Object.fromEntries(facts.children.map(fact => {
    const [key, value] = fact.children;
    return [key.textContent, { text: value.textContent, title: value.title, className: value.className }];
  }));
}

test('integral floats show without .0, like the property tables', () => {
  const facts = quickLook('{"value":135.0,"zero":-0.0,"count":42,"half":2.5,"long":24.933333333333334}');
  assert.equal(facts.value.text, '135');
  assert.equal(facts.zero.text, '0');
  assert.equal(facts.count.text, '42');
  assert.equal(facts.half.text, '2.5');
  // Only a latitude or longitude is rounded.
  assert.equal(facts.long.text, '24.933333333333334');
  for (const fact of Object.values(facts)) assert.equal(fact.title, '');
});

test('a latitude or longitude shows 5 decimals, its exact value in the title', () => {
  const facts = quickLook('{"lat":24.933333333333334,"Longitude":24.933333333333334,"station_lat":24.933333333333334,"LON":24.933333333333334,"latitude":60.123456789,"lng":60.1,"lon":25.0,"wind_lon":-0.000001}');
  for (const key of ['lat', 'Longitude', 'station_lat', 'LON']) {
    assert.deepEqual(facts[key], { text: '24.93333', title: '24.933333333333334', className: 'rounded' }, key);
  }
  assert.deepEqual(facts.latitude, { text: '60.12346', title: '60.123456789', className: 'rounded' });
  // Nothing lost: no title.
  assert.deepEqual(facts.lng, { text: '60.1', title: '', className: '' });
  assert.deepEqual(facts.lon, { text: '25', title: '', className: '' });
  assert.deepEqual(facts.wind_lon, { text: '0', title: '-0.000001', className: 'rounded' });
});

// Expected texts are Rust's `format!("{:.5}")`, which ds-core's
// `display_coordinate` uses: an exact tie rounds to even.
test('a coordinate on an exact tie rounds to even, as ds-core does', () => {
  const facts = quickLook('{"lat":60.015625,"lon":-24.078125,"y_lat":0.046875,"x_lon":0.015625}');
  assert.deepEqual(facts.lat, { text: '60.01562', title: '60.015625', className: 'rounded' });
  assert.deepEqual(facts.lon, { text: '-24.07812', title: '-24.078125', className: 'rounded' });
  assert.equal(facts.y_lat.text, '0.04688');
  assert.equal(facts.x_lon.text, '0.01562');
});

test('other values print as before', () => {
  const facts = quickLook('{"name":"Helsinki","missing":null,"flag":true}');
  assert.equal(facts.name.text, 'Helsinki');
  assert.equal(facts.missing.text, 'Not available');
  assert.equal(facts.flag.text, 'true');
});

// The view an items page's "Use map view" takes (#1143): the map element's
// `data-view` after a move, for MapLibre bounds west, south, east, north.
function viewAfterMove(west, south, east, north) {
  const status = new Element('p');
  const element = new Element('div');
  Object.assign(element.dataset, { quicklook: 'true', land: '', fallback: '' });
  const data = new Element('div');
  data.textContent = '{"type":"FeatureCollection","features":[]}';
  const elements = { 'map-status': status, 'feature-map': element, 'map-data': data };
  const handlers = {};
  class MovingMap {
    on(event, handler) { handlers[event] = handler; }
    addControl() {}
    getBounds() {
      return { getWest: () => west, getSouth: () => south, getEast: () => east, getNorth: () => north };
    }
  }
  const document = {
    readyState: 'complete',
    documentElement: new Element('html'),
    getElementById: id => elements[id] || null,
    createElement: tag => new Element(tag),
    querySelectorAll: () => [],
    addEventListener() {},
  };
  const maplibregl = { Map: MovingMap, NavigationControl: class {}, ScaleControl: class {} };
  const context = vm.createContext({
    document, maplibregl, URL,
    location: { href: 'https://x/items', origin: 'https://x', hash: '' },
    getComputedStyle: () => ({ getPropertyValue: () => '' }),
  });
  vm.runInContext(script, context);
  handlers.moveend();
  return element.dataset.view;
}

test('the map view is a CRS84 box, longitudes within ±180', () => {
  assert.equal(viewAfterMove(19.123456, 59.5, 31.7, 70.25), '19.1235,59.5,31.7,70.25');
  // Panned a turn east: the same box.
  assert.equal(viewAfterMove(379.123456, 59.5, 391.7, 70.25), '19.1235,59.5,31.7,70.25');
  // Across the antimeridian: west > east, as Features `bbox` takes it.
  assert.equal(viewAfterMove(170, -20, 190, 10), '170,-20,-170,10');
  // Wider than the world: all of it.
  assert.equal(viewAfterMove(-300, -85, 200, 85), '-180,-85,180,85');
  // An edge on the antimeridian keeps its side.
  assert.equal(viewAfterMove(100, 0, 180, 10), '100,0,180,10');
});

// #1220: the collection page's map.

test('map tiles draw by default; the status names no time before a tile arrives', async () => {
  const p = await page();
  assert.equal(p.map.options.cooperativeGestures, true);
  const [staged] = p.map.rasters();
  const source = p.map.getSource(staged.id);
  assert.equal(source.type, 'raster');
  assert.equal(source.tileSize, 256);
  assert.equal(source.tiles[0], 'https://x/collections/nwp/map/tiles/WebMercatorQuad/{z}/{y}/{x}');
  assert.match(p.status.textContent, /^Loading map tiles…$/);
  showFirst(p);
  assert.match(p.status.textContent, /^Map tiles · Showing the default time/);
});

test('map tiles: a time step keeps the drawn source until the next has loaded, then swaps in one paint', async () => {
  const p = await page();
  const first = showFirst(p);
  // Default stands for the latest time: one step back is the one before.
  p.byId('map-time-prev').dispatch('click');
  await settle();
  assert.match(p.status.textContent, /^Loading map tiles… Still showing the default time · Temperature \(K\)\.$/);
  const next = assertSwap(p, first);
  const query = template => new URL(template.replace(/[{}]/g, '')).search;
  assert.equal(query(p.map.getSource(next).tiles[0]), '?datetime=2024-01-01T01%3A00%3A00Z');
  assert.match(p.status.textContent, /^Map tiles · Showing 2024-01-01 01:00 UTC · Temperature \(K\)$/);
  // Each choice applies at once and swaps the same way.
  await p.change('map-parameter', 'r');
  const third = assertSwap(p, next);
  assert.equal(query(p.map.getSource(third).tiles[0]), '?parameter-name=r&datetime=2024-01-01T01%3A00%3A00Z');
});

test('Maps images: a time step keeps the drawn image until the next has loaded, then swaps in one paint', async () => {
  const p = await page({ mode: 'maps' });
  const first = showFirst(p);
  assert.equal(p.map.getSource(first).type, 'canvas');
  assert.match(p.status.textContent, /^Showing 2024-01-01 02:00 UTC/);
  await p.change('map-time', '2024-01-01T01:00:00Z');
  assert.equal(p.mapRequests().at(-1).searchParams.get('datetime'), '2024-01-01T01:00:00Z');
  assertSwap(p, first);
});

test('a failed request keeps the frame and says why; a 503 retries after Retry-After, at most three times', async () => {
  let answer = () => image('2024-01-01T02:00:00Z');
  const p = await page({ mode: 'maps', maps: url => answer(url) });
  const first = showFirst(p);
  // A pan whose image fails: the frame stays, marked stale.
  answer = () => respond(500, { json: { description: 'Internal server error' } });
  p.map.view = { west: 21, south: 56, east: 31, north: 66 };
  p.map.fire('moveend');
  await p.wait(300);
  p.map.render();
  assert.deepEqual(p.map.visible(), [first]);
  assert.equal(p.status.textContent, 'No data available for this collection right now. Still showing 2024-01-01 02:00 UTC · Temperature (K).');
  assert.ok(p.status.classList.contains('stale'));
  // No data at the chosen time: said so, and the frame stays.
  answer = () => respond(404, { json: { description: 'No data' } });
  await p.change('map-time', '2024-01-01T00:00:00Z');
  p.map.render();
  assert.deepEqual(p.map.visible(), [first]);
  assert.equal(p.status.textContent, 'No data at 2024-01-01 00:00 UTC. Choose another time or Default. Still showing 2024-01-01 02:00 UTC · Temperature (K).');
  assert.ok(p.status.classList.contains('stale'));
  // The time row stays: another time may have data.
  assert.equal(p.byId('map-time').closest('.map-time-field').hidden, false);
  // Busy: wait as told, then back off, three retries in all.
  answer = () => respond(503, { headers: { 'Retry-After': '2' }, json: { description: 'Server busy, try again later' } });
  const before = p.mapRequests().length;
  await p.change('map-time', '2024-01-01T01:00:00Z');
  assert.equal(p.status.textContent, 'Server busy, retrying in 2 s… Still showing 2024-01-01 02:00 UTC · Temperature (K).');
  await p.wait(1999);
  assert.equal(p.mapRequests().length, before + 1);
  await p.wait(1);
  assert.equal(p.mapRequests().length, before + 2);
  await p.wait(60000);
  assert.equal(p.mapRequests().length, before + 4);
  assert.deepEqual(p.map.visible(), [first]);
  assert.match(p.status.textContent, /^The server is busy\. Pan, zoom or change a choice to try again\. Still showing/);
  assert.ok(!/Update map/.test(p.status.textContent));
  // A retry that succeeds swaps as usual.
  answer = () => image('2024-01-01T01:00:00Z');
  await p.change('map-time', '2024-01-01T00:00:00Z');
  assertSwap(p, first);
  assert.ok(!p.status.classList.contains('stale'));
});

test('map tiles that all fail keep the frame; none in view is no data', async () => {
  const p = await page();
  const first = showFirst(p);
  await p.change('map-time', '2024-01-01T00:00:00Z');
  const [, staged] = p.map.rasters();
  assert.ok(staged, 'the change staged no frame');
  // A 404 tile is no event in MapLibre: nothing loaded, nothing failed.
  p.map.loadedSources.add(staged.id);
  p.map.render();
  assert.deepEqual(p.map.visible(), [first]);
  assert.ok(!p.map.getSource(staged.id));
  assert.match(p.status.textContent, /^No data at 2024-01-01 00:00 UTC\. Choose another time or Default\. Still showing/);
});

test('changing a select re-renders at once, with Maps parameter names only', async () => {
  const p = await page({ mode: 'maps' });
  showFirst(p);
  const before = p.mapRequests().length;
  // No Update button: the change itself requests the map.
  const level = p.byId('map-level');
  level.value = '850'; level.dispatch('change');
  assert.equal(p.mapRequests().length, before + 1);
  assert.equal(p.mapRequests().at(-1).searchParams.get('elevation'), '850');
  await p.change('map-parameter', 't');
  for (const url of p.mapRequests()) {
    for (const key of url.searchParams.keys()) assert.ok(MAP_PARAMETERS.includes(key), key);
  }
});

test('a pan does not pick up anything uncommitted', async () => {
  const p = await page({ mode: 'maps' });
  showFirst(p);
  // Edited but not committed: no change event yet.
  p.byId('map-parameter').value = 'r';
  p.byId('map-time').value = '2024-01-01T00:00:00Z';
  p.map.view = { west: 21, south: 56, east: 31, north: 66 };
  p.map.fire('moveend');
  await p.wait(300);
  const last = p.mapRequests().at(-1);
  assert.equal(last.searchParams.get('bbox'), '21,56,31,66');
  assert.equal(last.searchParams.get('parameter-name'), null);
  assert.equal(last.searchParams.get('datetime'), null);
});

test('a HiDPI screen gets device pixels, within about 4 Mpx, and 512-pixel tiles', async () => {
  const p = await page({ mode: 'maps', dpr: 2 });
  const url = p.mapRequests().at(-1);
  assert.equal(url.searchParams.get('width'), '1600');
  assert.equal(url.searchParams.get('height'), '960');
  const big = await page({ mode: 'maps', dpr: 3 });
  big.byId('feature-map').clientWidth = 2000;
  big.map.view = { west: 0, south: 50, east: 40, north: 70 };
  big.map.fire('moveend');
  await big.wait(300);
  const sized = big.mapRequests().at(-1);
  const [w, h] = ['width', 'height'].map(k => Number(sized.searchParams.get(k)));
  assert.ok(w * h <= 4 * 1024 * 1024 && w * h > 4e6, `${w}x${h}`);
  assert.ok(Math.abs(w / h - 2000 / 480) < 0.01);
  const tiles = await page({ dpr: 2 });
  assert.match(tiles.map.getSource(tiles.map.rasters()[0].id).tiles[0], /\?width=512&height=512$/);
});

test('the legend stays, dimmed, while the next loads; each is fetched once', async () => {
  const p = await page();
  showFirst(p);
  const legendRequests = () => p.requests.filter(url => url.includes('/legend'));
  assert.equal(legendRequests().length, 1);
  const bar = p.byId('map-legend-scale').children[0];
  assert.equal(bar.style.opacity, '0.85');
  // Another parameter: the old legend keeps its place until the new one is in.
  p.byId('map-parameter').value = 'r';
  p.byId('map-parameter').dispatch('change');
  assert.ok(p.legend.classList.contains('stale'));
  assert.equal(p.byId('map-legend-scale').hidden, false);
  assert.equal(p.byId('map-legend-status').hidden, true);
  await settle();
  assert.ok(!p.legend.classList.contains('stale'));
  // Back to the first: from the page's cache.
  await p.change('map-parameter', '');
  assert.equal(legendRequests().length, 2);
});

test('the fragment restores the view and choices, and follows them', async () => {
  const hash = '#map=6/61.5/24.25&parameter-name=r&style=warm&datetime=2024-01-01T01:00:00Z&elevation=850&mode=maps';
  const p = await page({ hash });
  assert.deepEqual(p.map.log.filter(e => ['jumpTo', 'fitBounds'].includes(e[0])).map(e => e.join(' ')), ['jumpTo 6/61.5/24.25']);
  assert.equal(p.byId('map-parameter').value, 'r');
  assert.equal(p.byId('map-style').selectedIndex, 1);
  assert.equal(p.byId('map-time').value, '2024-01-01T01:00:00Z');
  assert.equal(p.byId('map-level').value, '850');
  const url = p.mapRequests().at(-1);
  assert.equal(url.pathname, '/collections/nwp/styles/warm/map');
  assert.equal(url.searchParams.get('parameter-name'), 'r');
  assert.equal(url.searchParams.get('datetime'), '2024-01-01T01:00:00Z');
  assert.equal(url.searchParams.get('elevation'), '850');
  // Nothing is written until the viewer moves or chooses.
  await p.wait(5000);
  assert.deepEqual(p.writes, []);
  p.map.center = { lng: 25.5, lat: 62.123456 }; p.map.zoom = 7.25; p.map.view = { west: 22, south: 60, east: 29, north: 64 };
  p.map.fire('moveend');
  await p.change('map-level', '1000');
  await p.wait(1000);
  // One write for both, through replaceState.
  assert.deepEqual(p.writes, ['#map=7.25/62.1235/25.5&parameter-name=r&style=warm&datetime=2024-01-01T01:00:00Z&elevation=1000&mode=maps']);
  // Its keys are Maps parameter names, besides the view, style and mode.
  for (const key of new URLSearchParams(p.writes[0].slice(1)).keys()) {
    assert.ok(MAP_PARAMETERS.includes(key) || ['map', 'style', 'mode'].includes(key), key);
  }
  // A reload of that URL reopens the same.
  const again = await page({ hash: p.writes[0] });
  assert.deepEqual(again.map.log.filter(e => e[0] === 'jumpTo').map(e => e[1]), ['7.25/62.1235/25.5']);
  assert.equal(again.byId('map-level').value, '1000');
  for (const key of ['parameter-name', 'datetime', 'elevation']) {
    assert.equal(again.mapRequests().at(-1).searchParams.get(key), p.mapRequests().at(-1).searchParams.get(key), key);
  }
  assert.equal(again.mapRequests().at(-1).pathname, '/collections/nwp/styles/warm/map');
  // At most about once a second, however fast the choices come.
  for (let i = 0; i < 30; i++) {
    await p.change('map-level', i % 2 ? '850' : '1000');
    await p.wait(100);
  }
  await p.wait(2000);
  assert.ok(p.writes.length >= 3 && p.writes.length <= 5, `${p.writes.length} writes`);
  p.writeTimes.slice(1).forEach((time, i) => assert.ok(time - p.writeTimes[i] >= 1000, `${p.writeTimes}`));
  assert.match(p.writes.at(-1), /&elevation=850&mode=maps$/);
  const written = p.writes.length;
  // Another tab's fragment is left alone.
  p.location.hash = '#metadata';
  await p.change('map-level', '');
  await p.wait(5000);
  assert.equal(p.writes.length, written);
  assert.equal(p.location.hash, '#metadata');
});

test('the form fields are Maps parameters; the draw switch and the style select are not fields', async () => {
  const p = await page();
  const form = p.byId('map-form');
  assert.equal(form.method, 'get');
  assert.equal(form.action, 'https://x/collections/nwp/map');
  const fields = [...form.walk()].filter(e => ['input', 'select', 'button'].includes(e.tag) && e.name);
  assert.deepEqual(fields.map(e => e.name).sort(), ['datetime', 'elevation', 'parameter-name']);
  for (const field of fields) assert.ok(MAP_PARAMETERS.includes(field.name), field.name);
  // The script submits nothing: Enter in a field applies, the page stays.
  assert.equal(form.dispatch('submit').defaultPrevented, true);
  assert.ok(!p.byId('map-style').name);
  const switches = [...p.root.walk()].filter(e => e.name === 'map-mode');
  assert.equal(switches.length, 2);
  assert.ok(switches.every(radio => !radio.closest('form')));
});

test('a date and time typed in applies on change, not before', async () => {
  const p = await page({
    mode: 'maps',
    edit: root => {
      const select = [...root.walk()].find(e => e.id === 'map-time');
      const input = Object.assign(new Element('input'), { id: 'map-time', name: 'datetime', type: 'datetime-local', value: '' });
      const picker = select.parent;
      picker.children = picker.children.filter(child => child.tag !== 'button').map(child => child === select ? input : child);
      input.parent = picker;
    },
  });
  showFirst(p);
  const before = p.mapRequests().length;
  const input = p.byId('map-time');
  input.value = '2024-01-01T01:00';
  assert.equal(p.mapRequests().length, before);
  input.dispatch('change');
  assert.equal(p.mapRequests().at(-1).searchParams.get('datetime'), '2024-01-01T01:00:00Z');
});

test('one style is no choice: the style select is hidden', async () => {
  const p = await page({
    edit: root => {
      const select = [...root.walk()].find(e => e.id === 'map-style');
      select.children = select.children.filter(child => child.dataset?.id !== 'warm');
    },
  });
  assert.equal(p.byId('map-style').closest('label').hidden, true);
  const two = await page();
  assert.equal(two.byId('map-style').closest('label').hidden, false);
});
