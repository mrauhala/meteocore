// Run with: node --test crates/api-common/tests/map.test.cjs
// Exercise the shipped workbench map script's quick look with a stub DOM and
// MapLibre. No WebGL mock claims to validate drawing; these tests cover what
// the script prints.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const script = fs.readFileSync(`${__dirname}/../src/workbench/map.js`, 'utf8');

class Element {
  constructor(tag) {
    this.tag = tag; this.children = []; this.dataset = {}; this.attributes = {};
    this.className = ''; this.title = ''; this.textContent = ''; this.hidden = false;
  }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; }
  setAttribute(name, value) { this.attributes[name] = value; }
  after(node) { this.next = node; }
  addEventListener() {}
}

class StubMap {
  on() {}
  addControl() {}
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
  const maplibregl = { Map: StubMap, NavigationControl: class {}, ScaleControl: class {} };
  const context = vm.createContext({
    document, maplibregl, URL,
    location: { href: 'https://x/items/a', origin: 'https://x' },
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
