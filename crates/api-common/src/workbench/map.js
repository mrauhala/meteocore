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
  // Without MapLibre or WebGL nothing on the panel works: hide the map with
  // every control, and say where this page lists the same facts.
  function unavailable() {
    ['map-controls','map-request','map-legend'].forEach(id => { const node = document.getElementById(id); if (node) node.hidden = true; });
    element.hidden = true;
    status.textContent = `This browser cannot draw the map. ${element.dataset.fallback || ''}`.trim();
  }
  if (typeof maplibregl === 'undefined') { unavailable(); return; }
  let started = false;
  try {
    const data = JSON.parse(document.getElementById('map-data').textContent);
    const palette = () => {
      const css = getComputedStyle(document.documentElement);
      const dark = document.documentElement.dataset.theme === 'dark';
      return { water: dark ? '#192e23' : '#e4eee8', land: dark ? '#293e32' : '#f0f5ef', border: '#819a85', ink: css.getPropertyValue('--teal').trim() };
    };
    const map = new maplibregl.Map({
      container: element,
      style: { version: 8, sources: {}, layers: [{ id: 'background', type: 'background', paint: { 'background-color': palette().water } }] },
      center: [10, 30], zoom: 1
    });
    map.on('load',() => { started = true; });
    // The view as a CRS84 box, west,south,east,north, for an items page's
    // "Use map view" (#1143). Longitudes wrap into ±180: a view across the
    // antimeridian has west > east, as Features `bbox` takes it.
    function recordView() {
      const view = map.getBounds();
      const round = n => Number(n.toFixed(4));
      const wrap = lon => ((lon + 180) % 360 + 360) % 360 - 180;
      const whole = view.getEast() - view.getWest() >= 360;
      const west = whole ? -180 : wrap(view.getWest());
      const east = whole ? 180 : wrap(view.getEast());
      const lat = n => Math.max(-90, Math.min(90, n));
      element.dataset.view = [west, lat(view.getSouth()), east === -180 ? 180 : east, lat(view.getNorth())].map(round).join(',');
    }
    map.on('moveend',recordView);
    map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
    map.addControl(new maplibregl.ScaleControl({ unit: 'metric' }));
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
    // Draw the collection with Maps (one image per view), its map tiles, or
    // its vector tiles, whichever it advertises; a switch picks among several.
    function installRasterPreview(extentBounds) {
      const request = data.mapRequest;
      const modes = request.modes || ['maps'];
      const modeInputs = [...document.querySelectorAll('input[name="map-mode"]')];
      const currentMode = () => modeInputs.find(input => input.checked)?.value || modes[0];
      const form = document.getElementById('map-controls');
      const parameter = document.getElementById('map-parameter');
      const style = document.getElementById('map-style');
      const time = document.getElementById('map-time');
      const level = document.getElementById('map-level');
      const previous = document.getElementById('map-time-prev');
      const next = document.getElementById('map-time-next');
      const help = document.getElementById('map-help');
      const submit = form.querySelector('button.primary');
      const mapsHelp = help?.textContent;
      const parameterField = parameter?.closest('label');
      const styleField = style.closest('label');
      const timeField = time?.closest('.map-time-field');
      const levelField = level?.closest('label');
      const timeList = time?.tagName === 'SELECT';
      const parameters = request.parameter_names || [];
      // A collection crossing the antimeridian comes as one unwrapped
      // polygon (east > 180). Tile sources clamp their bounds to ±180, so
      // such a tileset is given none.
      const tileBounds = extentBounds && !extentBounds.isEmpty() && extentBounds.getWest() >= -180 && extentBounds.getEast() <= 180
        ? extentBounds.toArray().flat() : undefined;
      // What the server drew when the choice was left to it: the instant
      // from the map's Content-Datetime, the parameter from the legend.
      let drawnDefault, defaultParameter, showing, noDataShown = false, nearNow = false;
      // UTC to the minute, seconds only when present, as the options read.
      function formatTime(date) {
        const iso = date.toISOString();
        return `${iso.slice(0,10)} ${iso.slice(11, iso.slice(17,23) === '00.000' ? 16 : 19)}`;
      }
      function findParameter(name) {
        return parameters.find(p => p.name === name) || parameters.find(p => p.label === name);
      }
      function parameterTitle(name, unit) {
        const known = findParameter(name);
        const symbol = unit || known?.unit;
        return symbol ? `${known?.label || name} (${symbol})` : (known?.label || name);
      }
      // The option nearest an instant (milliseconds), skipping Default.
      function nearestOption(ms) {
        let best = -1, distance = Infinity;
        for (let i = 1; i < time.options.length; i++) {
          const d = Math.abs(Date.parse(time.options[i].value) - ms);
          if (d < distance) { best = i; distance = d; }
        }
        return best;
      }
      // Default (index 0) stands for the instant the server drew for it, or
      // the latest time before a map has named one. An instant past either
      // end of the listed times (a page left open while new frames arrive)
      // stands one place beyond that end, so one step lands on the end.
      function timeIndex() {
        if (time.selectedIndex > 0) return time.selectedIndex;
        const last = time.options.length - 1;
        if (!drawnDefault || last < 1) return last;
        const ms = drawnDefault.getTime();
        if (ms > Date.parse(time.options[last].value)) return last + 1;
        if (ms < Date.parse(time.options[1].value)) return 0;
        return nearestOption(ms);
      }
      function updateTimeButtons() {
        if (!timeList || !previous || !next) return;
        const index = timeIndex();
        previous.disabled = index <= 1;
        next.disabled = index >= time.options.length - 1;
      }
      function stepTime(direction) {
        const index = timeIndex() + direction;
        if (index < 1 || index >= time.options.length) return;
        time.selectedIndex = index; nearNow = false;
        updateTimeButtons(); clearTimeout(timer); render();
      }
      previous?.addEventListener('click',() => stepTime(-1));
      next?.addEventListener('click',() => stepTime(1));
      time?.addEventListener('change',() => { nearNow = false; updateTimeButtons(); });
      // A forecast whose axis runs past now opens at the valid time nearest
      // the viewer's clock, not at the server's default (its last step).
      // The collection advertises no default time of its own. The margin
      // keeps a radar whose newest frame is a little ahead of a slow clock
      // on Default, which follows new frames.
      if (timeList && time.options.length > 1) {
        const now = Date.now();
        if (Date.parse(time.options[time.options.length - 1].value) > now + 10 * 60 * 1000) {
          time.selectedIndex = nearestOption(now); nearNow = true;
        }
      }
      updateTimeButtons();
      const link = document.getElementById('map-image-link');
      const requestText = document.getElementById('map-image-request');
      const legendBox = document.getElementById('map-legend');
      const legendTitle = document.getElementById('map-legend-title');
      const legendScale = document.getElementById('map-legend-scale');
      const legendStatus = document.getElementById('map-legend-status');
      const legendImage = document.getElementById('map-legend-image');
      const legendJson = document.getElementById('map-legend-link');
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
      // titled with the parameter's label and unit.
      function drawLegend(legend, styleTitle) {
        legendScale.replaceChildren();
        const title = legend.parameter ? parameterTitle(legend.parameter, legend.unit) : (legend.title || styleTitle);
        legendTitle.textContent = styleTitle && styleTitle !== 'Default' ? `${title} · ${styleTitle}` : title;
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
          legendScale.append(list); legendScale.hidden = false;
          return;
        }
        const stops = (legend.stops || []).filter(stop => Number.isFinite(stop.value) && /^#[0-9a-f]{6}([0-9a-f]{2})?$/i.test(stop.color));
        if (!stops.length) throw new Error('Legend has no colour stops');
        const min = Number.isFinite(legend.min) ? legend.min : stops[0].value;
        const max = Number.isFinite(legend.max) ? legend.max : stops[stops.length - 1].value;
        const at = value => max > min ? Math.min(100,Math.max(0,(value - min) / (max - min) * 100)) : 0;
        const bar = document.createElement('div'); bar.className = 'map-legend-bar';
        bar.style.background = legend.interpolation === 'step'
          ? `linear-gradient(to right, ${stops.map((stop,i) => `${stop.color} ${at(stop.value)}% ${at(stops[i + 1]?.value ?? max)}%`).join(', ')})`
          : `linear-gradient(to right, ${stops.map(stop => `${stop.color} ${at(stop.value)}%`).join(', ')})`;
        const scale = document.createElement('div'); scale.className = 'map-legend-ticks';
        const width = legendBox.clientWidth || 300;
        ticks(min, max, Math.max(2, Math.floor(width / 90))).forEach(value => {
          const tick = document.createElement('span'); tick.textContent = formatValue(value);
          const position = at(value); tick.style.left = `${position}%`;
          if (position < 4) tick.classList.add('start'); else if (position > 96) tick.classList.add('end');
          scale.append(tick);
        });
        legendScale.append(bar,scale); legendScale.hidden = false;
      }
      function nameDefaultParameter(name) {
        const known = findParameter(name);
        defaultParameter = known?.name || name;
        if (parameter) parameter.options[0].textContent = `Default · ${known?.label || name}`;
        if (showing !== undefined) status.textContent = describe(showing);
      }
      // The selected style's legend for the selected parameter, fetched as
      // JSON beside the map, not after it: it also names the parameter the
      // server draws by default. The PNG and the JSON stay links.
      async function showLegend(href, styleTitle) {
        if (!legendBox) return;
        const choice = parameter?.value || '';
        const key = `${href} ${choice}`;
        if (activeLegend === key) return;
        activeLegend = key;
        const current = ++legendGeneration;
        legendTitle.textContent = 'Legend';
        if (!href || !sameOrigin(href)) { legendMessage('This style advertises no legend.'); return; }
        legendMessage('Loading legend…');
        try {
          const url = new URL(href, location.href);
          if (choice) url.searchParams.set('parameter-name',choice); else url.searchParams.delete('parameter-name');
          url.searchParams.set('f','json');
          // Legends are cacheable, but an operator can change a style in place.
          // Revalidate on selection so a cached old palette cannot label new pixels.
          const response = await fetch(url,{cache:'no-cache',headers:{Accept:'application/json'},signal:AbortSignal.timeout(15000)});
          if (!response.ok) throw new Error('Legend unavailable');
          const legend = await response.json();
          if (current !== legendGeneration) return;
          drawLegend(legend, styleTitle);
          legendJson.href = url.href;
          url.searchParams.set('f','png'); legendImage.href = url.href;
          legendImage.hidden = false; legendJson.hidden = false; legendStatus.hidden = true;
          if (!choice && legend.parameter) nameDefaultParameter(legend.parameter);
        } catch (_) {
          if (current === legendGeneration) { activeLegend = undefined; legendTitle.textContent = 'Legend'; legendMessage('Legend unavailable.'); }
        }
      }
      function noLegend(text) {
        if (!legendBox) return;
        activeLegend = undefined; ++legendGeneration;
        legendTitle.textContent = 'Legend'; legendMessage(text);
      }
      // The status line once a map is drawn: the instant, the parameter, and
      // any style or level chosen.
      function describe(drawn, prefix) {
        const parts = prefix ? [prefix] : [];
        const nearest = nearNow && time?.selectedIndex > 0 ? ', the time nearest now' : '';
        parts.push(drawn ? `Showing ${formatTime(drawn)} UTC${nearest}` : time ? 'Default time' : 'Showing the map data');
        const name = parameter?.value || defaultParameter;
        if (name) parts.push(parameterTitle(name));
        if (style.selectedIndex > 0) parts.push(style.selectedOptions[0].textContent);
        if (level?.value) parts.push(level.selectedOptions[0].textContent);
        return parts.join(' · ');
      }
      // A collection without data: say so, and offer no time to choose.
      function noData() {
        noDataShown = true;
        if (timeField) timeField.hidden = true;
        status.textContent = 'No data available for this collection right now.';
      }
      function dataShown() {
        if (!noDataShown) return;
        noDataShown = false;
        if (timeField) timeField.hidden = currentMode() === 'vector';
      }
      // Whether the viewer chose anything but the time: a parameter, a
      // named style or a level.
      function choseLayer() {
        return Boolean(parameter?.value || style.selectedIndex > 0 || level?.value);
      }
      // A failed map request as a sentence. A 404 or 500 for the server's
      // own choices means the collection has nothing to draw. A 404 for the
      // viewer's own says why in the server's words: a style the parameter
      // does not have is no lack of data.
      async function requestError(response, chosenTime) {
        let description;
        try { description = (await response.json())?.description; } catch (_) {}
        const error = new Error();
        if ([404,500,502].includes(response.status)) {
          if (!chosenTime && !choseLayer()) {
            error.noData = true;
          } else if (response.status === 404 && choseLayer() && typeof description === 'string' && description) {
            error.message = `Nothing to draw for this choice: ${description}`;
          } else {
            error.message = chosenTime ? 'No data at the selected time. Choose another time or Default.'
              : 'No data available for this choice right now.';
          }
        } else if ([503,504].includes(response.status)) {
          error.message = 'The server is busy. Use Update map to retry.';
        } else if (response.status === 400 && typeof description === 'string') {
          error.message = `The server rejected this request: ${description}`;
        } else {
          error.message = `Map request failed (HTTP ${response.status}). Use Update map to retry.`;
        }
        return error;
      }
      let pending, generation = 0, timer, activeId, staged;
      function discardStaged() {
        if (!staged) return;
        map.off('render',staged.onRender);
        if (map.getLayer(staged.id)) map.removeLayer(staged.id);
        if (map.getSource(staged.id)) map.removeSource(staged.id);
        staged = undefined;
      }
      function clearImage() {
        discardStaged();
        if (activeId) { map.removeLayer(activeId); map.removeSource(activeId); activeId = undefined; }
      }
      const TILE_LAYERS = ['map-tiles','map-vector-fill','map-vector-line','map-vector-points'];
      function clearTiles() {
        TILE_LAYERS.forEach(id => { if (map.getLayer(id)) map.removeLayer(id); });
        ['map-tiles','map-vector'].forEach(id => { if (map.getSource(id)) map.removeSource(id); });
      }
      function disableImageLink() {
        link.removeAttribute('href'); link.setAttribute('aria-disabled','true');
      }
      function selectedTimeValue() {
        return time?.value ? (time.type === 'datetime-local' ? new Date(`${time.value}Z`).toISOString() : time.value) : '';
      }
      function sameOrigin(href) {
        // Tile URL templates carry literal `{z}`-style placeholders.
        try {
          const url = new URL(href.replace(/[{}]/g,''), location.href);
          return url.origin === location.origin && ['http:','https:'].includes(url.protocol);
        } catch (_) { return false; }
      }
      // A tile that fails to load leaves the rest of the tiled view usable.
      // A map tile's 404 is a choice with nothing to draw, worded as the
      // map's own; vector tiles take no time, parameter, style or level.
      map.on('error', event => {
        if (!['map-tiles','map-vector'].includes(event.sourceId)) return;
        let text = 'Some tiles failed to load; the rest of the view is shown.';
        if (event.error?.status === 404) {
          const vector = event.sourceId === 'map-vector';
          text = vector || (!selectedTimeValue() && !choseLayer()) ? 'No data available for this collection right now.'
            : choseLayer() ? 'No map tiles for this choice. Choose another parameter, style or level, or Default.'
            : 'No data at the selected time. Choose another time or Default.';
        }
        status.textContent = text;
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
        const mode = currentMode();
        const vector = mode === 'vector';
        [parameterField, styleField, levelField, submit].forEach(field => { if (field) field.hidden = vector; });
        if (timeField) timeField.hidden = vector || noDataShown;
        // A vector-only collection has nothing to choose: no controls at all.
        form.hidden = vector && modes.length === 1;
        for (const option of style.options) {
          option.disabled = mode === 'maps' ? !option.value : mode === 'tiles' ? !option.dataset.tiles : false;
        }
        if (style.selectedOptions[0]?.disabled) {
          const first = [...style.options].find(option => !option.disabled);
          if (first) first.selected = true;
        }
        link.textContent = mode === 'maps' ? 'Open rendered image ↗' : 'Open tileset metadata ↗';
        if (help) help.textContent = mode === 'maps' ? mapsHelp
          : mode === 'tiles' ? 'Map tiles load as you pan and zoom. Choose a parameter, style, time or level, then Update map.'
          : 'Vector tiles load as you pan and zoom: feature geometry in one outline color.';
      }
      async function renderTiles(mode) {
        const current = ++generation;
        pending?.abort(); clearTimeout(timer); clearImage(); clearTiles();
        showing = undefined;
        const vector = mode === 'vector';
        const option = style.selectedOptions[0];
        const selectedTime = selectedTimeValue();
        if (vector) noLegend('Vector tiles draw feature geometry in one color; there is no legend.');
        else showLegend(option.dataset.legend, option.textContent);
        try {
          status.textContent = 'Loading tileset…';
          const tileset = await webMercatorTileset(vector ? request.vector.href : option.dataset.tiles, vector ? 'vector' : 'map');
          if (current !== generation) return;
          const params = new URLSearchParams();
          if (!vector && parameter?.value) params.set('parameter-name',parameter.value);
          if (!vector && selectedTime) params.set('datetime',selectedTime);
          if (!vector && level?.value) params.set('elevation',level.value);
          const query = params.toString();
          const template = query ? `${tileset.template}${tileset.template.includes('?') ? '&' : '?'}${query}` : tileset.template;
          const source = { type: vector ? 'vector' : 'raster', tiles: [template], minzoom: tileset.minzoom, maxzoom: tileset.maxzoom };
          if (!vector) source.tileSize = 256;
          if (tileBounds) source.bounds = tileBounds;
          const before = map.getLayer('border-halo') ? 'border-halo' : 'outlines';
          if (vector) {
            const colors = palette(), layer = request.vector.layer;
            map.addSource('map-vector',source);
            const kind = types => ['match',['geometry-type'],types,true,false];
            map.addLayer({id:'map-vector-fill',type:'fill',source:'map-vector','source-layer':layer,filter:kind(['Polygon','MultiPolygon']),paint:{'fill-color':colors.ink,'fill-opacity':0.2}},before);
            map.addLayer({id:'map-vector-line',type:'line',source:'map-vector','source-layer':layer,filter:['!',kind(['Point','MultiPoint'])],paint:{'line-color':colors.ink,'line-width':1.5}},before);
            map.addLayer({id:'map-vector-points',type:'circle',source:'map-vector','source-layer':layer,filter:kind(['Point','MultiPoint']),paint:{'circle-color':colors.ink,'circle-radius':4,'circle-stroke-color':colors.land,'circle-stroke-width':1}},before);
          } else {
            map.addSource('map-tiles',source);
            map.addLayer({id:'map-tiles',type:'raster',source:'map-tiles',paint:{'raster-opacity':0.85,'raster-fade-duration':0}},before);
          }
          link.href = tileset.self; link.removeAttribute('aria-disabled');
          requestText.textContent = template;
          // Tiles carry no Content-Datetime: an explicit time is what they show.
          status.textContent = vector ? 'Vector tiles · loading as you pan and zoom'
            : describe(selectedTime ? new Date(selectedTime) : undefined, 'Map tiles');
        } catch (error) {
          if (current !== generation) return;
          clearTiles(); disableImageLink();
          status.textContent = error.message || 'Tiles unavailable.';
        }
      }
      function render() {
        const mode = currentMode();
        return mode === 'maps' ? renderImage() : renderTiles(mode);
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
      async function renderImage() {
        if (!element.clientWidth || !element.clientHeight) return;
        const current = ++generation;
        discardStaged(); clearTiles();
        // Nothing is named as shown until this image is: a legend arriving
        // first must not rewrite the status for the previous image.
        showing = undefined;
        pending?.abort(); pending = new AbortController();
        const controller = pending;
        const timeout = setTimeout(() => controller.abort(), 15000);
        const option = style.selectedOptions[0];
        showLegend(option.dataset.legend, option.textContent);
        const selectedTime = selectedTimeValue();
        let requestedUrl;
        try {
          const url = new URL(style.value, location.href);
          if (url.origin !== location.origin || !['http:','https:'].includes(url.protocol)) throw new Error('Map endpoint must use this server.');
          const view = viewBox();
          if (!view) throw new Error('Pan back to the collection to view data.');
          const width = Math.min(1024,Math.max(1,Math.round(element.clientWidth * view.share)));
          const height = Math.min(768,Math.max(1,Math.round(element.clientHeight)));
          url.searchParams.set('bbox',view.bbox.join(','));
          url.searchParams.set('bbox-crs','CRS:84'); url.searchParams.set('crs','EPSG:3857');
          url.searchParams.set('width',width); url.searchParams.set('height',height);
          url.searchParams.set('f','image/png'); url.searchParams.set('transparent','true');
          if (parameter?.value) url.searchParams.set('parameter-name',parameter.value); else url.searchParams.delete('parameter-name');
          if (selectedTime) url.searchParams.set('datetime',selectedTime); else url.searchParams.delete('datetime');
          if (level?.value) url.searchParams.set('elevation',level.value); else url.searchParams.delete('elevation');
          requestedUrl = url.href;
          if (!activeId) requestText.textContent = requestedUrl;
          status.textContent = activeId ? 'Updating map data… Showing the previous image until ready.' : 'Loading map data…';
          const response = await fetch(url,{signal:controller.signal});
          if (!response.ok) throw await requestError(response, selectedTime);
          if (!response.headers.get('content-type')?.startsWith('image/')) throw new Error('Map endpoint did not return an image.');
          // The instant drawn, which the request may have left to the server.
          const header = response.headers.get('content-datetime');
          const drawn = header && !Number.isNaN(Date.parse(header)) ? new Date(header) : undefined;
          const bitmap = await createImageBitmap(await response.blob());
          if (current !== generation || controller.signal.aborted) { bitmap.close(); return; }
          const canvas = document.createElement('canvas');
          canvas.width = bitmap.width; canvas.height = bitmap.height;
          canvas.getContext('2d').drawImage(bitmap,0,0); bitmap.close();
          const coordinates = [[view.west,view.north],[view.east,view.north],[view.east,view.south],[view.west,view.south]];
          // Stage the replacement invisibly. Keep the displayed image and
          // request link until MapLibre has uploaded the new source's pixels.
          const nextId = activeId === 'map-data-0' ? 'map-data-1' : 'map-data-0';
          const onRender = () => {
            if (current !== generation || !map.isSourceLoaded(nextId)) return;
            map.off('render',onRender);
            map.setPaintProperty(nextId,'raster-opacity',0.85);
            if (activeId) { map.removeLayer(activeId); map.removeSource(activeId); }
            activeId = nextId; staged = undefined;
            link.href = requestedUrl; link.removeAttribute('aria-disabled');
            requestText.textContent = requestedUrl;
            dataShown();
            if (!selectedTime && timeList) {
              drawnDefault = drawn;
              time.options[0].textContent = drawn ? `Default · ${formatTime(drawn)}` : 'Default';
              updateTimeButtons();
            }
            showing = drawn ?? null;
            status.textContent = describe(drawn);
          };
          staged = {id:nextId,onRender};
          map.addSource(nextId,{type:'canvas',canvas,coordinates,animate:false});
          map.addLayer({id:nextId,type:'raster',source:nextId,paint:{'raster-opacity':0,'raster-fade-duration':0}},map.getLayer('border-halo') ? 'border-halo' : 'outlines');
          map.on('render',onRender); map.triggerRepaint();
        } catch (error) {
          if (current !== generation) return;
          clearImage(); disableImageLink(); showing = undefined;
          if (requestedUrl) requestText.textContent = requestedUrl;
          if (error.noData) noData();
          else status.textContent = error.name === 'AbortError' || error.name === 'TimeoutError' ? 'Map request timed out. Use Update map to retry.' : (error.message || 'Map data unavailable. Use Update map to retry.');
        } finally { clearTimeout(timeout); }
      }
      function schedule() {
        // Tiles follow the view by themselves; only a map image is re-requested.
        if (currentMode() !== 'maps') return;
        // Invalidate immediately so an old response cannot replace a new view.
        ++generation; pending?.abort(); discardStaged(); clearTimeout(timer);
        timer = setTimeout(render,300);
      }
      form.addEventListener('submit',event => { event.preventDefault(); clearTimeout(timer); render(); });
      modeInputs.forEach(input => input.addEventListener('change',() => { applyMode(); clearTimeout(timer); render(); }));
      map.on('moveend',schedule);
      new ResizeObserver(() => { if (element.clientWidth && element.clientHeight) map.resize(); }).observe(element);
      applyMode();
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
      if (!bounds.isEmpty()) map.fitBounds(bounds, { padding: 45, maxZoom: data.mapRequest ? 8 : 5, duration: 0 });
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
      document.addEventListener('workbench-theme', () => {
        const colors = palette();
        map.setPaintProperty('background','background-color',colors.water);
        if (map.getLayer('land')) map.setPaintProperty('land','fill-color',colors.land);
        map.setPaintProperty('areas','fill-color',colors.ink); map.setPaintProperty('outlines','line-color',colors.ink);
        map.setPaintProperty('points','circle-color',colors.ink); map.setPaintProperty('points','circle-stroke-color',colors.land);
      });
      try {
        const response = await fetch(element.dataset.land);
        if (!response.ok) throw new Error('Backdrop unavailable');
        const land = await response.json();
        map.addSource('land',{type:'geojson',data:land,attribution:'Natural Earth · public domain'});
        map.addLayer({id:'land',type:'fill',source:'land',paint:{'fill-color':palette().land}},'areas');
        // Keep borders above every replacement raster, with a light halo so
        // boundaries remain visible over both dark and bright weather colors.
        map.addLayer({id:'border-halo',type:'line',source:'land',paint:{'line-color':'#ffffff','line-width':3,'line-opacity':0.75}},'outlines');
        map.addLayer({id:'borders',type:'line',source:'land',paint:{'line-color':'#29434b','line-width':1}},'outlines');
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
