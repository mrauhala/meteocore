// Run with: node --test crates/api-3dtiles/tests/viewer.test.cjs
// Exercise the shipped script with delayed metadata/content events. No WebGL
// mock claims to validate rendering; these tests cover scheduling and state.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const html = fs.readFileSync(`${__dirname}/../viewer/index.html`, 'utf8');
const script = html.slice(html.lastIndexOf('<script>') + 8, html.lastIndexOf('</script>'))
  .replace(/loadCollections\(\);\s*$/, '');

class Event {
  listeners = new Set();
  addEventListener(fn) { this.listeners.add(fn); return () => this.listeners.delete(fn); }
  fire(value) { for (const fn of [...this.listeners]) fn(value); }
}
class Resource {
  constructor(options) { this.url = options.url; this.headers = {}; }
  clone(result) { return Object.assign(result || new Resource({url: this.url}), {url: this.url, headers: this.headers}); }
  getDerivedResource() { return this.clone(); }
}
class Tileset {
  root = { contentReady: false };
  tileLoad = new Event();
  tileFailed = new Event();
  tileUnload = new Event();
  dead = false;
  constructor(url, options) { this.url = url; Object.assign(this, options); }
  complete() { this.root.contentReady = true; this.tileLoad.fire(this.root); }
  fail() { this.tileFailed.fire({ message: '404 no echo' }); }
  isDestroyed() { return this.dead; }
  destroy() { this.dead = true; }
}
class Element {
  options = [];
  listeners = new Map();
  style = {};
  _value = '';
  set innerHTML(_) { this.options = []; this._value = ''; }
  set value(v) { this._value = v; }
  get value() { return this._value; }
  set selectedIndex(i) { this._value = this.options[i].value; }
  appendChild(o) { this.options.push(o); if (!this._value) this._value = o.value || ''; }
  addEventListener(name, fn) { this.listeners.set(name, fn); }
  querySelector() { return new Element(); }
}
const tick = () => new Promise(resolve => setImmediate(resolve));
async function until(predicate) {
  for (let i = 0; i < 100; i++) { if (predicate()) return; await tick(); }
  assert.fail('asynchronous state did not settle');
}
function harness(t, metadataTimeout = null) {
  const elements = new Map(), created = [], primitives = new Set(), timers = new Set(), intervals = [];
  const document = { hidden: false, addEventListener() {}, createElement: () => new Element(),
    getElementById(id) { if (!elements.has(id)) elements.set(id, new Element()); return elements.get(id); } };
  const viewer = { scene: { globe: {}, requestRender() {}, primitives: {
    add(ts) { primitives.add(ts); return ts; },
    remove(ts) { const found = primitives.delete(ts); if (found) ts.destroy(); return found; },
  } }, zoomTo: async () => true };
  const context = vm.createContext({ document, URLSearchParams, URL, AbortController, DOMException,
    location: { search: '', pathname: '/3dtiles/viewer', origin: 'https://test' },
    console: { error() {}, warn() {} },
    setTimeout(fn, ms) { const id = setTimeout(fn, ms); timers.add(id); return id; },
    clearTimeout(id) { clearTimeout(id); timers.delete(id); },
    setInterval(fn, ms) { intervals.push({fn, ms}); return intervals.length; }, clearInterval() {},
    fetch: async () => ({ok: true, json: async () => ({})}),
    Cesium: { Resource, Viewer: function () { return viewer; }, ImageryLayer: class {}, UrlTemplateImageryProvider: class {},
      Color: { BLACK: {} }, Math: { toRadians: x => x }, HeadingPitchRange: class {}, Cesium3DTileStyle: class {},
      Cesium3DTileset: { fromUrl: async (resource, options) => {
        // Cesium clones Resource twice before fetchJson: fromUrl and loadJson.
        const copy = resource.getDerivedResource().getDerivedResource();
        await copy.fetchJson();
        const ts = new Tileset(copy.url, options); created.push(ts); return ts;
      } } },
  });
  vm.runInContext(metadataTimeout === null ? script : script.replace('const FRAME_LOAD_TIMEOUT_MS = 120000;', `const FRAME_LOAD_TIMEOUT_MS = ${metadataTimeout};`), context);
  const run = code => vm.runInContext(code, context);
  run(`$coll.value = 'A'; metaCollection = 'A'; $qty.value = 'DBZH'; $rep.value = 'points'; $num.value = '5'; $res.value = 'med';`);
  t.after(() => { run('frameController?.abort()'); for (const timer of timers) clearTimeout(timer); });
  return { context, run, created, primitives, elements, intervals };
}
async function finishFrames(h, promise) {
  let done = false;
  promise.finally(() => { done = true; });
  await until(() => {
    for (const ts of h.created) if (h.primitives.has(ts) && !ts.dead && !ts.root.contentReady) ts.complete();
    return done;
  });
  await promise;
}

