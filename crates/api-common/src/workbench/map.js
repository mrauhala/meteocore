// Shared locator for collection extents and this page's feature geometry.
// Natural Earth is bundled locally; no third-party map service is contacted.
(function initializeMap() {
  'use strict';
  // Bind only once the whole document exists.
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded',initializeMap,{once:true});
    return;
  }
  const status = document.getElementById('map-status');
  const element = document.getElementById('feature-map');
  // A collection's map (#1220): its panel, and the stage the map and its
  // floating panels fill.
  const panel = element.closest('.map-raster');
  const stage = document.getElementById('map-stage');
  // Without MapLibre or WebGL nothing on the panel works: hide the map with
  // every control, and say where this page lists the same facts.
  function unavailable() {
    ['map-stage','map-controls','map-request','map-legend'].forEach(id => { const node = document.getElementById(id); if (node) node.hidden = true; });
    element.hidden = true;
    panel?.classList.remove('map-live');
    status.textContent = `This browser cannot draw the map. ${element.dataset.fallback || ''}`.trim();
  }
  if (typeof maplibregl === 'undefined') { unavailable(); return; }
  let started = false;
  try {
    const data = JSON.parse(document.getElementById('map-data').textContent);
    // The data layer's opacity over the basemap by default; the legend is
    // drawn at it too, so its colours match the map's.
    const DATA_OPACITY = 0.85;
    // The map's colours are the theme's tokens (style.css `--map-*`), read
    // when drawn and again on every theme change: water, land, borders over
    // the data on a halo of the theme's background, and the outline of the
    // extent and of vector features.
    const TOKENS = { water: '--map-water', land: '--map-land', border: '--map-border', halo: '--map-border-halo', ink: '--map-ink' };
    const palette = () => {
      const css = getComputedStyle(document.documentElement);
      const colors = {};
      Object.entries(TOKENS).forEach(([key, token]) => { colors[key] = css.getPropertyValue(token).trim() || 'transparent'; });
      return colors;
    };
    // A collection page's map keeps its view and choices in the URL
    // fragment (#1220), under the API's own parameter names:
    // `#map=<zoom>/<lat>/<lon>&parameter-name=t&style=…&datetime=…&elevation=…&mode=tiles`,
    // and the time bar's loop as `window=<start>/<end>`. Any other
    // fragment, such as the `#metadata` tab, is not the map's.
    const FRAGMENT_KEYS = ['map','parameter-name','style','datetime','elevation','mode','window'];
    function readFragment() {
      const text = location.hash.replace(/^#/,'');
      if (!text.includes('=')) return undefined;
      const params = new URLSearchParams(text);
      const keys = [...params.keys()];
      if (!keys.length || !keys.every(key => FRAGMENT_KEYS.includes(key))) return undefined;
      const saved = {};
      keys.forEach(key => { saved[key] = params.get(key); });
      const view = /^(-?[\d.]+)\/(-?[\d.]+)\/(-?[\d.]+)$/.exec(saved.map || '');
      if (view) {
        const [zoom, lat, lng] = view.slice(1).map(Number);
        if ([zoom, lat, lng].every(Number.isFinite)) saved.view = { zoom, center: [lng, lat] };
      }
      return saved;
    }
    const saved = data.mapRequest ? readFragment() : undefined;
    const options = {
      container: element,
      style: { version: 8, sources: {}, layers: [{ id: 'background', type: 'background', paint: { 'background-color': palette().water } }] },
      center: [10, 30], zoom: 1,
      // A collection page's map sits on a long page: the wheel and one
      // finger scroll it, and the map pans with a modifier or two fingers.
      cooperativeGestures: element.dataset.quicklook !== 'true'
    };
    // A collection's map adds its own attribution, which names the data's
    // licence beside Natural Earth.
    if (data.mapRequest) options.attributionControl = false;
    const map = new maplibregl.Map(options);
    map.on('load',() => { started = true; });
    // Once the map runs, its status line is the chip's live region.
    panel?.classList.add('map-live');
    // The view as a CRS84 box, west,south,east,north, for an items page's
    // "Use map view" (#1143). Longitudes wrap into ±180: a view across the
    // antimeridian has west > east, as Features `bbox` takes it.
    const wrapLongitude = lon => ((lon + 180) % 360 + 360) % 360 - 180;
    function recordView() {
      const view = map.getBounds();
      const round = n => Number(n.toFixed(4));
      const whole = view.getEast() - view.getWest() >= 360;
      const west = whole ? -180 : wrapLongitude(view.getWest());
      const east = whole ? 180 : wrapLongitude(view.getEast());
      const lat = n => Math.max(-90, Math.min(90, n));
      element.dataset.view = [west, lat(view.getSouth()), east === -180 ? 180 : east, lat(view.getNorth())].map(round).join(',');
    }
    map.on('moveend',recordView);
    // The basemap is the bundled Natural Earth only (#1220): land and
    // borders, borders only, or none, with country names on or off.
    const basemap = { mode: 'land', labels: true };
    function applyBasemap() {
      const show = (id, on) => { if (map.getLayer(id)) map.setLayoutProperty(id, 'visibility', on ? 'visible' : 'none'); };
      show('land', basemap.mode === 'land');
      show('border-halo', basemap.mode !== 'none');
      show('borders', basemap.mode !== 'none');
      element.classList.toggle('map-no-labels', !basemap.labels);
    }
    if (data.mapRequest) {
      // Top right: the full window and the basemap beside the zoom; bottom
      // right: the scale over a compact attribution that also names the
      // collection's licence.
      const tools = document.getElementById('map-tools');
      if (tools) map.addControl({ onAdd: () => { tools.classList.add('maplibregl-ctrl', 'maplibregl-ctrl-group'); return tools; }, onRemove: () => tools.remove() }, 'top-right');
      map.addControl(new maplibregl.NavigationControl({ showCompass: false }), 'top-right');
      const escapeHtml = text => String(text).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
      const license = data.mapRequest.license;
      let licenseHtml = [];
      if (license?.title) {
        let href;
        try { href = license.href && ['http:','https:'].includes(new URL(license.href, location.href).protocol) ? license.href : undefined; } catch (_) {}
        licenseHtml = [`Data: ${href ? `<a href="${escapeHtml(href)}">${escapeHtml(license.title)}</a>` : escapeHtml(license.title)}`];
      }
      map.addControl(new maplibregl.AttributionControl({ compact: true, customAttribution: licenseHtml }), 'bottom-right');
      map.addControl(new maplibregl.ScaleControl({ unit: 'metric' }), 'bottom-right');
    } else {
      map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
      map.addControl(new maplibregl.ScaleControl({ unit: 'metric' }));
    }
    const inspector = document.createElement('div');
    inspector.className = 'mini-inspector';
    inspector.setAttribute('aria-live', 'polite');
    const selection = document.createElement('div');
    selection.className = 'map-selection';
    status.after(selection);
    if (element.dataset.quicklook === 'true') selection.after(inspector);
    // A property's number as the workbench's tables show it (api-common
    // `workbench::metadata::number_text`, #1140, over ds-core's
    // `display_number` and `display_coordinate`): an integral float without
    // `.0`, which JSON.parse already drops, and a latitude or longitude to
    // at most 5 decimals, about a metre. The second value says whether the
    // text is rounded.
    function coordinateKey(key) {
      const name = key.replace(/[A-Z]/g, c => c.toLowerCase());
      return ['lat','lon','lng','latitude','longitude'].includes(name)
        || ['_lat','_lon','_lng','latitude','longitude'].some(end => name.endsWith(end));
    }
    function numberText(key, n) {
      if (!Number.isFinite(n) || !coordinateKey(key)) return [String(n), false];
      // An exact tie at 5 decimals is an odd multiple of 1/64 (60.015625):
      // Rust rounds it to even, toFixed away from zero. Both scalings are
      // exact.
      let fixed = n;
      if (Number.isInteger(n * 64) && !Number.isInteger(n * 32)) {
        const floor = Math.floor(n * 1e5);
        fixed = (floor % 2 === 0 ? floor : floor + 1) / 1e5;
      }
      let text = fixed.toFixed(5).replace(/0+$/, '').replace(/\.$/, '');
      if (text === '-0') text = '0';
      return [text, Number(text) !== n];
    }
    function inspect(feature) {
      inspector.replaceChildren();
      const eyebrow = document.createElement('span'); eyebrow.className = 'eyebrow'; eyebrow.textContent = 'QUICK LOOK'; inspector.append(eyebrow);
      const heading = document.createElement('h3'); heading.textContent = feature.properties.label || String(feature.id); inspector.append(heading);
      const facts = document.createElement('div'); facts.className = 'mini-facts';
      Object.entries(feature.properties.facts || { Geometry: feature.geometry?.type || 'Not available' }).forEach(([label,value]) => {
        const fact = document.createElement('div'); const key = document.createElement('small'); key.textContent = label;
        const text = document.createElement('strong'); text.textContent = value === null ? 'Not available' : String(value);
        if (typeof value === 'number') {
          // A rounded value keeps the exact one in its title.
          const [shown, rounded] = numberText(label, value);
          text.textContent = shown;
          if (rounded) { text.className = 'rounded'; text.title = String(value); }
        }
        fact.append(key,text); facts.append(fact);
      });
      inspector.append(facts);
      if (feature.properties.href) {
        const href = new URL(feature.properties.href, location.href);
        if (href.origin === location.origin) {
          const link = document.createElement('a'); link.className = 'btn primary'; link.href = href.href; link.textContent = 'Open item →'; inspector.append(link);
          document.querySelectorAll('.item-table tbody tr').forEach(row => row.classList.toggle('selected',row.querySelector('a')?.href === href.href));
        }
      }
    }
    if (element.dataset.quicklook === 'true' && data.features.length) inspect(data.features[0]);
    // Render the viewport in Web Mercator so image pixels align with MapLibre.
    // Bounds come from the map; projection math remains in the existing API.
    // Draw the collection with its map tiles, Maps (one image per view), WMS
    // GetMap or its vector tiles, whichever it offers; a switch picks among
    // several. A time bar under the map steps through and plays its axis.
    function installRasterPreview(extentBounds) {
      const request = data.mapRequest;
      const modes = request.modes || ['maps'];
      const byId = id => document.getElementById(id);
      const block = byId('map-controls');
      const form = byId('map-form');
      const modeInputs = [...document.querySelectorAll('input[name="map-mode"]')];
      const parameter = byId('map-parameter');
      const style = byId('map-style');
      // The form's date and time: the no-JS request's, and the script's only
      // for a collection whose time has no axis to bind the time bar to.
      const time = byId('map-time');
      const level = byId('map-level');
      const help = byId('map-help');
      const generalHelp = help?.textContent;
      const parameterField = parameter?.closest('label');
      const styleField = style.closest('label');
      const timeField = time?.closest('.map-time-field');
      const levelField = level?.closest('label');
      const parameters = request.parameter_names || [];
      // A collection crossing the antimeridian comes as one unwrapped
      // polygon (east > 180). Tile sources clamp their bounds to ±180, so
      // such a tileset is given none.
      const tileBounds = extentBounds && !extentBounds.isEmpty() && extentBounds.getWest() >= -180 && extentBounds.getEast() <= 180
        ? extentBounds.toArray().flat() : undefined;
      // The parameter the server draws when the choice is left to it, named
      // by its legend.
      let defaultParameter;
      const MINUTE = 6e4, HOUR = 36e5, DAY = 864e5;
      // UTC to the minute, seconds only when present, as the options read.
      function formatTime(date) {
        const iso = date.toISOString();
        return `${iso.slice(0,10)} ${iso.slice(11, iso.slice(17,23) === '00.000' ? 16 : 19)}`;
      }
      // An instant as `datetime` takes it and the fragment keeps it: RFC
      // 3339 in UTC, milliseconds only when present.
      function instant(ms) {
        const iso = new Date(ms).toISOString();
        return iso.endsWith('.000Z') ? `${iso.slice(0,19)}Z` : iso;
      }
      const WEEKDAYS = ['Sun','Mon','Tue','Wed','Thu','Fri','Sat'];
      const weekday = ms => WEEKDAYS[new Date(ms).getUTCDay()];
      // The time bar's label: `Sun 2026-04-05 21:00 UTC`, UTC only (#1220).
      const timeText = ms => `${weekday(ms)} ${formatTime(new Date(ms))} UTC`;
      // A length of time in words, rounded: `45 min`, `3 h`, `2 d`; to a
      // tenth of an hour or a day when `exact`.
      function lengthText(ms, exact) {
        const round = n => exact ? Number(n.toFixed(1)) : Math.round(n);
        if (ms < 59.5 * MINUTE) return `${Math.round(ms / MINUTE)} min`;
        if (ms < 48 * HOUR) return `${round(ms / HOUR)} h`;
        return `${round(ms / DAY)} d`;
      }
      // How far an instant is from the viewer's clock: `25 min ago`, `in 3 h`.
      function relativeText(ms) {
        const delta = ms - Date.now();
        if (Math.abs(delta) < MINUTE) return 'now';
        return delta < 0 ? `${lengthText(-delta)} ago` : `in ${lengthText(delta)}`;
      }
      function findParameter(name) {
        return parameters.find(p => p.name === name) || parameters.find(p => p.label === name);
      }
      function parameterTitle(name, unit) {
        const known = findParameter(name);
        const symbol = unit || known?.unit;
        return symbol ? `${known?.label || name} (${symbol})` : (known?.label || name);
      }
      // A select's option matching `test`, chosen; others keep theirs.
      function choose(select, test) {
        const index = select ? [...select.options].findIndex(test) : -1;
        if (index >= 0) select.selectedIndex = index;
        return index >= 0;
      }

      // The time axes `mapRequest` carries (#1220): `{start, step, count}`
      // for a regular grid, `step` in seconds, or `{times: [...]}`. Decoded
      // to milliseconds, ascending, each instant once.
      function decodeAxis(axis) {
        if (!axis || typeof axis !== 'object') return undefined;
        let times;
        if (Array.isArray(axis.times)) times = axis.times.map(t => Date.parse(t)).filter(Number.isFinite);
        else {
          const start = Date.parse(axis.start), step = Number(axis.step) * 1000, count = Number(axis.count);
          if (!Number.isFinite(start) || !(step > 0) || !Number.isInteger(count) || count < 1 || count > 1e6) return undefined;
          times = Array.from({ length: count }, (_, i) => start + i * step);
        }
        times.sort((a, b) => a - b);
        times = times.filter((t, i) => i === 0 || t !== times[i - 1]);
        return times.length ? times : undefined;
      }
      const collectionAxis = decodeAxis(request.axis) || [];
      const parameterAxes = new Map();
      // The axis a parameter is drawn on: its own when it has one (#819),
      // else the collection's.
      function axisOf(name) {
        const known = findParameter(name);
        if (!known?.axis) return collectionAxis;
        if (!parameterAxes.has(known.name)) parameterAxes.set(known.name, decodeAxis(known.axis) || collectionAxis);
        return parameterAxes.get(known.name);
      }

      // The fragment's choices, where this page offers them. A value it
      // does not offer keeps the control's default.
      function restore(saved) {
        if (!saved) return;
        if (saved.mode && modeInputs.some(input => input.value === saved.mode)) modeInputs.forEach(input => { input.checked = input.value === saved.mode; });
        if (saved['parameter-name']) choose(parameter, option => option.value === saved['parameter-name']);
        if (saved.style) choose(style, option => option.dataset.id === saved.style);
        if (saved.elevation) choose(level, option => option.value !== '' && Number(option.value) === Number(saved.elevation));
        const ms = Date.parse(saved.datetime || '');
        if (time && Number.isFinite(ms)) time.value = instant(ms).slice(0,19);
      }
      function selectedTimeValue() {
        if (!time?.value) return '';
        const ms = Date.parse(`${time.value}Z`);
        return Number.isFinite(ms) ? instant(ms) : '';
      }
      restore(saved);
      // What the map draws: one object, which every request reads. A
      // control writes its own key on `change`, so a pan, a step or a retry
      // never picks up an edit the viewer has not made, and nothing waits
      // for a button. Keys are the API's parameter names.
      const state = {
        mode: modeInputs.find(input => input.checked)?.value || modes[0],
        'parameter-name': parameter?.value || '',
        style: Math.max(0, style.selectedIndex),
        datetime: '',
        elevation: level?.value || ''
      };
      const LAYER_KEYS = ['mode','parameter-name','style','elevation'];
      function apply(change) {
        const changed = Object.keys(change).filter(key => state[key] !== change[key]);
        Object.assign(state, change);
        if (!changed.length) return;
        if (changed.includes('mode')) applyMode();
        // Another layer: nothing buffered or failed for the old one counts.
        if (changed.some(key => LAYER_KEYS.includes(key))) resetFrames();
        if (changed.includes('parameter-name')) rebind();
        syncLayer();
        render();
        saveFragment();
      }

      // The floating panels (#1220). Narrow screens get a bottom sheet for
      // the layer card, which then also holds the basemap and opacity, and
      // a folded legend strip.
      const narrow = typeof matchMedia === 'function' ? matchMedia('(max-width: 600px)') : undefined;
      const isNarrow = () => Boolean(narrow?.matches);
      const layerBody = byId('map-layer-body');
      const pill = byId('map-layer-pill');
      const pillText = byId('map-layer-text');
      const levelText = value => [...(level?.options || [])].find(option => option.value === value)?.textContent || value;
      // The pill names what is drawn: "Temperature · 850 hPa".
      function layerSummary() {
        if (state.mode === 'vector') return 'Vector tiles';
        const name = state['parameter-name'] || defaultParameter || (parameters.length === 1 ? parameters[0].name : '');
        const parts = [name ? findParameter(name)?.label || name : 'Map layer'];
        if (state.style > 0) parts.push(style.options[state.style]?.textContent);
        if (state.elevation) parts.push(levelText(state.elevation));
        return parts.filter(Boolean).join(' · ');
      }
      // The layer card open or folded into its pill; on a narrow screen,
      // open is the bottom sheet, which takes focus and gives it back.
      function setCard(open, refocus) {
        if (!block) return;
        block.classList.toggle('open', open);
        pill?.setAttribute('aria-expanded', String(open));
        if (!isNarrow()) return;
        if (open) [...(layerBody?.querySelectorAll?.('select,input,button') || [])].find(node => !node.disabled && node.offsetParent !== null)?.focus();
        else if (refocus) pill?.focus();
      }
      pill?.addEventListener('click',() => setCard(!block.classList.contains('open')));
      byId('map-sheet-close')?.addEventListener('click',() => setCard(false, true));
      // The level ladder: one button per level, pressed for the level drawn.
      const ladder = byId('map-ladder');
      const ladderButtons = ladder ? [...ladder.children].filter(node => node.tagName === 'BUTTON') : [];
      ladderButtons.forEach(button => button.addEventListener('click',() => {
        if (choose(level, option => option.value !== '' && Number(option.value) === Number(button.dataset.level))) apply({ elevation: level.value });
      }));
      function syncLayer() {
        if (pillText) pillText.textContent = layerSummary();
        ladderButtons.forEach(button => button.setAttribute('aria-pressed', String(state.elevation !== '' && Number(button.dataset.level) === Number(state.elevation))));
      }
      // Top right: the basemap and the data's opacity in a popover, or in
      // the sheet on a narrow screen.
      const tools = byId('map-tools');
      const basemapButton = byId('map-basemap-button');
      const basemapPanel = byId('map-basemap');
      const opacityInput = byId('map-opacity');
      const opacityText = byId('map-opacity-value');
      let dataOpacity = DATA_OPACITY;
      function setPopover(open) {
        if (!basemapPanel || isNarrow()) return;
        basemapPanel.hidden = !open;
        basemapButton?.setAttribute('aria-expanded', String(open));
      }
      function placeBasemap() {
        if (!basemapPanel) return;
        if (isNarrow()) { layerBody?.append(basemapPanel); basemapPanel.hidden = false; basemapButton?.setAttribute('aria-expanded', 'false'); }
        else if (tools && basemapPanel.parentNode !== tools) { tools.append(basemapPanel); basemapPanel.hidden = true; }
      }
      basemapButton?.addEventListener('click',() => setPopover(basemapPanel.hidden));
      document.querySelectorAll('input[name="map-basemap"]').forEach(input => input.addEventListener('change',() => {
        if (!input.checked) return;
        basemap.mode = input.value; applyBasemap();
      }));
      const labelsInput = byId('map-labels');
      labelsInput?.addEventListener('change',() => { basemap.labels = labelsInput.checked; applyBasemap(); });
      // The data's opacity, 0–100 %: the frame drawn now and every later
      // one, and the legend with them.
      function setOpacity(value) {
        dataOpacity = Math.max(0, Math.min(1, value));
        if (front?.id && map.getLayer(front.id)) map.setPaintProperty(front.id,'raster-opacity',dataOpacity);
        if (opacityText) opacityText.textContent = `${Math.round(dataOpacity * 100)} %`;
        if (legendBar) legendBar.style.opacity = String(dataOpacity);
      }
      opacityInput?.addEventListener('input',() => setOpacity(Number(opacityInput.value) / 100));
      // The full window: the map over the page, the wheel and one finger
      // its own; Esc or the button leaves it.
      const expandButton = byId('map-expand');
      const expanded = () => Boolean(stage?.classList.contains('expanded'));
      function setExpanded(on) {
        if (!stage || expanded() === on) return;
        stage.classList.toggle('expanded', on);
        document.documentElement.classList.toggle('map-expanded', on);
        expandButton?.setAttribute('aria-pressed', String(on));
        if (on) map.cooperativeGestures?.disable?.(); else if (element.dataset.quicklook !== 'true') map.cooperativeGestures?.enable?.();
        map.resize();
        measureDock();
      }
      expandButton?.addEventListener('click',() => setExpanded(!expanded()));
      // The legend folds to its title and bar; a narrow screen starts with
      // the strip.
      const legendToggle = byId('map-legend-toggle');
      let legendBar;
      function foldLegend(folded) {
        legendBox?.classList.toggle('folded', folded);
        legendToggle?.setAttribute('aria-expanded', String(!folded));
      }
      legendToggle?.addEventListener('click',() => foldLegend(!legendBox.classList.contains('folded')));
      // The status chip, top centre: what is loading, retrying or missing.
      // It is hidden from assistive technology, which hears the status
      // line; a notice fades after 4 s, a load or a failure stays.
      const chip = byId('map-chip');
      let chipTimer, chipBuffering = false;
      function showChip(text, kind) {
        if (!chip) return;
        clearTimeout(chipTimer);
        chipBuffering = false;
        chip.textContent = text || '';
        chip.hidden = !text;
        chip.classList.toggle('stale', kind === 'stale');
        if (text && !kind) chipTimer = setTimeout(() => { chip.hidden = true; }, 4000);
      }
      // Narrow screens: the sheet and strip; wide ones: the card and popover.
      function placePanels() {
        placeBasemap();
        if (isNarrow()) { setCard(false); foldLegend(true); }
      }
      narrow?.addEventListener?.('change',() => { placePanels(); if (!isNarrow()) { setCard(true); foldLegend(false); } });
      // A tap outside closes the sheet or the popover.
      document.addEventListener('pointerdown',event => {
        const inside = node => Boolean(node && event.target && (node === event.target || node.contains?.(event.target)));
        if (isNarrow() && block?.classList.contains('open') && !inside(block)) setCard(false);
        if (!isNarrow() && basemapPanel && !basemapPanel.hidden && !inside(basemapPanel) && !inside(basemapButton)) setPopover(false);
      });
      // Esc closes the innermost open panel, else leaves the full window.
      document.addEventListener('keydown',event => {
        if (event.key !== 'Escape' || event.defaultPrevented) return;
        if (isNarrow() && block?.classList.contains('open')) setCard(false, true);
        else if (basemapPanel && !basemapPanel.hidden && !isNarrow()) { setPopover(false); basemapButton?.focus(); }
        else if (byId('map-time-menu')?.open) byId('map-time-menu').open = false;
        else if (keysHelp && !keysHelp.hidden) toggleHelp(false);
        else if (expanded()) { setExpanded(false); expandButton?.focus(); }
        else return;
        event.preventDefault();
      });
      // How far the docked time bar reaches up the map: the legend, the
      // scale and the attribution sit above it.
      function measureDock() {
        if (!stage?.style?.setProperty) return;
        const docked = bar && !bar.hidden && getComputedStyle(bar).position === 'absolute';
        let dock = docked ? bar.offsetHeight : 0;
        // A narrow screen's legend is a strip along the map's bottom.
        if (isNarrow() && legendBox && !legendBox.hidden) dock += legendBox.offsetHeight;
        stage.style.setProperty('--map-dock', `${dock}px`);
      }
      parameter?.addEventListener('change',() => apply({ 'parameter-name': parameter.value }));
      style.addEventListener('change',() => apply({ style: Math.max(0, style.selectedIndex) }));
      level?.addEventListener('change',() => apply({ elevation: level.value }));
      time?.addEventListener('change',() => { if (!axis.length) apply({ datetime: selectedTimeValue() }); });
      modeInputs.forEach(input => input.addEventListener('change',() => { if (input.checked) apply({ mode: input.value }); }));
      // Without the script the form opens the Maps image. With it, Enter in
      // the date and time field applies that time, and the page stays.
      form?.addEventListener('submit',event => {
        event.preventDefault();
        if (time && !axis.length) apply({ datetime: selectedTimeValue() });
      });

      // The time bar (#1220): the axis of the parameter drawn, the playhead
      // (`head`, an index into it) and the loop window (`win`, indices).
      const bar = byId('map-timebar');
      const track = byId('map-track');
      const marksBox = byId('map-track-marks');
      const bufferBox = byId('map-track-buffer');
      const band = byId('map-window');
      const slider = byId('map-playhead');
      const playheadMark = byId('map-playhead-mark');
      const playButton = byId('map-play');
      const speedSelect = byId('map-speed');
      const repeatInputs = [...document.querySelectorAll('input[name="map-repeat"]')];
      const drawnLabel = byId('map-time-drawn');
      const relativeLabel = byId('map-time-relative');
      const seekLabel = byId('map-time-seek');
      const windowText = byId('map-window-text');
      const menu = byId('map-time-menu');
      const jump = byId('map-jump');
      const keysButton = byId('map-keys');
      const keysHelp = byId('map-keys-help');
      // Observations end no later than about now; a forecast reaches on.
      // The margin keeps a radar whose newest frame is a little ahead of a
      // slow clock an observation.
      const NOW_MARGIN = 10 * MINUTE;
      // The loop window: 24 frames by default, at most 96.
      const WINDOW_FRAMES = 24, WINDOW_CAP = 96;
      const SPEEDS = [1, 2, 4, 8];
      // The last frame of a loop is held this long before it repeats.
      const LAST_HOLD = 1200;
      // A drag requests the frame under the pointer once it rests this long.
      const DRAG_REST = 150;
      let axis = [], cadence = Infinity, head = -1, win = { lo: 0, hi: -1 }, userWindow = false;
      let playing = false, holding = false, direction = 1, playTimer, speedChosen = false;
      // The viewer has used the time bar: only then are frames buffered
      // ahead, so a page that is only looked at costs one frame.
      let engaged = false, playedOnce = false;
      // Frames of the current layer that failed or had no data, by instant:
      // marked on the track and skipped in playback.
      const marks = new Map();
      const connection = typeof navigator === 'object' ? navigator.connection : undefined;
      const saveData = Boolean(connection?.saveData);
      const metered = saveData || /^(slow-2g|2g|3g)$/.test(connection?.effectiveType || '') || connection?.type === 'cellular';
      function nearestIndex(ms) {
        let lo = 0, hi = axis.length - 1;
        while (lo < hi) { const mid = (lo + hi) >> 1; if (axis[mid] < ms) lo = mid + 1; else hi = mid; }
        return lo > 0 && ms - axis[lo - 1] <= axis[lo] - ms ? lo - 1 : lo;
      }
      const isForecast = () => axis.length > 0 && axis[axis.length - 1] > Date.now() + NOW_MARGIN;
      // The time shown first: the latest frame of observations, and for a
      // forecast the step nearest the viewer's clock. A request always names
      // it, so even the first frame is cacheable and labelled with its time.
      // The collection document names no default time of its own.
      const openingIndex = () => isForecast() ? nearestIndex(Date.now()) : axis.length - 1;
      // The default loop: the last 24 frames of observations, a forecast's
      // 24 from the step nearest now, and every frame when there are fewer.
      function defaultWindow() {
        const size = Math.min(WINDOW_FRAMES, axis.length);
        const lo = isForecast() ? Math.min(nearestIndex(Date.now()), axis.length - size) : axis.length - size;
        win = { lo, hi: lo + size - 1 };
      }
      function setWindow(lo, hi) {
        const last = axis.length - 1;
        lo = Math.max(0, Math.min(last, lo)); hi = Math.max(0, Math.min(last, hi));
        if (hi < lo) [lo, hi] = [hi, lo];
        win = { lo, hi: Math.min(hi, lo + WINDOW_CAP - 1) };
      }
      // One end of the loop at `index`; the other stays where it can.
      function setWindowEnd(end, index, quiet) {
        if (!axis.length) return;
        let { lo, hi } = win;
        if (end === 'lo') {
          lo = index;
          if (hi < lo) hi = Math.min(axis.length - 1, lo + WINDOW_FRAMES - 1);
          hi = Math.min(hi, lo + WINDOW_CAP - 1);
        } else {
          hi = index;
          if (lo > hi) lo = Math.max(0, hi - WINDOW_FRAMES + 1);
          lo = Math.max(lo, hi - WINDOW_CAP + 1);
        }
        win = { lo, hi }; userWindow = true;
        drawWindow();
        if (quiet) return;
        trimPool(); prefetch(); saveFragment();
        report(`Loop: ${windowSummary()}, ${timeText(axis[lo])} to ${timeText(axis[hi])}.`);
      }
      function windowSummary() {
        const count = win.hi - win.lo + 1;
        return `${count} frame${count === 1 ? '' : 's'} · ${lengthText(axis[win.hi] - axis[win.lo], true)}`;
      }
      // Bind the time bar to `times`, at the instant nearest `keep` (the
      // fragment's, or the one shown before a parameter change), with the
      // loop window nearest `keepWindow` when the viewer set one.
      function bindAxis(times, keep, keepWindow) {
        axis = times;
        if (bar) bar.hidden = !axis.length;
        measureDock();
        if (timeField) timeField.hidden = axis.length > 0;
        if (!axis.length) { head = -1; return; }
        head = Number.isFinite(keep) ? nearestIndex(keep) : openingIndex();
        const gaps = [];
        for (let i = 1; i < axis.length; i++) gaps.push(axis[i] - axis[i - 1]);
        gaps.sort((a, b) => a - b);
        cadence = gaps.length ? gaps[gaps.length >> 1] : Infinity;
        userWindow = Boolean(keepWindow);
        if (keepWindow) setWindow(nearestIndex(keepWindow[0]), nearestIndex(keepWindow[1])); else defaultWindow();
        state.datetime = instant(axis[head]);
        if (!bar) return;
        slider.min = '0';
        slider.max = String((axis[axis.length - 1] - axis[0]) / 1000);
        if (jump) { jump.min = instant(axis[0]).slice(0,19); jump.max = instant(axis[axis.length - 1]).slice(0,19); }
        // Four frames a second at radar cadences, two at model steps.
        if (!speedChosen && speedSelect) speedSelect.value = String(cadence <= 15 * MINUTE ? 4 : 2);
        drawTrack(); syncSlider();
      }
      // A parameter on its own axis rebinds the time bar to it, at the
      // instant nearest the one shown, as /preview does.
      function rebind() {
        const next = axisOf(state['parameter-name'] || defaultParameter || '');
        if (next === axis) return;
        const keep = head >= 0 ? axis[head] : undefined;
        const keepWindow = userWindow && axis.length ? [axis[win.lo], axis[win.hi]] : undefined;
        bindAxis(next, keep, keepWindow);
      }

      // The track, laid out by time: frame notches where they are at least
      // 4 px apart (else a strip that breaks at gaps), day and hour ticks,
      // the viewer's clock, the loop window and the buffered frames.
      const TICK_STEPS = [10 * MINUTE, 15 * MINUTE, 30 * MINUTE, HOUR, 2 * HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR,
        DAY, 2 * DAY, 7 * DAY, 14 * DAY, 28 * DAY, 91 * DAY, 364 * DAY];
      const percent = ms => {
        const total = axis[axis.length - 1] - axis[0];
        return total > 0 ? (ms - axis[0]) / total * 100 : 50;
      };
      function spanAt(className, ms, text) {
        const node = document.createElement('span');
        node.className = className; node.style.left = `${percent(ms)}%`;
        if (text) node.textContent = text;
        return node;
      }
      function tickText(ms, step) {
        const iso = new Date(ms).toISOString();
        if (step >= 28 * DAY) return iso.slice(0,10);
        if (ms % DAY === 0) return `${weekday(ms)} ${iso.slice(5,10)}`;
        return iso.slice(11,16);
      }
      function drawTrack() {
        if (!bar || !axis.length) return;
        const width = track.clientWidth || 600;
        const first = axis[0], last = axis[axis.length - 1], total = last - first;
        const nodes = [];
        let gap = Infinity;
        for (let i = 1; i < axis.length; i++) gap = Math.min(gap, axis[i] - axis[i - 1]);
        if (axis.length === 1 || width * gap / total >= 4) axis.forEach(ms => nodes.push(spanAt('map-notch', ms)));
        else {
          // Too dense for notches: where frames are, broken at gaps of more
          // than three steps.
          let start = first;
          for (let i = 1; i <= axis.length && nodes.length < 400; i++) {
            if (i < axis.length && axis[i] - axis[i - 1] <= 3 * cadence) continue;
            const strip = spanAt('map-strip', start);
            strip.style.width = `${Math.max(0.2, percent(axis[i - 1]) - percent(start))}%`;
            nodes.push(strip);
            if (i < axis.length) start = axis[i];
          }
        }
        if (total > 0) {
          const step = TICK_STEPS.find(s => width * s / total >= 72) || TICK_STEPS[TICK_STEPS.length - 1];
          let previous = -Infinity;
          for (let t = Math.ceil(first / step) * step, k = 0; t <= last && k < 200; t += step, k++) {
            const tick = spanAt(t % DAY === 0 ? 'map-tick day' : 'map-tick', t, tickText(t, step));
            // A label near the track's end reads leftward, never cut, and
            // is left out where it would meet the one before.
            if ((last - t) * width / total < 48) {
              if ((t - previous) * width / total < 100) continue;
              tick.dataset.edge = 'end';
            }
            previous = t;
            nodes.push(tick);
          }
          const now = Date.now();
          if (now >= first && now <= last) { const node = spanAt('map-now', now); node.title = 'Now'; nodes.push(node); }
        }
        marksBox.replaceChildren(...nodes);
        drawWindow(); drawBuffer(); moveMark();
      }
      function drawWindow() {
        if (!bar || !axis.length) return;
        band.style.left = `${percent(axis[win.lo])}%`;
        band.style.width = `${percent(axis[win.hi]) - percent(axis[win.lo])}%`;
        drawWindowText();
      }
      function drawWindowText() {
        if (!windowText || !axis.length) return;
        let text = `Loop: ${windowSummary()}`;
        // Before the first play on a metered connection: what it will cost.
        if (metered && !playedOnce) {
          const view = state.mode === 'tiles' ? undefined : imageView();
          let missing = 0;
          for (let i = win.lo; i <= win.hi; i++) if (!readyAt(i, view)) missing++;
          text += ` · about ${missing} frame${missing === 1 ? '' : 's'} to load on this metered connection`;
        }
        windowText.textContent = text;
      }
      // The frames ready to show, and the ones that failed.
      function drawBuffer() {
        if (!bar || !axis.length) return;
        const total = axis[axis.length - 1] - axis[0];
        const width = total > 0 && Number.isFinite(cadence) ? Math.max(0.3, cadence / total * 100) : 2;
        const layer = layerKey(state);
        const nodes = [];
        for (const frame of [front, ...pool.values()]) {
          if (!frame || frame.state !== 'ready' || layerKey(frame.want) !== layer || !Number.isFinite(frame.ms)) continue;
          const node = spanAt('map-buffered', frame.ms);
          node.style.left = `${percent(frame.ms) - width / 2}%`; node.style.width = `${width}%`;
          nodes.push(node);
        }
        marks.forEach((kind, ms) => {
          const node = spanAt(`map-errored ${kind}`, ms);
          node.title = `${kind === 'nodata' ? 'No data' : 'Failed'} at ${timeText(ms)}`;
          nodes.push(node);
        });
        bufferBox.replaceChildren(...nodes);
        drawWindowText();
        if (chipBuffering) bufferChip();
      }
      // The playhead mark sits at the frame drawn, moved in the paint that
      // shows it.
      function moveMark() {
        if (!bar || !axis.length) return;
        const ms = Number.isFinite(front?.ms) ? front.ms : axis[head];
        playheadMark.style.left = `${percent(ms)}%`;
      }
      // The slider is the playhead for assistive technology: it is moved by
      // the viewer, on a pause and on a step, never once per played frame.
      function syncSlider() {
        if (!bar || head < 0) return;
        slider.value = String((axis[head] - axis[0]) / 1000);
        slider.setAttribute('aria-valuetext', timeText(axis[head]));
      }
      // The label names the time drawn, changed in the paint that shows it:
      // a Maps image's `Content-Datetime`, else the time requested.
      function labelFrame(frame) {
        if (!bar || !frame.drawn) return;
        const ms = frame.drawn.getTime();
        const snapped = Number.isFinite(frame.ms) && frame.ms !== ms;
        drawnLabel.textContent = `${timeText(ms)}${snapped ? ' · nearest available' : ''}`;
        relativeLabel.textContent = relativeText(ms);
        seekLabel.hidden = true;
        moveMark();
      }
      // The track and the relative time follow the viewer's clock.
      if (bar) {
        (function refreshClock() {
          setTimeout(() => {
            if (document.visibilityState !== 'hidden') { drawTrack(); if (shown?.drawn) relativeLabel.textContent = relativeText(shown.drawn.getTime()); }
            refreshClock();
          }, 60000);
        })();
        new ResizeObserver(() => drawTrack()).observe(track);
      }

      // Playback. The next frame is shown once it has fully loaded; until
      // then the current one holds. Failed frames are skipped. There is no
      // autoplay: only the viewer starts it.
      const repeat = () => repeatInputs.find(input => input.checked)?.value || 'loop';
      const speed = () => Number(speedSelect?.value) || 4;
      const interval = () => 1000 / speed();
      // The frame after `index` in playback order, and the direction then:
      // in the loop window when `inside`, else on to the axis end.
      // Undefined at the end of a loop played once.
      function following(index, dir, inside) {
        if (!inside) return index + 1 < axis.length ? { index: index + 1, dir: 1 } : undefined;
        const { lo, hi } = win;
        if (index < lo || index > hi) return { index: lo, dir: 1 };
        if (repeat() === 'bounce') {
          if (lo === hi) return { index: lo, dir };
          let next = index + dir;
          if (next > hi || next < lo) { dir = -dir; next = index + dir; }
          return { index: next, dir };
        }
        if (index < hi) return { index: index + 1, dir: 1 };
        return repeat() === 'loop' ? { index: lo, dir: 1 } : undefined;
      }
      // The next `count` frames to show after the playhead, failed ones
      // skipped: in the loop window while playing or in it, else onward.
      function upcoming(count) {
        const out = [];
        if (head < 0) return out;
        const inside = playing || (head >= win.lo && head <= win.hi);
        let at = { index: head, dir: direction };
        for (let guard = 0; out.length < count && guard <= axis.length; guard++) {
          at = following(at.index, at.dir, inside);
          if (!at || at.index === head) break;
          if (!marks.has(axis[at.index]) && !out.includes(at.index)) out.push(at.index);
        }
        return out;
      }
      function updatePlay() {
        if (!playButton) return;
        playButton.classList.toggle('playing', playing);
        playButton.setAttribute('aria-label', playing ? 'Pause' : 'Play');
        playButton.classList.toggle('buffering', playing && holding);
        bufferChip();
      }
      // While playback waits for a frame, the chip counts the loop's frames
      // in, "Loading 6/24"; the live region says nothing per frame.
      function bufferChip() {
        if (playing && holding && axis.length) {
          let ready = 0;
          for (let i = win.lo; i <= win.hi; i++) if (readyAt(i)) ready++;
          showChip(`Loading ${ready}/${win.hi - win.lo + 1}`, 'loading');
          chipBuffering = true;
        } else if (chipBuffering) showChip('');
      }
      function play() {
        if (playing || axis.length < 2) return;
        playing = true; engaged = true; playedOnce = true; direction = 1;
        updatePlay(); drawWindowText();
        report(`Playing ${windowSummary()}, ${speed()} frame${speed() === 1 ? '' : 's'} a second.`);
        // From the loop's end or outside it, play starts at the loop's start.
        if (head < win.lo || head >= win.hi) goTo(win.lo, true);
        else if (!pendingFront) shownNext();
        prefetch();
      }
      // Stop playing. A pause is announced; `quiet` leaves that to what
      // follows, such as a step.
      function pause(text, quiet) {
        if (!playing) return;
        playing = false; holding = false;
        clearTimeout(playTimer); playTimer = undefined;
        updatePlay(); syncSlider(); saveFragment();
        if (!quiet) report(text || `Paused at ${shown?.drawn ? frameText(shown) : timeText(axis[head])}.`);
      }
      const halt = () => pause(undefined, true);
      function tick() {
        playTimer = undefined;
        if (!playing) return;
        let at = { index: head, dir: direction };
        for (let guard = 0; guard <= win.hi - win.lo + 1; guard++) {
          at = following(at.index, at.dir, true);
          if (!at || !marks.has(axis[at.index])) break;
        }
        if (!at || marks.has(axis[at.index])) {
          pause(at ? 'No frame of the loop could be drawn; paused.' : `Played once to ${timeText(axis[head])}; paused.`);
          return;
        }
        direction = at.dir;
        goTo(at.index, true);
      }
      // A frame has been shown: while playing, the next follows after the
      // frame interval, or after 1.2 s on the last frame of the loop.
      function shownNext() {
        if (!playing) return;
        holding = false; updatePlay();
        clearTimeout(playTimer);
        const last = head === win.hi && repeat() !== 'once';
        playTimer = setTimeout(tick, last ? Math.max(LAST_HOLD, interval()) : interval());
      }
      // Show frame `index`. During playback (`played`) nothing is announced
      // and the slider stays; a viewer's move is announced when drawn.
      function goTo(index, played, dragging) {
        if (!axis.length) return;
        head = Math.max(0, Math.min(axis.length - 1, index));
        state.datetime = instant(axis[head]);
        if (!played) {
          engaged = true;
          if (!dragging) { syncSlider(); if (seekLabel) seekLabel.hidden = true; }
          saveFragment();
        }
        render();
      }
      function step(delta) { if (!axis.length) return; halt(); goTo(head + delta); }
      function byDay(sign) {
        if (!axis.length) return;
        halt();
        let index = nearestIndex(axis[head] + sign * DAY);
        if (index === head) index += sign;
        goTo(index);
      }
      function speedBy(delta) {
        if (!speedSelect) return;
        const index = Math.max(0, Math.min(SPEEDS.length - 1, SPEEDS.indexOf(speed()) + delta));
        speedSelect.value = String(SPEEDS[index]); speedChosen = true;
        report(`${SPEEDS[index]} frame${SPEEDS[index] === 1 ? '' : 's'} a second.`);
      }
      function toggleHelp(open) {
        if (!keysHelp) return;
        const show = open ?? keysHelp.hidden;
        keysHelp.hidden = !show;
        keysButton?.setAttribute('aria-expanded', String(show));
      }
      const readyAt = (index, view = state.mode === 'tiles' ? undefined : imageView()) => {
        const key = frameKey({ ...state, datetime: instant(axis[index]) }, view);
        return front?.key === key || pool.get(key)?.state === 'ready';
      };
      playButton?.addEventListener('click',() => { if (playing) pause(); else play(); });
      byId('map-prev')?.addEventListener('click',event => step(event.shiftKey ? -10 : -1));
      byId('map-next')?.addEventListener('click',event => step(event.shiftKey ? 10 : 1));
      byId('map-first')?.addEventListener('click',event => { halt(); goTo(event.shiftKey ? 0 : win.lo); });
      byId('map-last')?.addEventListener('click',event => { halt(); goTo(event.shiftKey ? axis.length - 1 : win.hi); });
      speedSelect?.addEventListener('change',() => { speedChosen = true; });
      repeatInputs.forEach(input => input.addEventListener('change',() => { direction = 1; trimPool(); prefetch(); }));
      keysButton?.addEventListener('click',() => toggleHelp());
      const closeMenu = () => { if (menu) menu.open = false; };
      byId('map-window-start')?.addEventListener('click',() => { setWindowEnd('lo', head); closeMenu(); });
      byId('map-window-end')?.addEventListener('click',() => { setWindowEnd('hi', head); closeMenu(); });
      byId('map-window-reset')?.addEventListener('click',() => {
        if (!axis.length) return;
        userWindow = false; defaultWindow(); drawWindow(); trimPool(); prefetch(); saveFragment(); closeMenu();
        report(`Loop: ${windowSummary()}.`);
      });
      // Jump to a date and time, read as UTC: the frame nearest it.
      jump?.addEventListener('change',() => {
        const ms = Date.parse(`${jump.value}Z`);
        if (!Number.isFinite(ms) || !axis.length) return;
        halt(); goTo(nearestIndex(ms)); closeMenu();
      });
      // Dragging shows buffered frames at once; any other is requested once
      // the pointer rests, or on release, never once per pixel.
      let dragTimer;
      const sliderIndex = () => nearestIndex(axis[0] + Number(slider.value) * 1000);
      slider?.addEventListener('input',() => {
        if (!axis.length) return;
        const index = sliderIndex();
        halt(); engaged = true; clearTimeout(dragTimer);
        slider.setAttribute('aria-valuetext', timeText(axis[index]));
        if (index === head) { seekLabel.hidden = true; return; }
        seekLabel.textContent = `→ ${timeText(axis[index])}`; seekLabel.hidden = false;
        if (readyAt(index)) goTo(index, false, true);
        else dragTimer = setTimeout(() => goTo(index, false, true), DRAG_REST);
      });
      slider?.addEventListener('change',() => { if (!axis.length) return; clearTimeout(dragTimer); goTo(sliderIndex()); });
      // The loop window's handles.
      [['lo', byId('map-window-lo')], ['hi', byId('map-window-hi')]].forEach(([end, handle]) => {
        handle?.addEventListener('pointerdown',event => {
          if (!axis.length) return;
          event.preventDefault();
          handle.setPointerCapture?.(event.pointerId);
          const move = e => {
            const rect = track.getBoundingClientRect();
            if (!rect.width) return;
            const share = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
            setWindowEnd(end, nearestIndex(axis[0] + share * (axis[axis.length - 1] - axis[0])), true);
          };
          const up = () => {
            ['pointermove','pointerup','pointercancel'].forEach((type, i) => handle.removeEventListener(type, i ? up : move));
            setWindowEnd(end, end === 'lo' ? win.lo : win.hi);
          };
          handle.addEventListener('pointermove',move);
          handle.addEventListener('pointerup',up);
          handle.addEventListener('pointercancel',up);
        });
      });
      // Keyboard shortcuts, only while focus is in the map or its time bar
      // (WCAG 2.1.4). Arrow keys on the map keep MapLibre's pan; on the
      // slider they step frames.
      function onKey(event) {
        if (!axis.length || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
        const target = event.target, tag = target?.tagName;
        // A field keeps its own keys: text, a select, and the basemap's
        // radios and opacity slider. Only the playhead takes the map's.
        if (tag === 'SELECT' || tag === 'TEXTAREA' || (tag === 'INPUT' && target !== slider)) return;
        const key = event.key, count = event.shiftKey ? 10 : 1;
        const onSlider = target === slider;
        if (key === ' ' || key === 'k' || key === 'K') {
          // Space on a button or link is its own click.
          if (key === ' ' && ['BUTTON','SUMMARY','A'].includes(tag)) return;
          if (playing) pause(); else play();
        } else if (key === '<') speedBy(-1);
        else if (key === '>') speedBy(1);
        else if (key === ',' || event.code === 'Comma') step(-count);
        else if (key === '.' || event.code === 'Period') step(count);
        else if (onSlider && (key === 'ArrowLeft' || key === 'ArrowDown')) step(-count);
        else if (onSlider && (key === 'ArrowRight' || key === 'ArrowUp')) step(count);
        else if (key === 'PageUp') byDay(-1);
        else if (key === 'PageDown') byDay(1);
        else if (key === 'Home') { halt(); goTo(0); }
        else if (key === 'End') { halt(); goTo(axis.length - 1); }
        else if (key === '[') setWindowEnd('lo', head);
        else if (key === ']') setWindowEnd('hi', head);
        else if (key === 'n' || key === 'N') { halt(); goTo(nearestIndex(Date.now())); }
        else if (key === '?') toggleHelp();
        else if (key === 'Escape' && keysHelp && !keysHelp.hidden) toggleHelp(false);
        else return;
        event.preventDefault();
      }
      element.addEventListener('keydown',onKey);
      bar?.addEventListener('keydown',onKey);

      // The fragment follows the view and the choices, replaced rather than
      // pushed so history stays clean, and at most about once a second:
      // Safari allows 100 replaceState calls per 30 s. It is not written per
      // played frame, only when playback stops.
      let fragmentTimer, lastFragment = 0;
      function fragmentText() {
        const center = map.getCenter();
        const fixed = (n, digits) => String(Number(n.toFixed(digits)));
        const parts = [`map=${fixed(map.getZoom(),2)}/${fixed(center.lat,4)}/${fixed(wrapLongitude(center.lng),4)}`];
        const add = (key, value) => {
          if (value) parts.push(`${key}=${encodeURIComponent(value).replace(/%3A/gi,':').replace(/%2F/gi,'/')}`);
        };
        add('parameter-name', state['parameter-name']);
        if (state.style > 0) add('style', style.options[state.style]?.dataset.id);
        add('datetime', state.datetime);
        add('elevation', state.elevation);
        add('mode', state.mode);
        // The loop window the viewer set, as an interval of its two ends.
        if (userWindow && axis.length) add('window', `${instant(axis[win.lo])}/${instant(axis[win.hi])}`);
        return parts.join('&');
      }
      // A write already due takes the state as it is then.
      function saveFragment() {
        if (fragmentTimer) return;
        fragmentTimer = setTimeout(() => {
          fragmentTimer = undefined;
          // Leave a tab's fragment, or an anchor, alone.
          if (!['', '#', '#overview'].includes(location.hash) && !readFragment()) return;
          lastFragment = Date.now();
          history.replaceState(history.state, '', `#${fragmentText()}`);
        }, Math.max(250, lastFragment + 1000 - Date.now()));
      }
      const link = byId('map-image-link');
      const requestText = byId('map-image-request');
      const legendBox = byId('map-legend');
      const legendTitle = byId('map-legend-title');
      const legendScale = byId('map-legend-scale');
      const legendStatus = byId('map-legend-status');
      const legendImage = byId('map-legend-image');
      const legendJson = byId('map-legend-link');
      let legendGeneration = 0, activeLegend;
      function legendMessage(text) {
        legendScale.hidden = true; legendImage.hidden = true; legendJson.hidden = true;
        legendStatus.hidden = !text; legendStatus.textContent = text;
      }
      // The scale's labels: min and max, and rounded values about `count`
      // apart between them, none close enough to an end to crowd its label.
      // Values come from an integer index, never a running sum, so a step
      // below the values' own precision cannot stall the loop; at most 50.
      function ticks(min, max, count) {
        const span = max - min;
        if (!(span > 0) || !Number.isFinite(span)) return [min];
        const raw = span / Math.max(1,count), power = 10 ** Math.floor(Math.log10(raw)), f = raw / power;
        const step = (f < 1.5 ? 1 : f < 3 ? 2 : f < 7 ? 5 : 10) * power;
        const gap = Math.max(step / 2, raw * 0.7);
        const first = Math.ceil(min / step - 1e-9), last = Math.floor(max / step + 1e-9);
        const out = [min];
        for (let k = 0; k <= Math.min(last - first, 50); k++) {
          const value = Number(((first + k) * step).toPrecision(10));
          if (value - min >= gap && max - value >= gap && value > out[out.length - 1]) out.push(value);
        }
        out.push(max);
        return out;
      }
      const formatValue = value => String(Number(Number(value).toPrecision(6)));
      // The legend document as a colour bar in the page's own type and
      // theme: the palette stops over the style's range, linear or stepped,
      // titled with the parameter's label and unit, at the data's opacity.
      function drawLegend(legend, styleTitle) {
        if (Array.isArray(legend.channels)) {
          // An RGB composite: one line per channel, no colour bar.
          const list = document.createElement('ul'); list.className = 'map-legend-channels';
          legend.channels.forEach(channel => {
            const item = document.createElement('li');
            const swatch = document.createElement('span'); swatch.className = `channel-swatch ${channel.channel}`;
            const text = document.createElement('span');
            text.textContent = `${channel.label} · ${formatValue(channel.min)} to ${formatValue(channel.max)}${channel.unit ? ` ${channel.unit}` : ''}`;
            item.append(swatch,text); list.append(item);
          });
          legendScale.replaceChildren(list);
        } else {
          const stops = (legend.stops || []).filter(stop => Number.isFinite(stop.value) && /^#[0-9a-f]{6}([0-9a-f]{2})?$/i.test(stop.color));
          if (!stops.length) throw new Error('Legend has no colour stops');
          const min = Number.isFinite(legend.min) ? legend.min : stops[0].value;
          const max = Number.isFinite(legend.max) ? legend.max : stops[stops.length - 1].value;
          const at = value => max > min ? Math.min(100,Math.max(0,(value - min) / (max - min) * 100)) : 0;
          const colourBar = document.createElement('div'); colourBar.className = 'map-legend-bar';
          colourBar.style.background = legend.interpolation === 'step'
            ? `linear-gradient(to right, ${stops.map((stop,i) => `${stop.color} ${at(stop.value)}% ${at(stops[i + 1]?.value ?? max)}%`).join(', ')})`
            : `linear-gradient(to right, ${stops.map(stop => `${stop.color} ${at(stop.value)}%`).join(', ')})`;
          colourBar.style.opacity = String(dataOpacity);
          legendBar = colourBar;
          const scale = document.createElement('div'); scale.className = 'map-legend-ticks';
          const width = legendBox.clientWidth || 300;
          ticks(min, max, Math.max(2, Math.floor(width / 90))).forEach(value => {
            const tick = document.createElement('span'); tick.textContent = formatValue(value);
            const position = at(value); tick.style.left = `${position}%`;
            if (position < 4) tick.classList.add('start'); else if (position > 96) tick.classList.add('end');
            scale.append(tick);
          });
          legendScale.replaceChildren(colourBar,scale);
        }
        const title = legend.parameter ? parameterTitle(legend.parameter, legend.unit) : (legend.title || styleTitle);
        legendTitle.textContent = styleTitle && styleTitle !== 'Default' ? `${title} · ${styleTitle}` : title;
        legendScale.hidden = false;
      }
      function nameDefaultParameter(name) {
        const known = findParameter(name);
        defaultParameter = known?.name || name;
        if (parameter) parameter.options[0].textContent = `Default · ${known?.label || name}`;
        syncLayer();
        if (shown && statusShowsFrame) showing(shown);
        // The default parameter on its own axis: the time bar follows it.
        if (!state['parameter-name']) {
          const before = state.datetime;
          rebind();
          if (state.datetime !== before) render();
        }
      }
      // A style's legend document for a parameter, fetched once per page:
      // revalidated on first use, since an operator can change a style in
      // place, then kept.
      const legends = new Map();
      function legendDocument(href, choice) {
        const url = new URL(href, location.href);
        if (choice) url.searchParams.set('parameter-name',choice); else url.searchParams.delete('parameter-name');
        url.searchParams.set('f','json');
        if (!legends.has(url.href)) {
          legends.set(url.href, (async () => {
            const response = await fetch(url,{cache:'no-cache',headers:{Accept:'application/json'},signal:AbortSignal.timeout(15000)});
            if (!response.ok) throw new Error('Legend unavailable');
            return { url: url.href, legend: await response.json() };
          })().catch(error => { legends.delete(url.href); throw error; }));
        }
        return legends.get(url.href);
      }
      // The legend of what a request draws, fetched beside the map, not
      // after it: it also names the parameter the server draws by default.
      // The last legend stays, dimmed, until the next one is drawn. The PNG
      // and the JSON stay links.
      async function showLegend(want) {
        if (!legendBox) return;
        const option = style.options[want.style];
        const href = option?.dataset.legend, styleTitle = option?.textContent;
        const choice = want['parameter-name'];
        const key = `${href} ${choice}`;
        if (activeLegend === key) return;
        activeLegend = key;
        const current = ++legendGeneration;
        if (!href || !sameOrigin(href)) {
          legendBox.classList.remove('stale'); legendTitle.textContent = 'Legend';
          legendMessage('This style advertises no legend.');
          return;
        }
        legendBox.classList.add('stale'); legendBox.setAttribute('aria-busy','true');
        try {
          const { url, legend } = await legendDocument(href, choice);
          if (current !== legendGeneration) return;
          drawLegend(legend, styleTitle);
          legendJson.href = url;
          const image = new URL(url); image.searchParams.set('f','png'); legendImage.href = image.href;
          legendImage.hidden = false; legendJson.hidden = false; legendStatus.hidden = true;
          if (!choice && legend.parameter) nameDefaultParameter(legend.parameter);
        } catch (_) {
          if (current === legendGeneration) { activeLegend = undefined; legendTitle.textContent = 'Legend'; legendMessage('Legend unavailable.'); }
        } finally {
          if (current === legendGeneration) { legendBox.classList.remove('stale'); legendBox.removeAttribute('aria-busy'); }
        }
      }
      function noLegend(text) {
        if (!legendBox) return;
        activeLegend = undefined; ++legendGeneration;
        legendBox.classList.remove('stale'); legendBox.removeAttribute('aria-busy');
        legendTitle.textContent = 'Legend'; legendMessage(text);
      }
      // What a frame shows, as the status line names it: the instant, the
      // parameter, and any style or level chosen.
      function frameText(frame) {
        const { want, drawn } = frame;
        const parts = [drawn ? `${formatTime(drawn)} UTC` : time || axis.length ? 'the default time' : 'the map data'];
        const name = want['parameter-name'] || defaultParameter;
        if (name) parts.push(parameterTitle(name));
        if (want.style > 0) parts.push(style.options[want.style]?.textContent);
        if (want.elevation) parts.push([...(level?.options || [])].find(option => option.value === want.elevation)?.textContent || want.elevation);
        return parts.filter(Boolean).join(' · ');
      }
      // The status line, a live region: loading, errors, steps, pauses. It
      // is left alone for each played frame. While a request is out or
      // after it failed, it also names the frame still on the map; a
      // failure marks it stale.
      let shown, statusShowsFrame = false;
      function report(text, kind) {
        const still = kind && shown;
        status.textContent = still ? `${text} Still showing ${frameText(shown)}.` : text;
        status.classList.toggle('stale', Boolean(still && kind === 'stale'));
        statusShowsFrame = false;
        showChip(text, kind);
      }
      function disableImageLink() {
        link.removeAttribute('href'); link.setAttribute('aria-disabled','true');
      }
      // A frame has been swapped in: its request is the one linked, the time
      // bar names its time, and the status names what it shows unless it
      // was played.
      function showing(frame) {
        shown = frame;
        link.href = frame.href; link.removeAttribute('aria-disabled');
        showRequest(frame.text);
        labelFrame(frame);
        if (playing) return;
        report(`${frame.kind === 'tiles' ? 'Map tiles · ' : ''}Showing ${frameText(frame)}${frame.note ? `. ${frame.note}` : ''}`);
        statusShowsFrame = true;
        // The time bar and the pill name a frame drawn; the chip only says
        // what went wrong with it.
        showChip(frame.note ? `${frame.note}.` : '', frame.note ? 'stale' : undefined);
      }
      // The frame's request, under the map: the text to read and copy.
      const copyButton = byId('map-request-copy');
      function showRequest(text) {
        requestText.textContent = text;
        if (!copyButton) return;
        copyButton.dataset.copy = text;
        copyButton.disabled = !text;
      }
      // A request that drew nothing new. The frame on the map stays; the
      // failed request is listed only while there is none.
      function failed(error, requested) {
        if (!shown && requested) { showRequest(requested); disableImageLink(); }
        const text = error.noData ? 'No data available for this collection right now.'
          : error.name === 'AbortError' || error.name === 'TimeoutError' ? 'Map request timed out. Pan, zoom or change a choice to try again.'
          : error.message || 'Map data unavailable.';
        report(text, 'stale');
      }
      function sameOrigin(href) {
        // Tile URL templates carry literal `{z}`-style placeholders.
        try {
          const url = new URL(href.replace(/[{}]/g,''), location.href);
          return url.origin === location.origin && ['http:','https:'].includes(url.protocol);
        } catch (_) { return false; }
      }
      // A request for nothing: the server's own choices are a collection
      // without data; the viewer's are a time or layer without any.
      function noDataError(want, description) {
        const error = new Error();
        error.nothing = true;
        const choseLayer = Boolean(want['parameter-name'] || want.style > 0 || want.elevation);
        if (!want.datetime && !choseLayer) error.noData = true;
        else if (choseLayer && description) error.message = `Nothing to draw for this choice: ${description}`;
        else if (want.datetime) error.message = `No data at ${formatTime(new Date(want.datetime))} UTC. Choose another time.`;
        else error.message = 'No data for this choice right now. Choose another parameter, style or level, or Default.';
        return error;
      }
      const BUSY = 'The server is busy. Pan, zoom or change a choice to try again.';
      const busyError = after => Object.assign(new Error(BUSY), { retry: true, after: after || 0 });
      // The server's words as a sentence of the status line.
      const sentence = text => /[.!?]$/.test(text) ? text : `${text}.`;
      // A failed map request as a sentence. A 404 for the viewer's own
      // choice says why in the server's words: a style the parameter does
      // not have is no lack of data. A 503 is retried.
      async function requestError(response, want) {
        let description;
        try { description = (await response.json())?.description; } catch (_) {}
        if (response.status === 204 || [404,500,502].includes(response.status)) {
          return noDataError(want, response.status === 404 && typeof description === 'string' ? description : undefined);
        }
        if ([503,504].includes(response.status)) {
          const after = response.headers.get('retry-after');
          return busyError(/^\d+$/.test(after || '') ? Number(after) : Math.max(0, (Date.parse(after || '') - Date.now()) / 1000) || 0);
        }
        if (response.status === 400 && typeof description === 'string') return new Error(`The server rejected this request: ${sentence(description)}`);
        return new Error(`Map request failed (HTTP ${response.status}).`);
      }
      // A WMS GetMap that drew nothing: the ServiceException its XML names
      // (WMS 1.3.0 answers every request error with one), or the error
      // tile (`x-cache: ERROR`), never shown as a frame.
      async function wmsError(response, want) {
        if ([503,504].includes(response.status)) return requestError(response, want);
        if (/xml/i.test(response.headers.get('content-type') || '')) {
          let text = '';
          try { text = await response.text(); } catch (_) {}
          const found = /<ServiceException\b([^>]*)>([\s\S]*?)<\/ServiceException>/.exec(text);
          const message = (found?.[2] || '').trim()
            .replace(/&(lt|gt|quot|apos|amp);/g, (_, name) => ({ lt: '<', gt: '>', quot: '"', apos: "'", amp: '&' })[name]);
          const error = new Error(message ? `The server rejected this request: ${sentence(message)}` : `WMS request failed (HTTP ${response.status}).`);
          error.code = /\bcode="([^"]*)"/.exec(found?.[1] || '')?.[1];
          return error;
        }
        if (response.ok && response.headers.get('x-cache') === 'ERROR') return new Error('The server could not draw this map.');
        return new Error(`WMS request failed (HTTP ${response.status}).`);
      }
      // Busy: try again after `Retry-After`, or 1, 2 and 4 s, at most three
      // times; the frame stays meanwhile. `refill` draws the same frame
      // again to fill the tiles a busy server left out.
      const RETRIES = 3;
      let retryTimer;
      function retryLater(attempt, after, refill) {
        const seconds = Math.min(30, Math.max(after || 0, 2 ** attempt));
        if (!playing) report(`Server busy, retrying in ${seconds} s…`, 'loading');
        retryTimer = setTimeout(() => render(attempt + 1, refill), seconds * 1000);
      }

      // The data layer is a set of frames. A frame is one combination of
      // draw mode, style, parameter, level and time (and for an image, the
      // view), staged as its own raster source and layer under the borders
      // by `stageFrame`, the only place that adds one. `front` is drawn;
      // the pool holds the frames buffered ahead, hidden at opacity 0. The
      // visible source is never removed or retargeted.
      const NO_TRANSITION = { duration: 0, delay: 0 };
      const MAX_IMAGE_FETCHES = 2, MAX_DECODED_AHEAD = 3;
      const pool = new Map();
      let front, pendingFront, frameCount = 0, backoffUntil = 0, backoffTimer, busyStreak = 0, onScreen = true;
      const layerKey = want => [want.mode, want.style, want['parameter-name'], want.elevation].join('|');
      const frameKey = (want, view) => `${layerKey(want)}|${want.datetime}|${view?.key || ''}`;
      function removeFrame(frame) {
        frame.dropped = true;
        frame.controller?.abort();
        if (frame.onRender) map.off('render',frame.onRender);
        if (frame.id && map.getLayer(frame.id)) map.removeLayer(frame.id);
        if (frame.id && map.getSource(frame.id)) map.removeSource(frame.id);
      }
      // Drop a frame that is not drawn: buffered, loading or failed.
      function dropFrame(frame) {
        if (!frame || frame === front) return;
        if (pool.get(frame.key) === frame) pool.delete(frame.key);
        if (pendingFront === frame) pendingFront = undefined;
        removeFrame(frame);
      }
      function clearFrames() {
        resetFrames();
        const old = front; front = undefined; shown = undefined;
        if (old) removeFrame(old);
      }
      // Another layer: abort everything in flight and drop every buffered
      // frame. The drawn one stays until the next is in.
      function resetFrames() {
        clearTimeout(backoffTimer); backoffUntil = 0; busyStreak = 0;
        [...pool.values()].forEach(dropFrame);
        if (pendingFront) dropFrame(pendingFront);
        pendingFront = undefined;
        marks.clear();
        drawBuffer();
      }
      // Stage `source` invisibly for `frame`. Once MapLibre reports every
      // tile in view loaded or failed, `frame.settle` judges it, in that
      // `render` callback.
      function stageFrame(frame, source) {
        frame.id = `map-frame-${++frameCount}`;
        map.addSource(frame.id, source);
        map.addLayer({ id: frame.id, type: 'raster', source: frame.id,
          paint: { 'raster-opacity': 0, 'raster-opacity-transition': NO_TRANSITION, 'raster-fade-duration': 0 } },
        map.getLayer('border-halo') ? 'border-halo' : 'outlines');
        frame.onRender = () => {
          if (frame.dropped || !map.isSourceLoaded(frame.id)) return;
          map.off('render',frame.onRender); frame.onRender = undefined;
          frame.settle(frame);
        };
        map.on('render',frame.onRender);
        map.triggerRepaint();
      }
      // Show a loaded frame in one paint: it goes to the data opacity and
      // the old to 0, both without a transition (MapLibre's default 300 ms
      // fade would show the basemap through both). The old frame stays,
      // hidden, while it is one of the frames ahead; else it is removed
      // after that paint.
      function swapTo(frame) {
        const old = front;
        if (pool.get(frame.key) === frame) pool.delete(frame.key);
        front = frame;
        map.setPaintProperty(frame.id,'raster-opacity',dataOpacity);
        if (!old || old === frame) return;
        map.setPaintProperty(old.id,'raster-opacity',0);
        if (old.key !== frame.key && !pool.has(old.key) && wantedKeys().has(old.key)) pool.set(old.key, old);
        else map.once('render',() => { if (front !== old && pool.get(old.key) !== old) removeFrame(old); });
      }
      // The frames worth keeping: the next ones to show, as many as may be
      // buffered ahead.
      function aheadLimit() {
        if (saveData) return 1;
        if (state.mode !== 'tiles') return MAX_DECODED_AHEAD;
        // K = clamp(⌊160 / tiles in view⌋, 2, 6).
        const tiles = (Math.ceil(element.clientWidth / 256) + 1) * (Math.ceil(element.clientHeight / 256) + 1);
        return Math.max(2, Math.min(6, Math.floor(160 / Math.max(1, tiles))));
      }
      function wantedKeys() {
        const keys = new Set();
        if (!engaged || !axis.length) return keys;
        const view = state.mode === 'tiles' ? undefined : imageView();
        upcoming(aheadLimit()).forEach(index => keys.add(frameKey({ ...state, datetime: instant(axis[index]) }, view)));
        return keys;
      }
      function trimPool() {
        const keep = wantedKeys();
        [...pool.values()].forEach(frame => { if (frame !== pendingFront && !keep.has(frame.key)) dropFrame(frame); });
        drawBuffer();
      }
      // Stop buffering: drop every frame still loading ahead.
      function stopPrefetch() {
        clearTimeout(backoffTimer);
        [...pool.values()].forEach(frame => { if (frame.state === 'loading' && frame !== pendingFront) dropFrame(frame); });
      }
      // Buffer ahead, after the drawn frame is in and only for a viewer
      // using the time bar, never while the tab is hidden or the map is off
      // screen. Map tiles stage one frame at a time, up to K; images fetch
      // at most 2 at a time and keep up to 3 ahead. A busy server backs off.
      function prefetch() {
        if (!engaged || axis.length < 2 || state.mode === 'vector' || !front || pendingFront || !onScreen || document.visibilityState === 'hidden') return;
        const wait = backoffUntil - Date.now();
        if (wait > 0) { clearTimeout(backoffTimer); backoffTimer = setTimeout(prefetch, wait); return; }
        const tiles = state.mode === 'tiles';
        const view = tiles ? undefined : imageView();
        if (!tiles && !view) return;
        trimPool();
        const loading = [...pool.values()].filter(frame => frame.state === 'loading');
        let free = tiles ? 1 - loading.length : MAX_IMAGE_FETCHES - loading.filter(frame => frame.fetching).length;
        for (const index of upcoming(aheadLimit())) {
          if (free <= 0) break;
          const want = { ...state, datetime: instant(axis[index]) };
          const key = frameKey(want, view);
          if (pool.has(key) || front.key === key) continue;
          load(newFrame(want, view));
          free--;
        }
        drawBuffer();
      }
      function newFrame(want, view, attempt = 0) {
        const frame = { key: frameKey(want, view), want, view, ms: Date.parse(want.datetime), state: 'loading', loaded: 0, errors: [], attempt,
          kind: want.mode === 'tiles' ? 'tiles' : 'image' };
        pool.set(frame.key, frame);
        return frame;
      }
      function load(frame) {
        if (frame.kind === 'tiles') loadTiles(frame); else loadImage(frame);
      }
      // A frame is in. The one awaited is shown at once; a buffered one
      // waits, and buffering goes on.
      function frameReady(frame) {
        frame.state = 'ready';
        busyStreak = 0;
        if (marks.delete(frame.ms)) drawBuffer();
        if (pendingFront === frame) promote(frame);
        else { drawBuffer(); prefetch(); }
      }
      // A frame drew nothing. One awaited keeps the frame on the map and
      // says why, or while playing is skipped; a buffered one is marked.
      // A busy server is retried for the frame awaited, and stops
      // buffering for a while, unmarked.
      function frameFailed(frame, error) {
        const awaited = pendingFront === frame;
        dropFrame(frame);
        if (error.retry || (!awaited && (error.name === 'AbortError' || error.name === 'TimeoutError'))) {
          if (!awaited) {
            // 2, 4, 8 … s, at most 30, or longer when the server says so;
            // fetches refused together count once.
            if (Date.now() >= backoffUntil) busyStreak++;
            backoffUntil = Math.max(backoffUntil, Date.now() + Math.min(30, Math.max(error.after || 0, 2 ** busyStreak)) * 1000);
            prefetch();
            return;
          }
          if (frame.attempt < RETRIES) { retryLater(frame.attempt, error.after); return; }
          if (playing) { pause(`${BUSY} Paused.`); return; }
          failed(error, frame.text);
          return;
        }
        if (Number.isFinite(frame.ms) && error.name !== 'AbortError' && error.name !== 'TimeoutError') {
          marks.set(frame.ms, error.nothing ? 'nodata' : 'error');
        }
        drawBuffer();
        if (!awaited) { prefetch(); return; }
        if (playing) {
          report(`${error.nothing ? 'No data' : 'Failed'} at ${formatTime(new Date(frame.ms))} UTC, skipped.`);
          tick();
          return;
        }
        failed(error, frame.text);
      }
      // The awaited frame is in: swap it in with its label, then go on.
      function promote(frame) {
        if (pendingFront !== frame || frame.dropped) return;
        pendingFront = undefined;
        swapTo(frame);
        showing(frame);
        // Fill the holes a busy server left.
        if (frame.busy && frame.attempt < RETRIES) retryLater(frame.attempt, 0, true);
        shownNext();
        trimPool();
        prefetch();
      }
      // Await `frame`, buffered or not; it is swapped in once loaded. Map
      // tiles buffered before a pan load the new view first.
      function awaitFrame(frame) {
        pendingFront = frame;
        if (frame.state !== 'ready') return;
        if (map.isSourceLoaded(frame.id)) { promote(frame); return; }
        const check = () => {
          if (pendingFront !== frame || frame.dropped) { map.off('render',check); return; }
          if (map.isSourceLoaded(frame.id)) { map.off('render',check); promote(frame); }
        };
        map.on('render',check); map.triggerRepaint();
      }
      // Tiles report per frame: a loaded tile counts, a failed one keeps its
      // HTTP status. A 404 tile is no event at all in MapLibre: no tile
      // loaded and none failed means nothing to draw.
      const frameById = id => front?.id === id ? front : [...pool.values()].find(frame => frame.id === id);
      map.on('sourcedata', event => {
        const frame = event.tile && frameById(event.sourceId);
        if (frame) frame.loaded++;
      });
      map.on('error', event => {
        if (event.sourceId === 'map-vector') {
          report('Some tiles failed to load; the rest of the view is shown.');
          return;
        }
        const frame = frameById(event.sourceId);
        if (!frame) return;
        frame.errors.push(event.error?.status);
        if (frame === front && !playing) report('Some tiles failed to load; the rest of the view is shown.');
      });
      // The Web Mercator tileset of one kind from an advertised tileset list
      // (a per-API list may hold map and vector tilesets side by side).
      const tilesets = new Map();
      function webMercatorTileset(listHref, dataType) {
        const key = `${dataType} ${listHref}`;
        if (!tilesets.has(key)) {
          tilesets.set(key, (async () => {
            if (!sameOrigin(listHref)) throw new Error('Tileset list must use this server.');
            const response = await fetch(listHref,{headers:{Accept:'application/json'},signal:AbortSignal.timeout(15000)});
            if (!response.ok) throw new Error(`Tileset list unavailable (HTTP ${response.status}).`);
            const list = await response.json();
            const tileset = (list.tilesets || []).find(t =>
              (!t.dataType || t.dataType === dataType) && /\/WebMercatorQuad$/.test(t.tileMatrixSetURI || ''));
            const item = tileset?.links?.find(l => l.rel === 'item');
            if (!item || !sameOrigin(item.href)) throw new Error('This collection advertises no Web Mercator tileset.');
            const zooms = (tileset.tileMatrixSetLimits || []).map(l => Number(l.tileMatrix)).filter(Number.isFinite);
            // Every URL this page opens is validated like the tile template.
            const self = tileset.links.find(l => l.rel === 'self')?.href;
            return {
              template: item.href.replace('{tileMatrix}','{z}').replace('{tileRow}','{y}').replace('{tileCol}','{x}'),
              minzoom: zooms.length ? Math.min(...zooms) : 0,
              maxzoom: zooms.length ? Math.max(...zooms) : 22,
              self: self && sameOrigin(self) ? self : listHref
            };
          })().catch(error => { tilesets.delete(key); throw error; }));
        }
        return tilesets.get(key);
      }
      function applyMode() {
        const mode = state.mode;
        const vector = mode === 'vector';
        if (vector) halt();
        [parameterField, levelField].forEach(field => { if (field) field.hidden = vector; });
        // The time row and bar stay even when a frame has no data: another
        // time may have some.
        if (timeField) timeField.hidden = vector || axis.length > 0;
        if (bar) bar.hidden = vector || !axis.length;
        // A vector-only collection has nothing to choose: no controls at all.
        block.hidden = vector && modes.length === 1;
        let usable = 0;
        for (const option of style.options) {
          // WMS serves every style by name; Maps and map tiles where linked.
          option.disabled = mode === 'maps' ? !option.value : mode === 'tiles' ? !option.dataset.tiles : false;
          if (!option.disabled) usable++;
        }
        if (style.selectedOptions[0]?.disabled) {
          const first = [...style.options].findIndex(option => !option.disabled);
          if (first >= 0) style.selectedIndex = first;
        }
        state.style = Math.max(0, style.selectedIndex);
        // One style is no choice.
        styleField.hidden = vector || usable < 2;
        link.textContent = mode === 'maps' || mode === 'wms' ? 'Open ↗' : 'Open tileset ↗';
        if (ladder) ladder.hidden = vector;
        measureDock();
        if (help) help.textContent = vector ? 'Vector tiles load as you pan and zoom: feature geometry in one outline color.' : generalHelp;
      }
      // The request a render makes, fixed when it starts.
      const snapshot = () => ({ ...state });
      const pixelRatio = () => typeof devicePixelRatio === 'number' && devicePixelRatio > 0 ? devicePixelRatio : 1;
      let timer;
      function render(attempt = 0, refill = false) {
        clearTimeout(timer); clearTimeout(retryTimer);
        if (state.mode === 'vector') return renderVector();
        clearVector();
        return requestFrame(attempt, refill);
      }
      // Draw what the state names: at once when that frame is buffered,
      // else once it has loaded, the current frame holding meanwhile.
      function requestFrame(attempt, refill) {
        const want = snapshot();
        showLegend(want);
        let view;
        if (want.mode !== 'tiles') {
          if (!element.clientWidth || !element.clientHeight) return;
          view = imageView();
          if (!view) { stopPrefetch(); if (pendingFront) dropFrame(pendingFront); failed(new Error('Pan back to the collection to view data.')); return; }
        }
        const key = frameKey(want, view);
        if (!refill && front?.key === key) {
          if (pendingFront) dropFrame(pendingFront);
          pendingFront = undefined;
          if (!playing) showing(front);
          shownNext(); prefetch();
          return;
        }
        let frame = refill ? undefined : pool.get(key);
        if (frame && pendingFront === frame) return;
        // A frame not yet asked for goes first: nothing loading for
        // prefetch may queue ahead of its tiles or take its fetch slot.
        if (!frame) stopPrefetch();
        // A frame awaited before is given up; a buffered one stays.
        if (pendingFront && pendingFront !== frame) {
          const old = pendingFront;
          pendingFront = undefined;
          if (old.state !== 'ready') dropFrame(old);
        }
        if (frame?.state === 'ready') { awaitFrame(frame); return; }
        if (!frame) { frame = newFrame(want, view, attempt); pendingFront = frame; load(frame); }
        else { frame.attempt = attempt; pendingFront = frame; }
        if (playing) { holding = true; updatePlay(); }
        else report(want.mode === 'tiles' ? 'Loading map tiles…' : 'Loading the map image…', 'loading');
      }
      // Map tiles at the chosen time, parameter, style and level. HiDPI
      // screens get 512-pixel tiles over the same 256-pixel tile area
      // (Tiles `width`/`height`).
      function tileTemplate(tileset, want) {
        const params = new URLSearchParams();
        if (want['parameter-name']) params.set('parameter-name',want['parameter-name']);
        if (want.datetime) params.set('datetime',want.datetime);
        if (want.elevation) params.set('elevation',want.elevation);
        if (pixelRatio() >= 1.5) { params.set('width','512'); params.set('height','512'); }
        const query = params.toString();
        return query ? `${tileset.template}${tileset.template.includes('?') ? '&' : '?'}${query}` : tileset.template;
      }
      // Whether the view reaches the tileset at all: outside it no tile is
      // requested, which is no lack of data.
      function viewReaches(bounds) {
        if (!bounds) return true;
        const view = map.getBounds();
        if (view.getSouth() > bounds[3] || view.getNorth() < bounds[1]) return false;
        const west = view.getWest(), width = view.getEast() - west;
        if (width >= 360) return true;
        const start = wrapLongitude(west);
        return [-360, 0, 360].some(shift => start + shift <= bounds[2] && start + shift + width >= bounds[0]);
      }
      async function loadTiles(frame) {
        const want = frame.want;
        try {
          const tileset = await webMercatorTileset(style.options[want.style].dataset.tiles, 'map');
          if (frame.dropped) return;
          const template = tileTemplate(tileset, want);
          frame.href = tileset.self; frame.text = template;
          frame.drawn = want.datetime ? new Date(want.datetime) : undefined;
          frame.settle = judgeTiles;
          const source = { type: 'raster', tiles: [template], minzoom: tileset.minzoom, maxzoom: tileset.maxzoom, tileSize: 256 };
          if (tileBounds) source.bounds = tileBounds;
          stageFrame(frame, source);
        } catch (error) {
          if (!frame.dropped) frameFailed(frame, error);
        }
      }
      function judgeTiles(frame) {
        const busy = frame.errors.some(code => code === 503 || code === 504);
        if (!frame.loaded && (frame.errors.length || viewReaches(tileBounds))) {
          frameFailed(frame, busy ? busyError() : frame.errors.length ? new Error('Map tiles failed to load.') : noDataError(frame.want));
          return;
        }
        frame.busy = busy;
        frame.note = frame.errors.length ? 'Some tiles failed to load; the rest of the view is shown' : '';
        frameReady(frame);
      }
      // The visible area as a Maps request and where its image goes. A view
      // across the antimeridian is requested in the Maps form, west greater
      // than east, and placed at the view's own unwrapped longitudes. One
      // image covers at most one world. Latitude is not clamped: map bounds
      // and the server's Web Mercator rendering must agree.
      function viewBox() {
        const bounds = map.getBounds();
        const west = bounds.getWest(), east = bounds.getEast();
        const south = bounds.getSouth(), north = bounds.getNorth();
        if (!(west < east && south < north)) return undefined;
        if (east - west >= 360) {
          const shift = Math.round((west + east) / 720) * 360;
          return { bbox: [-180,south,180,north], west: -180 + shift, east: 180 + shift, south, north, share: 360 / (east - west) };
        }
        // West into [-180, 180); east follows it, less than a world on.
        const shift = Math.floor((west + 180) / 360) * 360;
        const w = west - shift, e = east - shift;
        return { bbox: [w,south,e > 180 ? e - 360 : e,north], west, east, south, north, share: 1 };
      }
      // The image in device pixels, so it is sharp on HiDPI screens, within
      // about 4 Mpx: under the Maps and WMS caps (8000 a side, 64 Mpx) and a
      // small share of the server's render memory budget.
      const MAX_IMAGE_PIXELS = 4 * 1024 * 1024, MAX_IMAGE_SIDE = 8000;
      function imageSize(share) {
        const ratio = pixelRatio();
        const width = Math.max(1, element.clientWidth * share * ratio), height = Math.max(1, element.clientHeight * ratio);
        const scale = Math.min(1, Math.sqrt(MAX_IMAGE_PIXELS / (width * height)), MAX_IMAGE_SIDE / width, MAX_IMAGE_SIDE / height);
        return [Math.max(1, Math.floor(width * scale)), Math.max(1, Math.floor(height * scale))];
      }
      // The view an image is drawn for, with its size; its key tells images
      // of one view from another's.
      function imageView() {
        const box = viewBox();
        if (!box || !element.clientWidth || !element.clientHeight) return undefined;
        const [width, height] = imageSize(box.share);
        return { ...box, width, height, key: [box.west, box.south, box.east, box.north, width, height].join(',') };
      }
      function mapsUrl(want, view) {
        const option = style.options[want.style];
        if (!option?.value || !sameOrigin(option.value)) throw new Error('Map endpoint must use this server.');
        const url = new URL(option.value, location.href);
        url.searchParams.set('bbox',view.bbox.join(','));
        url.searchParams.set('bbox-crs','CRS:84'); url.searchParams.set('crs','EPSG:3857');
        url.searchParams.set('width',view.width); url.searchParams.set('height',view.height);
        url.searchParams.set('f','image/png'); url.searchParams.set('transparent','true');
        ['parameter-name','datetime','elevation'].forEach(key => { if (want[key]) url.searchParams.set(key,want[key]); else url.searchParams.delete(key); });
        return url;
      }
      // EPSG:3857 metres from MapLibre's own Web Mercator. Nothing is
      // clamped: x keeps the view's unwrapped longitude, as a WMS client
      // sends a view across the antimeridian.
      const WEB_MERCATOR_HALF_WORLD = 20037508.342789244;
      function webMercator(lng, lat) {
        const point = maplibregl.MercatorCoordinate.fromLngLat([lng, lat]);
        return [(point.x - 0.5) * 2 * WEB_MERCATOR_HALF_WORLD, (0.5 - point.y) * 2 * WEB_MERCATOR_HALF_WORLD];
      }
      // A WMS 1.3.0 GetMap of the view: in EPSG:3857, so the BBOX is
      // minx,miny,maxx,maxy in metres with no axis-order trap; the
      // parameter's own layer, `coll/<parameter>`, or the collection's;
      // errors as XML, never an image.
      function wmsUrl(want, view) {
        const wms = request.wms;
        if (!wms?.href || !sameOrigin(wms.href)) throw new Error('WMS endpoint must use this server.');
        const url = new URL(wms.href, location.href);
        const west = view.share === 1 ? view.bbox[0] : -180;
        const east = west + (view.share === 1 ? view.east - view.west : 360);
        const [minx, miny] = webMercator(west, view.south), [maxx, maxy] = webMercator(east, view.north);
        const params = {
          SERVICE: 'WMS', VERSION: '1.3.0', REQUEST: 'GetMap',
          LAYERS: want['parameter-name'] ? `${wms.layer}/${want['parameter-name']}` : wms.layer,
          STYLES: want.style > 0 ? style.options[want.style]?.dataset.id || '' : '',
          CRS: 'EPSG:3857', BBOX: [minx, miny, maxx, maxy].map(n => Number(n.toFixed(2))).join(','),
          WIDTH: view.width, HEIGHT: view.height,
          FORMAT: 'image/png', TRANSPARENT: 'TRUE', EXCEPTIONS: 'XML'
        };
        if (want.datetime) params.TIME = want.datetime;
        if (want.elevation) params.ELEVATION = want.elevation;
        Object.entries(params).forEach(([key, value]) => url.searchParams.set(key, value));
        return url;
      }
      // An image frame, Maps or WMS: fetched as a Blob, decoded and staged
      // as a canvas source where its view lies.
      async function loadImage(frame) {
        const want = frame.want, view = frame.view, wms = want.mode === 'wms';
        const controller = frame.controller = new AbortController();
        const timeout = setTimeout(() => controller.abort(), 15000);
        frame.fetching = true;
        try {
          const url = wms ? wmsUrl(want, view) : mapsUrl(want, view);
          frame.href = frame.text = url.href;
          if (pendingFront === frame && !shown) showRequest(url.href);
          const response = await fetch(url,{signal:controller.signal});
          const type = response.headers.get('content-type') || '';
          if (wms) {
            if (!response.ok || !type.startsWith('image/') || response.headers.get('x-cache') === 'ERROR') throw await wmsError(response, want);
          } else {
            if (!response.ok || response.status === 204) throw await requestError(response, want);
            if (!type.startsWith('image/')) throw new Error('Map endpoint did not return an image.');
          }
          // The instant drawn: Maps names it, which the request may have
          // left to the server to snap; WMS does not, so it is the one asked.
          const header = wms ? null : response.headers.get('content-datetime');
          frame.drawn = header && !Number.isNaN(Date.parse(header)) ? new Date(header)
            : want.datetime ? new Date(want.datetime) : undefined;
          const bitmap = await createImageBitmap(await response.blob());
          if (frame.dropped) { bitmap.close(); return; }
          frame.fetching = false;
          const canvas = document.createElement('canvas');
          canvas.width = bitmap.width; canvas.height = bitmap.height;
          canvas.getContext('2d').drawImage(bitmap,0,0); bitmap.close();
          frame.settle = frameReady;
          stageFrame(frame, { type: 'canvas', canvas, animate: false,
            coordinates: [[view.west,view.north],[view.east,view.north],[view.east,view.south],[view.west,view.south]] });
        } catch (error) {
          if (!frame.dropped) frameFailed(frame, error);
        } finally {
          frame.fetching = false;
          clearTimeout(timeout);
          if (!frame.dropped && frame.state === 'loading' && pendingFront !== frame) prefetch();
        }
      }
      const VECTOR_LAYERS = ['map-vector-fill','map-vector-line','map-vector-points'];
      function clearVector() {
        VECTOR_LAYERS.forEach(id => { if (map.getLayer(id)) map.removeLayer(id); });
        if (map.getSource('map-vector')) map.removeSource('map-vector');
      }
      // Vector tiles take no time, parameter, style or level, and have no
      // frames to swap: the layers follow the view by themselves.
      let vectorGeneration = 0;
      async function renderVector() {
        const current = ++vectorGeneration;
        clearFrames(); clearVector();
        noLegend('Vector tiles draw feature geometry in one color; there is no legend.');
        try {
          report('Loading tileset…');
          const tileset = await webMercatorTileset(request.vector.href, 'vector');
          if (current !== vectorGeneration || state.mode !== 'vector') return;
          const colors = palette(), layer = request.vector.layer;
          const source = { type: 'vector', tiles: [tileset.template], minzoom: tileset.minzoom, maxzoom: tileset.maxzoom };
          if (tileBounds) source.bounds = tileBounds;
          const before = map.getLayer('border-halo') ? 'border-halo' : 'outlines';
          map.addSource('map-vector',source);
          const kind = types => ['match',['geometry-type'],types,true,false];
          map.addLayer({id:'map-vector-fill',type:'fill',source:'map-vector','source-layer':layer,filter:kind(['Polygon','MultiPolygon']),paint:{'fill-color':colors.ink,'fill-opacity':0.2}},before);
          map.addLayer({id:'map-vector-line',type:'line',source:'map-vector','source-layer':layer,filter:['!',kind(['Point','MultiPoint'])],paint:{'line-color':colors.ink,'line-width':1.5}},before);
          map.addLayer({id:'map-vector-points',type:'circle',source:'map-vector','source-layer':layer,filter:kind(['Point','MultiPoint']),paint:{'circle-color':colors.ink,'circle-radius':4,'circle-stroke-color':colors.land,'circle-stroke-width':1}},before);
          link.href = tileset.self; link.removeAttribute('aria-disabled');
          showRequest(tileset.template);
          report('Vector tiles · loading as you pan and zoom');
        } catch (error) {
          if (current !== vectorGeneration || state.mode !== 'vector') return;
          clearVector(); disableImageLink();
          report(error.message || 'Tiles unavailable.');
        }
      }
      // A pan or zoom: tiles follow the view by themselves, buffered ones
      // too; a map image is requested again, 300 ms after the view
      // settles, with the choices already applied. Images buffered for the
      // old view are dropped, and playback pauses. A resize that leaves the
      // view as it was changes nothing.
      const viewKey = () => { const b = map.getBounds(); return [b.getWest(), b.getSouth(), b.getEast(), b.getNorth(), element.clientWidth, element.clientHeight].join(','); };
      let lastView = viewKey();
      function viewChanged() {
        const key = viewKey();
        if (key === lastView) return;
        lastView = key;
        saveFragment();
        if (state.mode !== 'maps' && state.mode !== 'wms') return;
        if (playing) pause('Paused: the view changed.');
        // Invalidate at once so an old response cannot replace a new view.
        clearTimeout(retryTimer); clearTimeout(timer);
        [...pool.values()].forEach(dropFrame);
        pendingFront = undefined;
        timer = setTimeout(render,300);
      }
      map.on('moveend',viewChanged);
      new ResizeObserver(() => { if (element.clientWidth && element.clientHeight) map.resize(); }).observe(element);
      // A hidden tab pauses playback and buffering; a map scrolled out of
      // view does too.
      document.addEventListener('visibilitychange',() => {
        if (document.visibilityState === 'hidden') { pause(); stopPrefetch(); } else prefetch();
      });
      if (typeof IntersectionObserver === 'function') {
        new IntersectionObserver(entries => {
          onScreen = entries.some(entry => entry.isIntersecting);
          if (onScreen) prefetch(); else { pause(); stopPrefetch(); }
        }).observe(element);
      }
      const savedTime = Date.parse(saved?.datetime || '');
      const savedWindow = (saved?.window || '').split('/').map(Date.parse);
      bindAxis(axisOf(state['parameter-name']), Number.isFinite(savedTime) ? savedTime : undefined,
        savedWindow.length === 2 && savedWindow.every(Number.isFinite) ? savedWindow : undefined);
      if (!axis.length) state.datetime = selectedTimeValue();
      applyMode();
      placePanels();
      syncLayer();
      updatePlay();
      if (typeof ResizeObserver === 'function') {
        const docks = new ResizeObserver(measureDock);
        [bar, legendBox].forEach(node => { if (node) docks.observe(node); });
      }
      render();
    }
    map.on('style.load', async () => {
      const color = palette().ink;
      map.addSource('features', { type: 'geojson', data });
      map.addLayer({ id: 'areas', type: 'fill', source: 'features', filter: ['==', ['geometry-type'], 'Polygon'], paint: { 'fill-color': color, 'fill-opacity': data.mapRequest ? 0 : 0.18 } });
      map.addLayer({ id: 'outlines', type: 'line', source: 'features', filter: ['!=', ['geometry-type'], 'Point'], paint: { 'line-color': color, 'line-width': 2 } });
      map.addLayer({ id: 'points', type: 'circle', source: 'features', filter: ['==', ['geometry-type'], 'Point'], paint: { 'circle-color': color, 'circle-radius': 7, 'circle-stroke-color': palette().land, 'circle-stroke-width': 2 } });
      const bounds = new maplibregl.LngLatBounds();
      function extend(coords) {
        if (!Array.isArray(coords)) return;
        if (typeof coords[0] === 'number' && typeof coords[1] === 'number') {
          if (Number.isFinite(coords[0]) && Number.isFinite(coords[1])) bounds.extend(coords);
        } else coords.forEach(extend);
      }
      function geometry(g) { if (!g) return; if (g.geometries) g.geometries.forEach(geometry); else extend(g.coordinates); }
      data.features.forEach(feature => geometry(feature.geometry));
      // A fragment's view wins over the extent: a reload or a shared link
      // reopens where the viewer was.
      if (saved?.view) map.jumpTo(saved.view);
      else if (!bounds.isEmpty()) {
        // A collection's map frames the extent clear of its panels: the
        // layer card on the left and the time bar along the bottom of a
        // wide map. Padding never takes more than a third of a side.
        const width = element.clientWidth || 0, height = element.clientHeight || 0;
        const wide = width > 900 && Boolean(data.mapRequest);
        const padding = wide
          ? { top: 60, right: 80, bottom: document.getElementById('map-timebar') ? 150 : 60, left: 350 }
          : { top: 45, right: 45, bottom: 45, left: 45 };
        Object.keys(padding).forEach(side => { padding[side] = Math.min(padding[side], Math.floor((side === 'top' || side === 'bottom' ? height : width) / 3)); });
        map.fitBounds(bounds, { padding, maxZoom: data.mapRequest ? 8 : 5, duration: 0 });
      }
      recordView();
      status.textContent = element.dataset.quicklook === 'true' ? 'Current page · select a shape for a quick look.' : 'Advertised geometry · general-purpose locator.';
      if (data.mapRequest) installRasterPreview(bounds);
      map.on('click', event => {
        if (element.dataset.quicklook !== 'true') return;
        const { x, y } = event.point;
        const hits = map.queryRenderedFeatures([[x-7,y-7],[x+7,y+7]], { layers: ['areas','outlines','points'] });
        const unique = new Map(hits.map(feature => [feature.properties.href, feature]));
        selection.replaceChildren();
        if (!unique.size) return;
        const heading = document.createElement('p'); heading.textContent = `Choose from ${unique.size} nearby feature${unique.size === 1 ? '' : 's'}`; selection.append(heading);
        unique.forEach(hit => {
          const feature = data.features.find(f => f.properties.href === hit.properties.href);
          if (!feature) return;
          const button = document.createElement('button'); button.type = 'button'; button.className = 'btn small'; button.textContent = feature.properties.label || String(feature.id);
          button.addEventListener('click',() => inspect(feature)); selection.append(button);
          if (unique.size === 1) inspect(feature);
        });
      });
      // A theme change re-reads the tokens and repaints every layer that
      // takes one.
      document.addEventListener('workbench-theme', () => {
        const colors = palette();
        const paint = (id, property, value) => { if (map.getLayer(id)) map.setPaintProperty(id, property, value); };
        paint('background','background-color',colors.water);
        paint('land','fill-color',colors.land);
        paint('border-halo','line-color',colors.halo);
        paint('borders','line-color',colors.border);
        paint('areas','fill-color',colors.ink); paint('outlines','line-color',colors.ink);
        paint('points','circle-color',colors.ink); paint('points','circle-stroke-color',colors.land);
        paint('map-vector-fill','fill-color',colors.ink); paint('map-vector-line','line-color',colors.ink);
        paint('map-vector-points','circle-color',colors.ink); paint('map-vector-points','circle-stroke-color',colors.land);
      });
      try {
        const response = await fetch(element.dataset.land);
        if (!response.ok) throw new Error('Backdrop unavailable');
        const land = await response.json();
        map.addSource('land',{type:'geojson',data:land,attribution:'Natural Earth · public domain'});
        const colors = palette();
        map.addLayer({id:'land',type:'fill',source:'land',paint:{'fill-color':colors.land}},'areas');
        // Keep borders above every replacement raster, with a halo so
        // boundaries remain visible over both dark and bright weather
        // colors; the halo token carries its own transparency.
        map.addLayer({id:'border-halo',type:'line',source:'land',paint:{'line-color':colors.halo,'line-width':3}},'outlines');
        map.addLayer({id:'borders',type:'line',source:'land',paint:{'line-color':colors.border,'line-width':1}},'outlines');
        applyBasemap();
        // Country names, largest country first. Markers have no collision
        // detection (a symbol layer would need a glyph server), so a name
        // whose box overlaps one already placed, or the map's edge, is
        // skipped.
        // Size is the land's approximate area: each polygon's outer ring by
        // the shoelace formula, degrees² scaled by the cosine of its mean
        // latitude. A bounding box would not do: a country split at the
        // antimeridian (Fiji, Russia) spans the whole world.
        const landArea = g => {
          const polygons = g?.type === 'Polygon' ? [g.coordinates] : g?.type === 'MultiPolygon' ? g.coordinates : [];
          return polygons.reduce((sum, polygon) => {
            const ring = polygon[0] || [];
            let twice = 0, lat = 0;
            ring.forEach(([x1,y1],i) => { const [x2,y2] = ring[(i + 1) % ring.length]; twice += x1 * y2 - x2 * y1; lat += y1; });
            return sum + Math.abs(twice) / 2 * Math.cos(lat / Math.max(1,ring.length) * Math.PI / 180);
          }, 0);
        };
        const named = land.features.filter(f => f.properties.x != null && f.properties.y != null && f.properties.name)
          .map(f => ({ name: f.properties.name, x: f.properties.x, y: f.properties.y, size: landArea(f.geometry) }))
          .sort((a,b) => b.size - a.size);
        const probe = document.createElement('span'); probe.className = 'country-label'; element.append(probe);
        const font = getComputedStyle(probe); const measure = document.createElement('canvas').getContext('2d');
        measure.font = `${font.fontWeight} ${font.fontSize} ${font.fontFamily}`; probe.remove();
        const labels=[];
        function labelCountries() {
          labels.forEach(marker=>marker.remove()); labels.length=0;
          if (map.getZoom()>7) return;
          const view = map.getBounds(), west = view.getWest(), placed = [];
          for (const country of named) {
            if (labels.length >= 24) break;
            // The copy of the label point inside the view, if any.
            let lng = country.x;
            while (lng < west) lng += 360;
            while (lng >= west + 360) lng -= 360;
            if (lng > view.getEast() || country.y < view.getSouth() || country.y > view.getNorth()) continue;
            const point = map.project([lng,country.y]), half = measure.measureText(country.name).width / 2 + 4;
            const box = [point.x - half, point.y - 12, point.x + half, point.y + 12];
            // Whole names only: none cut by the map's edge.
            if (box[0] < 0 || box[1] < 0 || box[2] > element.clientWidth || box[3] > element.clientHeight) continue;
            if (placed.some(b => box[0] < b[2] && b[0] < box[2] && box[1] < b[3] && b[1] < box[3])) continue;
            placed.push(box);
            const label=document.createElement('span');label.className='country-label';label.textContent=country.name;
            const marker = new maplibregl.Marker({element:label}).setLngLat([lng,country.y]).addTo(map);
            label.setAttribute('role','presentation'); label.setAttribute('aria-hidden','true'); label.removeAttribute('tabindex'); labels.push(marker);
          }
        }
        labelCountries();map.on('moveend',labelCountries);
      } catch (_) { if (!data.mapRequest) status.textContent += ' Geographic backdrop unavailable.'; }
    });
    map.on('error', event => {
      // Tile and image sources report their own failures in the status line.
      if (/^map-/.test(event.sourceId || '')) return;
      if (!started) unavailable();
    });
  } catch (_) { unavailable(); }
})();
