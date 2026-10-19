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
  removeEventListener(type, listener) { this.listeners[type] = (this.listeners[type] || []).filter(l => l !== listener); }
  // An event on this element, bubbling up through its ancestors.
  dispatch(type, props = {}) {
    const event = { type, target: this, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; }, ...props };
    for (let node = this; node; node = node.parent) {
      event.currentTarget = node;
      [...(node.listeners[type] || [])].forEach(listener => listener(event));
    }
    return event;
  }
  getBoundingClientRect() { return { left: 0, width: this.clientWidth || 600 }; }
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
    this.loadedSources = new Set(); this.log = []; this.added = [];
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
    this.added.push([id, source]);
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

function respond(status, { json, text = '', headers = {} } = {}) {
  const lower = Object.fromEntries(Object.entries(headers).map(([k, v]) => [k.toLowerCase(), v]));
  return { ok: status >= 200 && status < 300, status, headers: { get: name => lower[name.toLowerCase()] ?? null }, json: async () => json, text: async () => text, blob: async () => ({}) };
}
const image = drawn => respond(200, { headers: { 'Content-Type': 'image/png', 'Content-Datetime': drawn } });

// A collection page with the fixture's map panel, the script run on it and
// MapLibre's style loaded. `maps` and `wms` answer each Maps image and WMS
// GetMap request; `mapData` edits the panel's `mapRequest` first; `now` is
// the viewer's clock; `connection` is `navigator.connection`.
async function page({ hash = '', dpr = 1, mode, maps = url => image(url.searchParams.get('datetime') || '2024-01-01T02:00:00Z'), wms = () => image(), edit, mapData,
  now = '2026-10-19T00:00:00Z', connection, size = [800, 480] } = {}) {
  const root = parse(panel);
  if (edit) edit(root);
  const byId = id => { for (const element of root.walk()) if (element.id === id) return element; return null; };
  if (mapData) {
    const holder = byId('map-data');
    const data = JSON.parse(holder.textContent);
    mapData(data.mapRequest);
    holder.textContent = JSON.stringify(data);
  }
  const map = byId('feature-map');
  [map.clientWidth, map.clientHeight] = size;
  const requests = [];
  const timers = new Map();
  let clock = 0, timerId = 0, inFlight = 0;
  const location = { href: 'https://x/collections/nwp', origin: 'https://x', pathname: '/collections/nwp', search: '', hash };
  const writes = [];
  // The page's clock is the fake one the timers run on.
  const start = Date.parse(now);
  class PageDate extends Date { static now() { return start + clock; } }
  const writeTimes = [];
  const history = { state: null, replaceState(state, title, url) { writes.push(url); writeTimes.push(clock); location.hash = url; } };
  const aborted = () => Object.assign(new Error('aborted'), { name: 'AbortError' });
  const signals = [];
  const fetch = async (url, init) => {
    url = String(url); requests.push(url);
    if (url.endsWith('/map/tiles')) {
      const base = url.replace(/\/tiles$/, '');
      return respond(200, { json: { tilesets: [{ dataType: 'map', tileMatrixSetURI: 'http://www.opengis.net/def/tilematrixset/OGC/1.0/WebMercatorQuad',
        links: [{ rel: 'self', href: `${base}/tiles/WebMercatorQuad` }, { rel: 'item', href: `${base}/tiles/WebMercatorQuad/{tileMatrix}/{tileRow}/{tileCol}` }] }] } });
    }
    if (url.includes('/legend')) return respond(200, { json: { stops: [{ value: 0, color: '#000000' }, { value: 1, color: '#ffffff' }], min: 0, max: 1, parameter: 't', unit: 'K' } });
    if (/\/(map|wms)\?/.test(url)) {
      const signal = init?.signal;
      if (signal?.aborted) throw aborted();
      signals.push({ url, signal });
      inFlight++;
      try {
        return await new Promise((resolve, reject) => {
          signal?.addEventListener('abort', () => reject(aborted()));
          Promise.resolve((url.includes('/wms?') ? wms : maps)(new URL(url))).then(response => resolve({ ...response, blob: async () => ({ url }) }), reject);
        });
      } finally { inFlight--; }
    }
    return respond(404);
  };
  const documentListeners = {};
  const document = {
    readyState: 'complete',
    visibilityState: 'visible',
    documentElement: Object.assign(new Element('html'), { dataset: { theme: 'light' } }),
    getElementById: byId,
    createElement: tag => {
      const element = new Element(tag);
      element.getContext = () => ({ drawImage(bitmap) { element.drawnUrl = bitmap.url; }, measureText: () => ({ width: 10 }) });
      return element;
    },
    querySelectorAll: selector => {
      const name = /^input\[name="([^"]+)"\]$/.exec(selector)?.[1];
      return name ? [...root.walk()].filter(element => element.tag === 'input' && element.name === name) : [];
    },
    addEventListener(type, listener) { (documentListeners[type] ||= []).push(listener); },
  };
  let intersection;
  const context = vm.createContext({
    document, location, history, fetch, URL, URLSearchParams, AbortController, Date: PageDate,
    AbortSignal: { timeout: () => new AbortController().signal },
    maplibregl: { Map: StubMap, NavigationControl: class {}, ScaleControl: class {}, LngLatBounds, Marker: class {},
      // MapLibre's Web Mercator: x and y in [0, 1] over the world.
      MercatorCoordinate: { fromLngLat: ([lng, lat]) => ({ x: (180 + lng) / 360, y: (180 - 180 / Math.PI * Math.log(Math.tan(Math.PI / 4 + lat * Math.PI / 360))) / 360 }) } },
    getComputedStyle: () => ({ getPropertyValue: () => '' }),
    ResizeObserver: class { observe() {} },
    IntersectionObserver: class { constructor(callback) { intersection = callback; } observe() {} },
    navigator: { connection },
    createImageBitmap: async blob => ({ width: 2, height: 2, close() {}, url: blob.url }),
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
  // The time a raster source draws: its tile template's `datetime`, or the
  // `datetime` or `TIME` of the request its image came from.
  function sourceTime(id) {
    const source = stub.getSource(id);
    const url = source?.tiles ? source.tiles[0].replace(/[{}]/g, '') : source?.canvas?.drawnUrl;
    if (!url) return undefined;
    const params = new URL(url).searchParams;
    return params.get('datetime') || params.get('TIME');
  }
  // Frames whose time is in `held` keep loading; every other source loads.
  const held = new Set();
  function loadAll() {
    for (const id of stub.sources.keys()) {
      if (/^map-frame-/.test(id) && !stub.loadedSources.has(id) && !held.has(sourceTime(id))) stub.tilesLoaded(id);
    }
  }
  // Run the page for `ms` as a browser would: MapLibre draws, every source
  // not held loads, and timers fall due.
  async function run(ms = 0) {
    const end = clock + ms;
    for (let guard = 0; guard < 5000; guard++) {
      for (let i = 0; i < 3; i++) { loadAll(); stub.render(); await settle(); }
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
  const click = async (id, props) => { byId(id).dispatch('click', props); await settle(); };
  // A key pressed with focus on `target`, the map by default.
  const key = async (name, props = {}, target = map) => { const event = target.dispatch('keydown', { key: name, ...props }); await settle(); return event; };
  const mapRequests = () => requests.filter(url => /\/map\?/.test(url)).map(url => new URL(url));
  const wmsRequests = () => requests.filter(url => /\/wms\?/.test(url)).map(url => new URL(url));
  // The frames staged as tiles, by their template.
  const tileFrames = () => stub.added.filter(([, source]) => source.tiles).map(([, source]) => new URL(source.tiles[0].replace(/[{}]/g, '')));
  const shownTime = () => { const [id] = stub.visible(); return id && sourceTime(id); };
  const playing = () => byId('map-play').getAttribute('aria-label') === 'Pause';
  return { root, map: stub, byId, change, click, key, wait, run, requests, mapRequests, wmsRequests, tileFrames, shownTime, sourceTime, held, playing,
    writes, writeTimes, location, document, signals, inFlight: () => inFlight, status: byId('map-status'), legend: byId('map-legend'),
    fireDocument: type => (documentListeners[type] || []).forEach(listener => listener({ type })),
    intersect: visible => intersection?.([{ isIntersecting: visible }]),
    clock: () => clock };
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
// and the old is removed only after that paint, unless it is `kept` as a
// buffered frame.
function assertSwap(p, old, { kept = false } = {}) {
  const staged = p.map.rasters().map(l => l.id).find(id => id !== old && !p.map.loadedSources.has(id));
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
  const swap = p.map.log.slice(mark).filter(entry => entry[0] === 'paint').map(entry => entry.join(' '));
  assert.deepEqual(swap, [`paint ${staged} raster-opacity 0.85`, `paint ${old} raster-opacity 0`]);
  assert.deepEqual(p.map.visible(), [staged]);
  assert.equal(JSON.stringify(p.map.getLayer(old).paint['raster-opacity-transition']), '{"duration":0,"delay":0}');
  p.map.render();
  if (kept) assert.ok(p.map.getLayer(old) && p.map.getSource(old), 'a frame still ahead was dropped');
  else assert.ok(!p.map.getLayer(old) && !p.map.getSource(old), 'the old frame outlived the swap');
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

// #1220: the collection page's map. The fixture's axis is three hourly
// frames, 2024-01-01 00:00 to 02:00 UTC: observations to a viewer on
// 2026-10-19, so the map opens on the last and loops all three.

const HOURLY = count => r => { r.axis = { start: '2024-01-01T00:00:00Z', step: 3600, count }; };
const hour = h => new Date(Date.parse('2024-01-01T00:00:00Z') + h * 3600e3).toISOString().replace('.000Z', 'Z');
// Image requests answered when the test says so, by the time they ask for.
function deferred() {
  const pending = new Map();
  return {
    handler: url => new Promise(resolve => pending.set(url.searchParams.get('datetime') || url.searchParams.get('TIME'), resolve)),
    pending,
    release(time, response = image(time)) { pending.get(time)(response); pending.delete(time); },
  };
}

test('map tiles draw by default, at an explicit time named once the tiles are in', async () => {
  const p = await page();
  assert.equal(p.map.options.cooperativeGestures, true);
  const [staged] = p.map.rasters();
  const source = p.map.getSource(staged.id);
  assert.equal(source.type, 'raster');
  assert.equal(source.tileSize, 256);
  assert.equal(source.tiles[0], 'https://x/collections/nwp/map/tiles/WebMercatorQuad/{z}/{y}/{x}?datetime=2024-01-01T02%3A00%3A00Z');
  assert.match(p.status.textContent, /^Loading map tiles…$/);
  assert.equal(p.byId('map-time-drawn').textContent, '–');
  showFirst(p);
  assert.equal(p.status.textContent, 'Map tiles · Showing 2024-01-01 02:00 UTC · Temperature (K)');
  // The time bar names the time drawn, UTC, and how long ago it was.
  assert.equal(p.byId('map-time-drawn').textContent, 'Mon 2024-01-01 02:00 UTC');
  assert.equal(p.byId('map-time-relative').textContent, '1022 d ago');
  assert.equal(p.byId('map-timebar').hidden, false);
  // The form's own date and time is for a page without the script.
  assert.equal(p.byId('map-time').closest('.map-time-field').hidden, true);
});

test('map tiles: a step keeps the drawn source until the next has loaded, then swaps in one paint', async () => {
  const p = await page();
  const first = showFirst(p);
  await p.click('map-prev');
  assert.match(p.status.textContent, /^Loading map tiles… Still showing 2024-01-01 02:00 UTC · Temperature \(K\)\.$/);
  // 02:00 is the loop's next frame from 01:00: it stays, hidden.
  const next = assertSwap(p, first, { kept: true });
  const query = template => new URL(template.replace(/[{}]/g, '')).search;
  assert.equal(query(p.map.getSource(next).tiles[0]), '?datetime=2024-01-01T01%3A00%3A00Z');
  assert.match(p.status.textContent, /^Map tiles · Showing 2024-01-01 01:00 UTC · Temperature \(K\)$/);
  // Each choice applies at once and swaps the same way.
  await p.change('map-parameter', 't');
  const third = assertSwap(p, next);
  assert.equal(query(p.map.getSource(third).tiles[0]), '?parameter-name=t&datetime=2024-01-01T01%3A00%3A00Z');
});

test('Maps images: a step keeps the drawn image until the next has loaded, then swaps in one paint', async () => {
  const p = await page({ mode: 'maps' });
  const first = showFirst(p);
  assert.equal(p.map.getSource(first).type, 'canvas');
  assert.match(p.status.textContent, /^Showing 2024-01-01 02:00 UTC/);
  await p.click('map-prev');
  assert.equal(p.mapRequests().at(-1).searchParams.get('datetime'), '2024-01-01T01:00:00Z');
  assertSwap(p, first, { kept: true });
  // The label is the instant the server drew.
  assert.equal(p.byId('map-time-drawn').textContent, 'Mon 2024-01-01 01:00 UTC');
});

test('a failed request keeps the frame and says why; a 503 retries after Retry-After, at most three times', async () => {
  let answer = url => image(url.searchParams.get('datetime'));
  const p = await page({ mode: 'maps', maps: url => answer(url) });
  const first = showFirst(p);
  // A pan whose image fails: the frame stays, marked stale.
  answer = () => respond(500, { json: { description: 'Internal server error' } });
  p.map.view = { west: 21, south: 56, east: 31, north: 66 };
  p.map.fire('moveend');
  await p.wait(300);
  p.map.render();
  assert.deepEqual(p.map.visible(), [first]);
  assert.equal(p.status.textContent, 'No data at 2024-01-01 02:00 UTC. Choose another time. Still showing 2024-01-01 02:00 UTC · Temperature (K).');
  assert.ok(p.status.classList.contains('stale'));
  // No data at the time chosen: said so, the frame stays, the track marks it.
  answer = () => respond(404, { json: { description: 'No data' } });
  await p.click('map-first');
  p.map.render();
  assert.deepEqual(p.map.visible(), [first]);
  assert.equal(p.status.textContent, 'No data at 2024-01-01 00:00 UTC. Choose another time. Still showing 2024-01-01 02:00 UTC · Temperature (K).');
  assert.ok(p.status.classList.contains('stale'));
  // The time bar stays: another time may have data.
  assert.equal(p.byId('map-timebar').hidden, false);
  const errored = () => p.byId('map-track-buffer').children.filter(node => node.className === 'map-errored nodata').map(node => [node.style.left, node.title]);
  assert.deepEqual(errored(), [['100%', 'No data at Mon 2024-01-01 02:00 UTC'], ['0%', 'No data at Mon 2024-01-01 00:00 UTC']]);
  // Busy: wait as told, then back off, three retries in all.
  answer = () => respond(503, { headers: { 'Retry-After': '2' }, json: { description: 'Server busy, try again later' } });
  const before = p.mapRequests().length;
  await p.click('map-next');
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
  // A request that succeeds swaps as usual, and clears the mark. The
  // old image is of the view before the pan: it goes.
  answer = url => image(url.searchParams.get('datetime'));
  await p.click('map-prev');
  assertSwap(p, first);
  assert.ok(!p.status.classList.contains('stale'));
  assert.deepEqual(errored(), [['100%', 'No data at Mon 2024-01-01 02:00 UTC']]);
});

test('map tiles that all fail keep the frame; none in view is no data', async () => {
  const p = await page();
  const first = showFirst(p);
  await p.click('map-first');
  const staged = p.map.rasters().find(layer => layer.id !== first);
  assert.ok(staged, 'the step staged no frame');
  // A 404 tile is no event in MapLibre: nothing loaded, nothing failed.
  p.map.loadedSources.add(staged.id);
  p.map.render();
  assert.deepEqual(p.map.visible(), [first]);
  assert.ok(!p.map.getSource(staged.id));
  assert.match(p.status.textContent, /^No data at 2024-01-01 00:00 UTC\. Choose another time\. Still showing/);
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
  p.byId('map-jump').value = '2024-01-01T00:00';
  p.map.view = { west: 21, south: 56, east: 31, north: 66 };
  p.map.fire('moveend');
  await p.wait(300);
  const last = p.mapRequests().at(-1);
  assert.equal(last.searchParams.get('bbox'), '21,56,31,66');
  assert.equal(last.searchParams.get('parameter-name'), null);
  assert.equal(last.searchParams.get('datetime'), '2024-01-01T02:00:00Z');
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
  assert.match(tiles.map.getSource(tiles.map.rasters()[0].id).tiles[0], /\?datetime=2024-01-01T02%3A00%3A00Z&width=512&height=512$/);
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
  const hash = '#map=6/61.5/24.25&parameter-name=r&style=warm&datetime=2024-01-01T01:30:00Z&elevation=850&mode=maps';
  const p = await page({ hash });
  assert.deepEqual(p.map.log.filter(e => ['jumpTo', 'fitBounds'].includes(e[0])).map(e => e.join(' ')), ['jumpTo 6/61.5/24.25']);
  assert.equal(p.byId('map-parameter').value, 'r');
  assert.equal(p.byId('map-style').selectedIndex, 1);
  assert.equal(p.byId('map-playhead').getAttribute('aria-valuetext'), 'Mon 2024-01-01 01:30 UTC');
  assert.equal(p.byId('map-level').value, '850');
  const url = p.mapRequests().at(-1);
  assert.equal(url.pathname, '/collections/nwp/styles/warm/map');
  assert.equal(url.searchParams.get('parameter-name'), 'r');
  assert.equal(url.searchParams.get('datetime'), '2024-01-01T01:30:00Z');
  assert.equal(url.searchParams.get('elevation'), '850');
  // Nothing is written until the viewer moves or chooses.
  await p.wait(5000);
  assert.deepEqual(p.writes, []);
  p.map.center = { lng: 25.5, lat: 62.123456 }; p.map.zoom = 7.25; p.map.view = { west: 22, south: 60, east: 29, north: 64 };
  p.map.fire('moveend');
  await p.change('map-level', '1000');
  await p.wait(1000);
  // One write for both, through replaceState.
  assert.deepEqual(p.writes, ['#map=7.25/62.1235/25.5&parameter-name=r&style=warm&datetime=2024-01-01T01:30:00Z&elevation=1000&mode=maps']);
  // Its keys are Maps parameter names, besides the view, style, mode and loop.
  for (const key of new URLSearchParams(p.writes[0].slice(1)).keys()) {
    assert.ok(MAP_PARAMETERS.includes(key) || ['map', 'style', 'mode', 'window'].includes(key), key);
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

test('the form fields are Maps parameters; the draw switch, the style and the time bar are not fields', async () => {
  const p = await page();
  const form = p.byId('map-form');
  assert.equal(form.method, 'get');
  assert.equal(form.action, 'https://x/collections/nwp/map');
  const fields = [...form.walk()].filter(e => ['input', 'select', 'button'].includes(e.tag) && e.name);
  assert.deepEqual(fields.map(e => e.name).sort(), ['datetime', 'elevation', 'parameter-name']);
  for (const field of fields) assert.ok(MAP_PARAMETERS.includes(field.name), field.name);
  // The time is a date and time bounded by the axis, not an option per step.
  const time = p.byId('map-time');
  assert.deepEqual([time.type, time.attributes.min, time.attributes.max, time.attributes.step],
    ['datetime-local', '2024-01-01T00:00:00', '2024-01-01T02:00:00', '3600']);
  // The script submits nothing: Enter in a field applies, the page stays.
  assert.equal(form.dispatch('submit').defaultPrevented, true);
  assert.ok(!p.byId('map-style').name);
  const switches = [...p.root.walk()].filter(e => e.name === 'map-mode');
  assert.deepEqual(switches.map(radio => radio.value), ['tiles', 'maps', 'wms']);
  assert.ok(switches.every(radio => !radio.closest('form')));
  assert.ok(!p.byId('map-timebar').closest('form'));
});

test('without a time axis, a date and time typed in applies on change, not before', async () => {
  const p = await page({
    mode: 'maps',
    mapData: r => { delete r.axis; r.parameter_names.forEach(parameter => delete parameter.axis); },
  });
  assert.equal(p.byId('map-timebar').hidden, true);
  assert.equal(p.byId('map-time').closest('.map-time-field').hidden, false);
  showFirst(p);
  assert.equal(p.mapRequests().at(-1).searchParams.get('datetime'), null);
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

test('the compact axis: a regular grid or an irregular list, laid out by time', async () => {
  const p = await page();
  const slider = p.byId('map-playhead');
  assert.deepEqual([slider.min, slider.max, slider.value], ['0', '7200', '7200']);
  assert.equal(slider.getAttribute('aria-valuetext'), 'Mon 2024-01-01 02:00 UTC');
  const notches = page => page.byId('map-track-marks').children.filter(node => node.className === 'map-notch').map(node => parseFloat(node.style.left));
  assert.deepEqual(notches(p), [0, 50, 100]);
  // An irregular list, out of order, with a repeat and an invalid entry:
  // each instant once, in order, and the gap shows on the track.
  const q = await page({ mapData: r => { r.axis = { times: ['2024-01-01T03:00:00Z', '2024-01-01T00:00:00Z', 'nonsense', '2024-01-01T00:20:00Z', '2024-01-01T00:00:00Z'] }; } });
  assert.equal(q.byId('map-playhead').max, '10800');
  assert.deepEqual(notches(q).map(n => Number(n.toFixed(3))), [0, 11.111, 100]);
  const times = [];
  for (let i = 0; i < 3; i++) { await q.key(','); times.push(q.tileFrames().at(-1).searchParams.get('datetime')); }
  assert.deepEqual(times, ['2024-01-01T00:20:00Z', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z']);
});

test('a 288-step and a 1500-step axis stay usable', async () => {
  const day = await page({ mapData: r => { r.axis = { start: '2024-01-01T00:00:00Z', step: 300, count: 288 }; } });
  const marks = page => page.byId('map-track-marks').children;
  // Five minutes apart on 600 px is too dense for notches: a strip, with
  // hour ticks and the day labelled.
  assert.equal(marks(day).filter(node => node.className === 'map-notch').length, 0);
  assert.equal(marks(day).filter(node => node.className === 'map-strip').length, 1);
  assert.deepEqual(marks(day).filter(node => /map-tick/.test(node.className)).map(node => node.textContent),
    ['Mon 01-01', '03:00', '06:00', '09:00', '12:00', '15:00', '18:00', '21:00']);
  assert.equal(day.byId('map-window-text').textContent, 'Loop: 24 frames · 1.9 h');
  assert.equal(day.byId('map-speed').value, '4');
  const long = await page({ mapData: r => { r.axis = { start: '2024-01-01T00:00:00Z', step: 300, count: 1500 }; } });
  const ticks = marks(long).filter(node => /map-tick/.test(node.className));
  assert.ok(ticks.length <= 600 / 72 && ticks.every(node => node.className === 'map-tick day'), ticks.map(n => n.textContent).join());
  const last = () => long.tileFrames().at(-1).searchParams.get('datetime');
  assert.equal(last(), '2024-01-06T04:55:00Z');
  await long.key(',', { shiftKey: true });
  assert.equal(last(), '2024-01-06T04:05:00Z');
  await long.key('PageUp');
  assert.equal(last(), '2024-01-05T04:05:00Z');
  await long.key('Home');
  assert.equal(last(), '2024-01-01T00:00:00Z');
  await long.key('End');
  assert.equal(last(), '2024-01-06T04:55:00Z');
});

test('map tiles: playback holds until every tile of the next frame is in, and announces nothing per frame', async () => {
  const p = await page();
  await p.run();
  p.held.add('2024-01-01T00:00:00Z');
  await p.click('map-play');
  assert.ok(p.playing());
  assert.equal(p.status.textContent, 'Playing 3 frames · 2 h, 2 frames a second.');
  await p.run(3000);
  assert.equal(p.shownTime(), '2024-01-01T02:00:00Z');
  assert.ok(p.byId('map-play').classList.contains('buffering'));
  p.held.clear();
  await p.run();
  assert.equal(p.shownTime(), '2024-01-01T00:00:00Z');
  assert.ok(!p.byId('map-play').classList.contains('buffering'));
  assert.equal(p.byId('map-time-drawn').textContent, 'Mon 2024-01-01 00:00 UTC');
  await p.run(500);
  assert.equal(p.shownTime(), '2024-01-01T01:00:00Z');
  // Played frames change the label, not the live status or the slider.
  assert.equal(p.status.textContent, 'Playing 3 frames · 2 h, 2 frames a second.');
  assert.equal(p.byId('map-playhead').getAttribute('aria-valuetext'), 'Mon 2024-01-01 02:00 UTC');
  // The last frame holds 1.2 s, then the loop starts over.
  await p.run(500);
  assert.equal(p.shownTime(), '2024-01-01T02:00:00Z');
  await p.run(1100);
  assert.equal(p.shownTime(), '2024-01-01T02:00:00Z');
  await p.run(100);
  assert.equal(p.shownTime(), '2024-01-01T00:00:00Z');
  // A pause is announced, and the slider moves to it.
  await p.click('map-play');
  assert.ok(!p.playing());
  assert.equal(p.status.textContent, 'Paused at 2024-01-01 00:00 UTC · Temperature (K).');
  assert.equal(p.byId('map-playhead').getAttribute('aria-valuetext'), 'Mon 2024-01-01 00:00 UTC');
});

test('Maps images: playback holds for the next image and skips a frame without data, said once', async () => {
  const images = deferred();
  const p = await page({ mode: 'maps', maps: images.handler });
  images.release(hour(2));
  await p.run();
  assert.equal(p.shownTime(), hour(2));
  // Played from its last frame, the loop starts over at its first, asked
  // for alone; the drawn frame holds until it is in.
  await p.click('map-play');
  assert.deepEqual([...images.pending.keys()], [hour(0)]);
  await p.run(5000);
  assert.equal(p.shownTime(), hour(2), 'a frame not in yet was shown');
  images.release(hour(0));
  await p.run();
  assert.equal(p.shownTime(), hour(0));
  // Buffering ahead: 01:00, while 02:00 stays buffered.
  assert.deepEqual([...images.pending.keys()], [hour(1)]);
  // 01:00 answers 404 while playback waits for it: marked and skipped.
  await p.run(500);
  images.release(hour(1), respond(404, { json: { description: 'No data' } }));
  await p.run();
  assert.equal(p.status.textContent, 'No data at 2024-01-01 01:00 UTC, skipped.');
  assert.equal(p.shownTime(), hour(2));
  assert.equal(p.byId('map-track-buffer').children.filter(node => node.className === 'map-errored nodata').length, 1);
  // The next loop skips it without asking again.
  await p.run(5000);
  assert.equal(p.mapRequests().filter(url => url.searchParams.get('datetime') === hour(1)).length, 1);
  assert.ok(p.playing());
});

test('every frame request names its time from the axis, and a second loop asks for nothing new', async () => {
  for (const mode of ['maps', 'wms']) {
    const p = await page({ mode });
    await p.run();
    await p.click('map-play');
    await p.run(4000);
    const requests = mode === 'maps' ? p.mapRequests : p.wmsRequests;
    const firstLoop = requests().length;
    assert.equal(firstLoop, 3, mode);
    await p.run(8000);
    assert.equal(requests().length, firstLoop, `${mode}: the second loop asked again`);
    for (const url of requests()) assert.match(url.searchParams.get(mode === 'maps' ? 'datetime' : 'TIME'), /^2024-01-01T0[0-2]:00:00Z$/);
  }
  // Map tiles: each frame its own source, staged once.
  const tiles = await page();
  await tiles.run();
  await tiles.click('map-play');
  await tiles.run(4000);
  const staged = tiles.tileFrames().length;
  await tiles.run(8000);
  assert.equal(tiles.tileFrames().length, staged);
  for (const url of tiles.tileFrames()) assert.match(url.searchParams.get('datetime'), /^2024-01-01T0[0-2]:00:00Z$/);
  // A loop longer than the buffer asks again for the same, cacheable URLs.
  const long = await page({ mode: 'maps', mapData: HOURLY(8) });
  await long.run();
  await long.click('map-play');
  await long.run(6000);
  const seen = new Set(long.mapRequests().map(url => url.href));
  assert.equal(seen.size, 8);
  await long.run(12000);
  assert.ok(long.mapRequests().length > seen.size);
  for (const url of long.mapRequests()) assert.ok(seen.has(url.href), url.href);
});

test('map tiles buffer one frame at a time, at most K, and drop them all on a layer change', async () => {
  const p = await page({ mapData: HOURLY(30) });
  await p.run();
  // Only looked at: one frame, nothing buffered.
  await p.run(5000);
  assert.equal(p.map.rasters().length, 1);
  await p.click('map-play');
  const loading = () => p.map.rasters().filter(layer => !p.map.loadedSources.has(layer.id));
  // 800 × 480 px holds 15 tiles: K = clamp(⌊160 / 15⌋, 2, 6) = 6.
  for (let i = 0; i < 8; i++) {
    assert.ok(loading().length <= 1, `${loading().length} frames loading at once`);
    if (!loading().length) break;
    p.map.tilesLoaded(loading()[0].id); p.map.render(); await settle();
  }
  assert.equal(p.map.rasters().length, 1 + 6);
  assert.deepEqual(p.map.visible().length, 1);
  // Another parameter: every buffered frame goes; the drawn one stays.
  const drawn = p.map.visible()[0];
  await p.change('map-parameter', 't');
  assert.deepEqual(p.map.rasters().map(layer => layer.id).filter(id => id !== drawn).length, 1);
  assert.equal(p.map.visible()[0], drawn);
  // A larger map buffers fewer: 2560 × 1440 px holds 77 tiles, K = 2.
  const big = await page({ mapData: HOURLY(30), size: [2560, 1440] });
  await big.run();
  await big.click('map-play');
  const bigLoading = () => big.map.rasters().filter(layer => !big.map.loadedSources.has(layer.id));
  for (let i = 0; i < 4 && bigLoading().length; i++) { big.map.tilesLoaded(bigLoading()[0].id); big.map.render(); await settle(); }
  assert.equal(big.map.rasters().length, 1 + 2);
});

test('Maps images: at most two fetches at a time and three frames ahead, aborted on a layer change', async () => {
  const images = deferred();
  const p = await page({ mode: 'maps', mapData: HOURLY(30), maps: images.handler });
  images.release(hour(29));
  await p.run();
  await p.click('map-play');
  assert.deepEqual([...images.pending.keys()], [hour(6)]);
  images.release(hour(6));
  await p.run();
  assert.equal(p.shownTime(), hour(6));
  assert.equal(p.inFlight(), 2);
  assert.deepEqual([...images.pending.keys()], [hour(7), hour(8)]);
  images.release(hour(7));
  await settle(); p.map.render(); await settle();
  assert.equal(p.inFlight(), 2);
  assert.deepEqual([...images.pending.keys()], [hour(8), hour(9)]);
  // Another level: both are aborted, and the drawn image stays.
  const drawn = p.map.visible()[0];
  const open = p.signals.filter(entry => [hour(8), hour(9)].some(time => entry.url.includes(encodeURIComponent(time))));
  await p.change('map-level', '850');
  assert.equal(open.length, 2);
  assert.ok(open.every(entry => entry.signal.aborted));
  assert.equal(p.map.visible()[0], drawn);
  // Released, the frames ahead stop at three.
  const q = await page({ mode: 'maps', mapData: HOURLY(30) });
  await q.run();
  await q.click('map-play');
  for (let i = 0; i < 5; i++) { await settle(); q.map.rasters().forEach(layer => q.map.tilesLoaded(layer.id)); q.map.render(); await settle(); }
  assert.equal(q.map.rasters().length, 1 + 3);
  // The opening frame, the loop's first, and three ahead.
  assert.equal(q.mapRequests().length, 1 + 1 + 3);
});

test('a busy server stops buffering for a while, unmarked, backing off 2, 4, 8 s', async () => {
  let busy = true;
  const refused = [];
  let clock;
  const p = await page({ mode: 'maps', mapData: HOURLY(30), maps: url => {
    const time = url.searchParams.get('datetime');
    if (!busy || [hour(28), hour(29)].includes(time)) return image(time);
    refused.push(clock());
    return respond(503, { json: {} });
  } });
  clock = p.clock;
  await p.run();
  // Stepped back to 28: the frames ahead, 6 and 7, are buffered; refused.
  await p.click('map-prev');
  await p.run(20000);
  const rounds = [...new Set(refused)];
  assert.deepEqual(rounds.slice(1).map((time, i) => time - rounds[i]), [2000, 4000, 8000]);
  assert.equal(p.byId('map-track-buffer').children.filter(node => /map-errored/.test(node.className)).length, 0);
  // Answered again: buffering goes on.
  busy = false;
  await p.run(20000);
  assert.equal(p.byId('map-track-buffer').children.filter(node => node.className === 'map-buffered').length, 1 + 3);
});

test('Save-Data buffers one frame ahead; a metered connection says what playing costs first', async () => {
  const p = await page({ connection: { saveData: true }, mapData: HOURLY(30) });
  await p.run();
  assert.equal(p.byId('map-window-text').textContent, 'Loop: 24 frames · 23 h · about 23 frames to load on this metered connection');
  await p.click('map-play');
  assert.equal(p.map.rasters().length, 2);
  assert.equal(p.byId('map-window-text').textContent, 'Loop: 24 frames · 23 h');
  const slow = await page({ connection: { effectiveType: '3g' } });
  await slow.run();
  assert.match(slow.byId('map-window-text').textContent, /about 2 frames to load/);
  const fast = await page({ connection: { effectiveType: '4g' } });
  assert.equal(fast.byId('map-window-text').textContent, 'Loop: 3 frames · 2 h');
});

test('the loop window: the last 24 observations, a forecast\'s 24 from now, all of fewer; kept in the fragment', async () => {
  const obs = await page({ mapData: HOURLY(30) });
  const band = page => [page.byId('map-window').style.left, page.byId('map-window').style.width].map(parseFloat).map(n => Number(n.toFixed(3)));
  assert.equal(obs.byId('map-window-text').textContent, 'Loop: 24 frames · 23 h');
  assert.deepEqual(band(obs), [Number((6 / 29 * 100).toFixed(3)), Number((23 / 29 * 100).toFixed(3))]);
  assert.equal(obs.tileFrames().at(-1).searchParams.get('datetime'), hour(29));
  // A forecast opens at the step nearest now and loops 24 steps from it.
  const forecast = await page({ mapData: r => { r.axis = { start: '2026-10-18T12:00:00Z', step: 3600, count: 40 }; } });
  assert.equal(forecast.tileFrames().at(-1).searchParams.get('datetime'), '2026-10-19T00:00:00Z');
  assert.deepEqual(band(forecast), [Number((12 / 39 * 100).toFixed(3)), Number((23 / 39 * 100).toFixed(3))]);
  assert.match(forecast.byId('map-time-drawn').textContent, /^–$/);
  // Near its end, the 24 that end there.
  const late = await page({ mapData: r => { r.axis = { start: '2026-10-18T00:00:00Z', step: 3600, count: 30 }; } });
  assert.deepEqual(band(late), [Number((6 / 29 * 100).toFixed(3)), Number((23 / 29 * 100).toFixed(3))]);
  const few = await page();
  assert.equal(few.byId('map-window-text').textContent, 'Loop: 3 frames · 2 h');
  // `[` and `]` set its ends at the playhead; the fragment keeps them.
  for (let i = 0; i < 5; i++) await obs.key(',');
  await obs.key('[');
  assert.equal(obs.byId('map-window-text').textContent, 'Loop: 6 frames · 5 h');
  assert.equal(obs.status.textContent, 'Loop: 6 frames · 5 h, Tue 2024-01-02 00:00 UTC to Tue 2024-01-02 05:00 UTC.');
  await obs.wait(1000);
  assert.match(obs.writes.at(-1), /&window=2024-01-02T00:00:00Z\/2024-01-02T05:00:00Z$/);
  const again = await page({ hash: obs.writes.at(-1), mapData: HOURLY(30) });
  assert.equal(again.byId('map-window-text').textContent, 'Loop: 6 frames · 5 h');
  assert.equal(again.tileFrames().at(-1).searchParams.get('datetime'), hour(24));
  // `]` before the start moves the start back, keeping 24 frames.
  await again.key('Home');
  await again.key(']');
  assert.equal(again.byId('map-window-text').textContent, 'Loop: 1 frame · 0 min');
  await again.click('map-window-reset');
  assert.equal(again.byId('map-window-text').textContent, 'Loop: 24 frames · 23 h');
});

test('shortcuts work only with focus in the map or its time bar', async () => {
  const p = await page({ mapData: HOURLY(30) });
  await p.run();
  // The frame asked for last, or shown again.
  const at = () => p.byId('map-playhead').getAttribute('aria-valuetext');
  const last = () => { const text = at().slice(4, 20); return `${text.replace(' ', 'T')}:00Z`; };
  // Outside the map region: nothing.
  await p.key('.', {}, p.byId('map-legend'));
  await p.key(',', {}, p.byId('map-help'));
  assert.equal(p.tileFrames().length, 1);
  // On the map: `,` steps back, with Shift ten frames.
  await p.key(',');
  assert.equal(last(), hour(28));
  await p.key(',', { shiftKey: true });
  assert.equal(last(), hour(18));
  // On the map, arrows are MapLibre's; on the time slider they step.
  assert.equal((await p.key('ArrowLeft')).defaultPrevented, false);
  assert.equal(last(), hour(18));
  assert.equal((await p.key('ArrowRight', {}, p.byId('map-playhead'))).defaultPrevented, true);
  assert.equal(last(), hour(19));
  // Typing in the jump field types.
  assert.equal((await p.key('k', {}, p.byId('map-jump'))).defaultPrevented, false);
  assert.ok(!p.playing());
  // Space or K plays and pauses; Space on a button is that button's.
  await p.key('k', {}, p.byId('map-playhead'));
  assert.ok(p.playing());
  await p.key(' ');
  assert.ok(!p.playing());
  assert.equal((await p.key(' ', {}, p.byId('map-next'))).defaultPrevented, false);
  assert.ok(!p.playing());
  // `<` and `>` change the speed, which is said.
  await p.key('>');
  assert.equal(p.byId('map-speed').value, '4');
  assert.equal(p.status.textContent, '4 frames a second.');
  await p.key('<'); await p.key('<'); await p.key('<');
  assert.equal(p.byId('map-speed').value, '1');
  // N: the frame nearest now, here the last.
  await p.key('n');
  assert.equal(last(), hour(29));
  // `?` lists the keys; Escape closes the list.
  await p.key('?');
  assert.equal(p.byId('map-keys-help').hidden, false);
  assert.equal(p.byId('map-keys').getAttribute('aria-expanded'), 'true');
  await p.key('Escape');
  assert.equal(p.byId('map-keys-help').hidden, true);
  // Jump to a date and time, read as UTC.
  await p.change('map-jump', '2024-01-01T03:20');
  assert.equal(last(), hour(3));
});

test('another parameter rebinds the time bar to its own axis at the nearest instant', async () => {
  const p = await page();
  await p.run();
  assert.equal(p.byId('map-playhead').max, '7200');
  await p.change('map-parameter', 'r');
  // r is drawn at 00:30 and 01:30 only: from 02:00, 01:30.
  assert.equal(p.byId('map-playhead').max, '3600');
  assert.equal(p.byId('map-playhead').getAttribute('aria-valuetext'), 'Mon 2024-01-01 01:30 UTC');
  assert.equal(p.tileFrames().at(-1).search, '?parameter-name=r&datetime=2024-01-01T01%3A30%3A00Z');
  assert.equal(p.byId('map-window-text').textContent, 'Loop: 2 frames · 1 h');
  await p.key(',');
  assert.equal(p.tileFrames().at(-1).searchParams.get('datetime'), '2024-01-01T00:30:00Z');
  // Back on the collection's axis: the nearest of 00:00 and 01:00.
  await p.change('map-parameter', '');
  assert.equal(p.byId('map-playhead').max, '7200');
  assert.equal(p.tileFrames().at(-1).searchParams.get('datetime'), '2024-01-01T00:00:00Z');
});

test('dragging the playhead asks for the frame under it after 150 ms at rest or on release, never per pixel', async () => {
  const p = await page({ mapData: HOURLY(30) });
  await p.run();
  const before = p.tileFrames().length;
  const slider = p.byId('map-playhead');
  for (let i = 0; i < 20; i++) {
    slider.value = String((28 - i) * 3600 - 600);
    slider.dispatch('input');
    await p.wait(100);
  }
  assert.equal(p.tileFrames().length, before, 'a frame was asked for mid-drag');
  // The pointer's time is shown at once, the drawn one stays labelled.
  assert.equal(slider.getAttribute('aria-valuetext'), 'Mon 2024-01-01 09:00 UTC');
  assert.equal(p.byId('map-time-seek').textContent, '→ Mon 2024-01-01 09:00 UTC');
  assert.equal(p.byId('map-time-drawn').textContent, 'Tue 2024-01-02 05:00 UTC');
  await p.wait(150);
  assert.equal(p.tileFrames().length, before + 1);
  assert.equal(p.tileFrames().at(-1).searchParams.get('datetime'), hour(9));
  // Released elsewhere: at once.
  slider.value = String(3 * 3600);
  slider.dispatch('change');
  await settle();
  assert.equal(p.tileFrames().length, before + 2);
  assert.equal(p.tileFrames().at(-1).searchParams.get('datetime'), hour(3));
});

test('WMS GetMap: EPSG:3857 metres, the parameter layer, an explicit TIME, and errors read as XML', async () => {
  let answer = () => image();
  const p = await page({ mode: 'wms', wms: url => answer(url) });
  await p.run();
  const url = p.wmsRequests().at(-1);
  assert.equal(`${url.origin}${url.pathname}`, 'https://x/wms');
  const params = Object.fromEntries(url.searchParams);
  const bbox = params.BBOX.split(',').map(Number);
  delete params.BBOX;
  assert.deepEqual(params, { SERVICE: 'WMS', VERSION: '1.3.0', REQUEST: 'GetMap', LAYERS: 'nwp', STYLES: '', CRS: 'EPSG:3857',
    WIDTH: '800', HEIGHT: '480', FORMAT: 'image/png', TRANSPARENT: 'TRUE', EXCEPTIONS: 'XML', TIME: '2024-01-01T02:00:00Z' });
  // minx,miny,maxx,maxy in metres, whatever a CRS's axis order: the view
  // is 20–30° E, 55–65° N.
  const R = 6378137, x = lon => R * lon * Math.PI / 180, y = lat => R * Math.log(Math.tan(Math.PI / 4 + lat * Math.PI / 360));
  [x(20), y(55), x(30), y(65)].forEach((value, i) => assert.ok(Math.abs(bbox[i] - value) < 0.01, `${bbox[i]} ≠ ${value}`));
  assert.equal(p.shownTime(), '2024-01-01T02:00:00Z');
  assert.equal(p.byId('map-image-link').href, url.href);
  // WMS names no Content-Datetime: the label is the time asked for.
  assert.equal(p.byId('map-time-drawn').textContent, 'Mon 2024-01-01 02:00 UTC');
  // The parameter's own layer, the style by name and the level.
  await p.change('map-parameter', 't');
  await p.change('map-style', 'https://x/collections/nwp/styles/warm/map');
  await p.change('map-level', '850');
  await p.run();
  const chosen = p.wmsRequests().at(-1).searchParams;
  assert.deepEqual(['LAYERS', 'STYLES', 'ELEVATION', 'TIME'].map(key => chosen.get(key)), ['nwp/t', 'warm', '850', '2024-01-01T02:00:00Z']);
  // A request error comes back as a ServiceException: the frame stays.
  const drawn = p.map.visible()[0];
  answer = () => respond(400, { headers: { 'Content-Type': 'text/xml' },
    text: '<?xml version="1.0" encoding="UTF-8"?><ServiceExceptionReport version="1.3.0" xmlns="http://www.opengis.net/ogc"><ServiceException code="StyleNotDefined">Style &apos;warm&apos; is not defined for layer &apos;nwp/t&apos;</ServiceException></ServiceExceptionReport>' });
  await p.click('map-prev');
  await p.run();
  assert.deepEqual(p.map.visible(), [drawn]);
  assert.equal(p.status.textContent, "The server rejected this request: Style 'warm' is not defined for layer 'nwp/t'. Still showing 2024-01-01 02:00 UTC · Temperature (K) · Warm · 850 hPa.");
  // So is the error image a render failure answers with: never drawn.
  answer = () => respond(200, { headers: { 'Content-Type': 'image/png', 'x-cache': 'ERROR' } });
  await p.click('map-prev');
  await p.run();
  assert.deepEqual(p.map.visible(), [drawn]);
  assert.match(p.status.textContent, /^The server could not draw this map\. Still showing/);
  assert.ok(p.wmsRequests().every(request => request.searchParams.get('EXCEPTIONS') === 'XML'));
});

test('no autoplay; a hidden tab or a map scrolled away pauses playback and buffering', async () => {
  const p = await page({ mode: 'maps', mapData: HOURLY(30) });
  await p.run(10000);
  assert.ok(!p.playing());
  assert.equal(p.mapRequests().length, 1);
  await p.click('map-play');
  assert.ok(p.playing());
  p.document.visibilityState = 'hidden';
  p.fireDocument('visibilitychange');
  assert.ok(!p.playing());
  assert.match(p.status.textContent, /^Paused at 2024-01-02 05:00 UTC/);
  const requests = p.mapRequests().length;
  await p.run(5000);
  assert.equal(p.mapRequests().length, requests);
  // Back: buffering resumes, playback does not.
  p.document.visibilityState = 'visible';
  p.fireDocument('visibilitychange');
  await p.run(5000);
  assert.ok(!p.playing());
  await p.click('map-play');
  p.intersect(false);
  assert.ok(!p.playing());
  const away = p.mapRequests().length;
  await p.run(5000);
  assert.equal(p.mapRequests().length, away);
});