test('content readiness bounds preloads and playback; metadata alone is not a frame', async t => {
  const h = harness(t);
  h.run(`times = Array.from({length: 12}, (_, i) => 't' + i); frameIdx = 11;`);
  const loading = h.run('loadFrames(true)');
  await until(() => h.created.length === 6);
  await tick();
  assert.equal(h.created.length, 6);
  assert.equal(h.run('frames.filter(Boolean).length'), 0);
  h.run('startPlaying()');
  assert.equal(h.run('playing'), false);
  h.created[0].complete();
  await until(() => h.created.length === 7);
  assert.equal(h.run('frames.filter(Boolean).length'), 1);
  await finishFrames(h, loading);
  assert.equal(h.run('frames.filter(Boolean).length'), 12);
  assert.equal(h.run('tileCache.size'), 12);
  assert.ok(h.created.every(ts => !ts.preloadWhenHidden));
});

test('content failures leave no playable or cached frame and remove listeners', async t => {
  const h = harness(t);
  h.run(`times = ['a', 'b'];`);
  const loading = h.run('loadFrames()');
  await until(() => h.created.length === 2);
  for (const ts of h.created) ts.fail();
  await loading;
  assert.equal(h.run('frames.filter(Boolean).length'), 0);
  assert.equal(h.run('tileCache.size'), 0);
  assert.equal(h.primitives.size, 0);
  assert.ok(h.created.every(ts => ts.dead && ts.tileLoad.listeners.size === 1 && ts.tileFailed.listeners.size === 0));
  assert.equal(h.elements.get('status').textContent, 'no data for this selection');
});

test('switching controls aborts metadata before reusing the six shared slots', async t => {
  const h = harness(t), pending = [];
  let active = 0, peak = 0, aborted = 0;
  h.context.fetch = (url, {signal}) => new Promise((resolve, reject) => {
    active++; peak = Math.max(peak, active);
    const cancel = () => { active--; aborted++; reject(signal.reason); };
    signal.addEventListener('abort', cancel, {once:true});
    pending.push(() => {
      if (signal.aborted) return;
      signal.removeEventListener('abort', cancel); active--;
      resolve({ok:true, json:async()=>({})});
    });
  });
  h.run(`times = Array.from({length: 8}, (_, i) => 't' + i);`);
  const old = h.run('loadFrames()');
  await until(() => pending.length === 6);
  const latest = h.run(`$qty.value = 'TH'; loadFrames()`);
  await old;
  await until(() => pending.length === 12);
  assert.equal(aborted, 6);
  assert.equal(peak, 6);
  assert.equal(h.created.length, 0);
  pending.splice(0).forEach(resolve => resolve());
  await until(() => h.primitives.size === 6);
  for (const ts of h.created) ts.complete();
  await until(() => pending.length === 2);
  pending.splice(0).forEach(resolve => resolve());
  await finishFrames(h, latest);
  assert.equal(h.run('frames.filter(Boolean).length'), 8);
  assert.equal(active, 0);
});

test('stalled metadata times out, aborts its fetch, and leaves the queue usable', async t => {
  const h = harness(t, 20);
  let aborted = 0;
  h.context.fetch = (url, {signal}) => new Promise((_, reject) => {
    signal.addEventListener('abort', () => { aborted++; reject(signal.reason); }, {once:true});
  });
  h.run(`times = ['a','b','c','d','e','f'];`);
  await h.run('loadFrames()');
  await until(() => h.run('activeFrameLoads') === 0);
  assert.equal(aborted, 6);
  assert.equal(h.created.length, 0);
  assert.equal(h.run('tileCache.size'), 0);
  h.context.fetch = async () => ({ok:true, json:async()=>({})});
  await finishFrames(h, h.run('loadFrames()'));
  assert.equal(h.run('frames.filter(isFrameReady).length'), 6);
});

test('late metadata from radar A cannot overwrite radar B', async t => {
  const h = harness(t), replies = new Map(), loads = [];
  h.context.fetch = url => new Promise(resolve => replies.set(url, resolve));
  h.context.captureLoad = () => loads.push(h.run('metaCollection + ":" + times.join(",")'));
  h.run('loadFrames = async () => captureLoad()');
  const a = h.run('loadCollectionMeta(true)');
  const b = h.run(`$coll.value = 'B'; loadCollectionMeta(true)`);
  replies.get('https://test/3dtiles/collections/B')({ok:true, json:async()=>({times:['B-time']})});
  await b;
  replies.get('https://test/3dtiles/collections/A')({ok:true, json:async()=>({times:['A-time']})});
  await a;
  assert.deepEqual(loads, ['B:B-time']);
  assert.equal(h.run('times[0]'), 'B-time');
});

test('manifest refresh fetches only new frames, follows latest, and preserves a historical seek', async t => {
  const h = harness(t);
  h.run(`times = ['a', 'b']; frameIdx = 1;`);
  await finishFrames(h, h.run('loadFrames()'));
  const original = h.run('frames[0]');
  h.context.fetch = async () => ({ok:true, json:async()=>({times:['a','b','c']})});
  await finishFrames(h, h.run('refreshCollectionTimes()'));
  assert.equal(h.created.length, 3);
  assert.equal(h.run('frames[0]'), original);
  assert.equal(h.run('frameTimes[frameIdx]'), 'c');
  h.run('showFrame(0)');
  h.context.fetch = async () => ({ok:true, json:async()=>({times:['b','c','d']})});
  await finishFrames(h, h.run('refreshCollectionTimes()'));
  assert.equal(h.created.length, 4);
  assert.equal(h.run('frameTimes[frameIdx]'), 'b'); // retired seek clamps to oldest retained
  assert.equal(h.intervals[0].ms, 60000);
});

test('a manifest response racing a collection change is ignored', async t => {
  const h = harness(t); let reply;
  h.run(`times = ['a']; frameTimes = ['a'];`);
  h.context.fetch = () => new Promise(resolve => { reply = resolve; });
  const refresh = h.run('refreshCollectionTimes()');
  h.run(`++metadataToken; $coll.value = 'B'; metaCollection = 'B'; times = ['B-time'];`);
  reply({ok:true, json:async()=>({times:['old-A','new-A']})});
  await refresh;
  assert.equal(h.run('times[0]'), 'B-time');
  assert.equal(h.created.length, 0);
});


test('manifest refresh retains the fetched point floor after client-side filtering', async t => {
  const h = harness(t);
  h.run(`times = ['a', 'b']; frameIdx = 1;`);
  await finishFrames(h, h.run('loadFrames()'));
  const first = h.run('frames[0]');
  h.run(`$num.value = '20'; pointsClientFilter()`);
  h.context.fetch = async () => ({ok:true, json:async()=>({times:['a','b','c']})});
  await finishFrames(h, h.run('refreshCollectionTimes()'));
  assert.equal(h.created.length, 3);
  assert.equal(h.run('frames[0]'), first);
  assert.equal(h.run('pointsStyleMin'), 20);
  assert.equal(h.run('pointsFetchFloor'), 5);
  assert.match(h.created[2].url, /min_value=5/);
});


test('an evicted root is no longer playable and is fetched again on reuse', async t => {
  const h = harness(t);
  h.run(`times = ['a', 'b'];`);
  await finishFrames(h, h.run('loadFrames()'));
  const evicted = h.run('frames[0]');
  evicted.tileUnload.fire(evicted.root);
  assert.equal(h.run('nextLoadedFrame(0)'), 1);
  await finishFrames(h, h.run('loadFrames()'));
  assert.equal(h.created.length, 3);
  assert.ok(evicted.dead);
  assert.equal(h.run('frames.filter(isFrameReady).length'), 2);
});


test('pausing during a manifest fill is not undone when new content finishes', async t => {
  const h = harness(t);
  h.run(`times = ['a', 'b']; frameIdx = 1;`);
  await finishFrames(h, h.run('loadFrames()'));
  h.run('startPlaying()');
  h.context.fetch = async () => ({ok:true, json:async()=>({times:['a','b','c']})});
  const refresh = h.run('refreshCollectionTimes()');
  await until(() => h.created.length === 3 && h.primitives.size === 3);
  assert.equal(h.run('playing'), true);
  h.elements.get('play').listeners.get('click')();
  assert.equal(h.run('playing'), false);
  await finishFrames(h, refresh);
  assert.equal(h.run('playing'), false);
});
