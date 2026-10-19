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
    // The data layer's opacity over the basemap; the legend is drawn at it
    // too, so its colours match the map's.
    const DATA_OPACITY = 0.85;
    const palette = () => {
      const css = getComputedStyle(document.documentElement);
      const dark = document.documentElement.dataset.theme === 'dark';
      // Borders over the data: a line in the theme's text colour on a halo
      // of its background, so neither glares in the dark theme.
      return { water: dark ? '#192e23' : '#e4eee8', land: dark ? '#293e32' : '#f0f5ef',
        border: dark ? '#c5d3ca' : '#29434b', halo: dark ? '#101b15' : '#ffffff', ink: css.getPropertyValue('--teal').trim() };
    };
    // A collection page's map keeps its view and choices in the URL
    // fragment (#1220), under the API's own parameter names:
    // `#map=<zoom>/<lat>/<lon>&parameter-name=t&style=…&datetime=…&elevation=…&mode=tiles`.
    // Any other fragment, such as the `#metadata` tab, is not the map's.
    const FRAGMENT_KEYS = ['map','parameter-name','style','datetime','elevation','mode'];
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
    const map = new maplibregl.Map({
      container: element,
      style: { version: 8, sources: {}, layers: [{ id: 'background', type: 'background', paint: { 'background-color': palette().water } }] },
      center: [10, 30], zoom: 1,
      // A collection page's map sits on a long page: the wheel and one
      // finger scroll it, and the map pans with a modifier or two fingers.
      cooperativeGestures: element.dataset.quicklook !== 'true'
    });
    map.on('load',() => { started = true; });
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
      const block = document.getElementById('map-controls');
      const form = document.getElementById('map-form');
      const modeInputs = [...document.querySelectorAll('input[name="map-mode"]')];
      const parameter = document.getElementById('map-parameter');
      const style = document.getElementById('map-style');
      const time = document.getElementById('map-time');
      const level = document.getElementById('map-level');
      const previous = document.getElementById('map-time-prev');
      const next = document.getElementById('map-time-next');
      const help = document.getElementById('map-help');
      const generalHelp = help?.textContent;
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
      let drawnDefault, defaultParameter, nearNow = false;
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
      // The fragment's choices, where this page offers them. A value it
      // does not offer keeps the control's default.
      function restore(saved) {
        if (!saved) return;
        if (saved.mode && modeInputs.some(input => input.value === saved.mode)) modeInputs.forEach(input => { input.checked = input.value === saved.mode; });
        if (saved['parameter-name']) choose(parameter, option => option.value === saved['parameter-name']);
        if (saved.style) choose(style, option => option.dataset.id === saved.style);
        if (saved.elevation) choose(level, option => option.value !== '' && Number(option.value) === Number(saved.elevation));
        const ms = Date.parse(saved.datetime || '');
        if (time && Number.isFinite(ms)) {
          if (timeList) choose(time, option => option.value !== '' && Date.parse(option.value) === ms);
          else time.value = instant(ms).slice(0,19);
        }
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
      function selectedTimeValue() {
        if (!time?.value) return '';
        const ms = Date.parse(time.type === 'datetime-local' ? `${time.value}Z` : time.value);
        return Number.isFinite(ms) ? instant(ms) : '';
      }
      restore(saved);
      // A forecast whose axis runs past now opens at the valid time nearest
      // the viewer's clock, not at the server's default (its last step).
      // The collection advertises no default time of its own. The margin
      // keeps a radar whose newest frame is a little ahead of a slow clock
      // on Default, which follows new frames.
      if (timeList && time.options.length > 1 && !saved?.datetime) {
        const now = Date.now();
        if (Date.parse(time.options[time.options.length - 1].value) > now + 10 * 60 * 1000) {
          time.selectedIndex = nearestOption(now); nearNow = true;
        }
      }
      // What the map draws: one object, which every request reads. A
      // control writes its own key on `change`, so a pan, a step or a retry
      // never picks up an edit the viewer has not made, and nothing waits
      // for a button. Keys are the API's parameter names.
      const state = {
        mode: modeInputs.find(input => input.checked)?.value || modes[0],
        'parameter-name': parameter?.value || '',
        style: Math.max(0, style.selectedIndex),
        datetime: selectedTimeValue(),
        elevation: level?.value || ''
      };
      function apply(change) {
        const changed = Object.keys(change).some(key => state[key] !== change[key]);
        Object.assign(state, change);
        if (!changed) return;
        if ('mode' in change) applyMode();
        updateTimeButtons();
        render();
        saveFragment();
      }
      function stepTime(direction) {
        const index = timeIndex() + direction;
        if (index < 1 || index >= time.options.length) return;
        time.selectedIndex = index; nearNow = false;
        apply({ datetime: selectedTimeValue() });
      }
      previous?.addEventListener('click',() => stepTime(-1));
      next?.addEventListener('click',() => stepTime(1));
      parameter?.addEventListener('change',() => apply({ 'parameter-name': parameter.value }));
      style.addEventListener('change',() => apply({ style: Math.max(0, style.selectedIndex) }));
      level?.addEventListener('change',() => apply({ elevation: level.value }));
      time?.addEventListener('change',() => { nearNow = false; apply({ datetime: selectedTimeValue() }); });
      modeInputs.forEach(input => input.addEventListener('change',() => { if (input.checked) apply({ mode: input.value }); }));
      // Without the script the form opens the Maps image. With it, Enter in
      // the date and time field applies that time, and the page stays.
      form?.addEventListener('submit',event => {
        event.preventDefault();
        if (time && !timeList) { nearNow = false; apply({ datetime: selectedTimeValue() }); }
      });
      // The fragment follows the view and the choices, replaced rather than
      // pushed so history stays clean, and at most about once a second:
      // Safari allows 100 replaceState calls per 30 s.
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
          const bar = document.createElement('div'); bar.className = 'map-legend-bar';
          bar.style.background = legend.interpolation === 'step'
            ? `linear-gradient(to right, ${stops.map((stop,i) => `${stop.color} ${at(stop.value)}% ${at(stops[i + 1]?.value ?? max)}%`).join(', ')})`
            : `linear-gradient(to right, ${stops.map(stop => `${stop.color} ${at(stop.value)}%`).join(', ')})`;
          bar.style.opacity = String(DATA_OPACITY);
          const scale = document.createElement('div'); scale.className = 'map-legend-ticks';
          const width = legendBox.clientWidth || 300;
          ticks(min, max, Math.max(2, Math.floor(width / 90))).forEach(value => {
            const tick = document.createElement('span'); tick.textContent = formatValue(value);
            const position = at(value); tick.style.left = `${position}%`;
            if (position < 4) tick.classList.add('start'); else if (position > 96) tick.classList.add('end');
            scale.append(tick);
          });
          legendScale.replaceChildren(bar,scale);
        }
        const title = legend.parameter ? parameterTitle(legend.parameter, legend.unit) : (legend.title || styleTitle);
        legendTitle.textContent = styleTitle && styleTitle !== 'Default' ? `${title} · ${styleTitle}` : title;
        legendScale.hidden = false;
      }
      function nameDefaultParameter(name) {
        const known = findParameter(name);
        defaultParameter = known?.name || name;
        if (parameter) parameter.options[0].textContent = `Default · ${known?.label || name}`;
        if (shown && statusShowsFrame) showing(shown);
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
      function frameText(info) {
        const { want, drawn } = info;
        const parts = [drawn ? `${formatTime(drawn)} UTC${want.nearNow ? ', the time nearest now' : ''}` : time ? 'the default time' : 'the map data'];
        const name = want['parameter-name'] || defaultParameter;
        if (name) parts.push(parameterTitle(name));
        if (want.style > 0) parts.push(style.options[want.style]?.textContent);
        if (want.elevation) parts.push([...(level?.options || [])].find(option => option.value === want.elevation)?.textContent || want.elevation);
        return parts.filter(Boolean).join(' · ');
      }
      // The status line. While a request is out or after it failed, it
      // also names the frame still on the map; a failure marks it stale.
      let shown, statusShowsFrame = false;
      function report(text, kind) {
        const still = kind && shown;
        status.textContent = still ? `${text} Still showing ${frameText(shown)}.` : text;
        status.classList.toggle('stale', Boolean(still && kind === 'stale'));
        statusShowsFrame = false;
      }
      function disableImageLink() {
        link.removeAttribute('href'); link.setAttribute('aria-disabled','true');
      }
      // A frame has been swapped in: its request is the one linked, and the
      // status names what it shows.
      function showing(info) {
        shown = info;
        link.href = info.href; link.removeAttribute('aria-disabled');
        requestText.textContent = info.text;
        if (info.kind === 'image' && !info.want.datetime && timeList) {
          drawnDefault = info.drawn;
          time.options[0].textContent = info.drawn ? `Default · ${formatTime(info.drawn)}` : 'Default';
          updateTimeButtons();
        }
        report(`${info.kind === 'tiles' ? 'Map tiles · ' : ''}Showing ${frameText(info)}${info.note ? `. ${info.note}` : ''}`);
        statusShowsFrame = true;
      }
      // A request that drew nothing new. The frame on the map stays; the
      // failed request is listed only while there is none.
      function failed(error, requested) {
        if (!shown && requested) { requestText.textContent = requested; disableImageLink(); }
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
        const choseLayer = Boolean(want['parameter-name'] || want.style > 0 || want.elevation);
        if (!want.datetime && !choseLayer) error.noData = true;
        else if (choseLayer && description) error.message = `Nothing to draw for this choice: ${description}`;
        else if (want.datetime) error.message = `No data at ${formatTime(new Date(want.datetime))} UTC. Choose another time or Default.`;
        else error.message = 'No data for this choice right now. Choose another parameter, style or level, or Default.';
        return error;
      }
      const BUSY = 'The server is busy. Pan, zoom or change a choice to try again.';
      // A failed map request as a sentence. A 404 for the viewer's own
      // choice says why in the server's words: a style the parameter does
      // not have is no lack of data. A 503 is retried.
      async function requestError(response, want) {
        let description;
        try { description = (await response.json())?.description; } catch (_) {}
        if (response.status === 204 || [404,500,502].includes(response.status)) {
          return noDataError(want, response.status === 404 && typeof description === 'string' ? description : undefined);
        }
        const error = new Error();
        if ([503,504].includes(response.status)) {
          error.retry = true; error.message = BUSY;
          const after = response.headers.get('retry-after');
          error.after = /^\d+$/.test(after || '') ? Number(after) : Math.max(0, (Date.parse(after || '') - Date.now()) / 1000) || 0;
        } else if (response.status === 400 && typeof description === 'string') {
          error.message = `The server rejected this request: ${description}`;
        } else {
          error.message = `Map request failed (HTTP ${response.status}).`;
        }
        return error;
      }
      // Busy: try again after `Retry-After`, or 1, 2 and 4 s, at most three
      // times; the frame stays meanwhile.
      const RETRIES = 3;
      let retryTimer;
      function retryLater(attempt, after) {
        const seconds = Math.min(30, Math.max(after || 0, 2 ** attempt));
        report(`Server busy, retrying in ${seconds} s…`, 'loading');
        retryTimer = setTimeout(() => render(attempt + 1), seconds * 1000);
      }

      // The data layer is a stack of frames, each one raster source and
      // layer under the borders: `front` is drawn, `staged` loads at
      // opacity 0. `stageFrame` owns a frame's whole life, so the time bar
      // can grow it into a pool of buffered frames.
      const NO_TRANSITION = { duration: 0, delay: 0 };
      let front, staged, frameCount = 0;
      function dropFrame(frame) {
        if (!frame) return;
        map.off('render',frame.onRender);
        if (map.getLayer(frame.id)) map.removeLayer(frame.id);
        if (map.getSource(frame.id)) map.removeSource(frame.id);
      }
      function discardStaged() { const frame = staged; staged = undefined; dropFrame(frame); }
      function clearFrames() { discardStaged(); const frame = front; front = undefined; dropFrame(frame); shown = undefined; }
      // Stage `source` invisibly. Once MapLibre reports every tile in view
      // loaded or failed, `settle(frame)` judges it: true swaps it in with
      // one paint, the new layer to the data opacity and the old to 0, both
      // without a transition (MapLibre's default 300 ms fade would show the
      // basemap through both), and retires the old frame after that paint.
      // False drops it and leaves the front frame up. The visible source is
      // never removed or retargeted.
      function stageFrame(source, settle) {
        discardStaged();
        const frame = { id: `map-frame-${++frameCount}`, loaded: 0, errors: [] };
        staged = frame;
        map.addSource(frame.id, source);
        map.addLayer({ id: frame.id, type: 'raster', source: frame.id,
          paint: { 'raster-opacity': 0, 'raster-opacity-transition': NO_TRANSITION, 'raster-fade-duration': 0 } },
        map.getLayer('border-halo') ? 'border-halo' : 'outlines');
        frame.onRender = () => {
          if (staged !== frame || !map.isSourceLoaded(frame.id)) return;
          map.off('render',frame.onRender);
          staged = undefined;
          if (!settle(frame)) { dropFrame(frame); return; }
          const old = front;
          front = frame;
          map.setPaintProperty(frame.id,'raster-opacity',DATA_OPACITY);
          if (old) {
            map.setPaintProperty(old.id,'raster-opacity',0);
            map.once('render',() => { if (front !== old) dropFrame(old); });
          }
        };
        map.on('render',frame.onRender);
        map.triggerRepaint();
        return frame;
      }
      // Tiles report per frame: a loaded tile counts, a failed one keeps its
      // HTTP status. A 404 tile is no event at all in MapLibre: no tile
      // loaded and none failed means nothing to draw.
      map.on('sourcedata', event => {
        if (event.tile && staged && event.sourceId === staged.id) staged.loaded++;
      });
      map.on('error', event => {
        if (event.sourceId === 'map-vector') {
          report('Some tiles failed to load; the rest of the view is shown.');
          return;
        }
        const frame = [staged, front].find(f => f && f.id === event.sourceId);
        if (!frame) return;
        frame.errors.push(event.error?.status);
        if (frame === front && frame !== staged) report('Some tiles failed to load; the rest of the view is shown.');
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
        [parameterField, levelField].forEach(field => { if (field) field.hidden = vector; });
        // The time row stays even when a frame has no data: another time may.
        if (timeField) timeField.hidden = vector;
        // A vector-only collection has nothing to choose: no controls at all.
        block.hidden = vector && modes.length === 1;
        let usable = 0;
        for (const option of style.options) {
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
        link.textContent = mode === 'maps' ? 'Open rendered image ↗' : 'Open tileset metadata ↗';
        if (help) help.textContent = vector ? 'Vector tiles load as you pan and zoom: feature geometry in one outline color.' : generalHelp;
      }
      // The request a render makes, fixed when it starts.
      const snapshot = () => ({ ...state, nearNow: nearNow && Boolean(state.datetime) });
      const pixelRatio = () => typeof devicePixelRatio === 'number' && devicePixelRatio > 0 ? devicePixelRatio : 1;
      let generation = 0, pending, timer;
      function render(attempt = 0) {
        clearTimeout(timer); clearTimeout(retryTimer);
        if (state.mode === 'vector') return renderVector();
        clearVector();
        return state.mode === 'maps' ? renderImage(attempt) : renderTiles(attempt);
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
      async function renderTiles(attempt) {
        const current = ++generation;
        pending?.abort(); discardStaged();
        const want = snapshot();
        showLegend(want);
        const option = style.options[want.style];
        try {
          report('Loading map tiles…', 'loading');
          const tileset = await webMercatorTileset(option.dataset.tiles, 'map');
          if (current !== generation) return;
          const template = tileTemplate(tileset, want);
          const source = { type: 'raster', tiles: [template], minzoom: tileset.minzoom, maxzoom: tileset.maxzoom, tileSize: 256 };
          if (tileBounds) source.bounds = tileBounds;
          stageFrame(source, frame => {
            if (current !== generation) return false;
            const busy = frame.errors.some(code => code === 503 || code === 504);
            if (!frame.loaded && (frame.errors.length || viewReaches(tileBounds))) {
              if (busy && attempt < RETRIES) retryLater(attempt);
              else failed(busy ? new Error(BUSY) : frame.errors.length ? new Error('Map tiles failed to load.') : noDataError(want), template);
              return false;
            }
            showing({ kind: 'tiles', want, drawn: want.datetime ? new Date(want.datetime) : undefined, href: tileset.self, text: template,
              note: frame.errors.length ? 'Some tiles failed to load; the rest of the view is shown' : '' });
            // Fill the holes a busy server left.
            if (busy && attempt < RETRIES) retryLater(attempt);
            return true;
          });
        } catch (error) {
          if (current !== generation) return;
          failed(error);
        }
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
      // about 4 Mpx: under the Maps caps (8000 a side, 64 Mpx) and a small
      // share of the server's render memory budget.
      const MAX_IMAGE_PIXELS = 4 * 1024 * 1024, MAX_IMAGE_SIDE = 8000;
      function imageSize(share) {
        const ratio = pixelRatio();
        const width = Math.max(1, element.clientWidth * share * ratio), height = Math.max(1, element.clientHeight * ratio);
        const scale = Math.min(1, Math.sqrt(MAX_IMAGE_PIXELS / (width * height)), MAX_IMAGE_SIDE / width, MAX_IMAGE_SIDE / height);
        return [Math.max(1, Math.floor(width * scale)), Math.max(1, Math.floor(height * scale))];
      }
      async function renderImage(attempt) {
        if (!element.clientWidth || !element.clientHeight) return;
        const current = ++generation;
        pending?.abort(); discardStaged();
        pending = new AbortController();
        const controller = pending;
        const timeout = setTimeout(() => controller.abort(), 15000);
        const want = snapshot();
        showLegend(want);
        let requested;
        try {
          const option = style.options[want.style];
          const url = new URL(option?.value || '', location.href);
          if (!option?.value || url.origin !== location.origin || !['http:','https:'].includes(url.protocol)) throw new Error('Map endpoint must use this server.');
          const view = viewBox();
          if (!view) throw new Error('Pan back to the collection to view data.');
          const [width, height] = imageSize(view.share);
          url.searchParams.set('bbox',view.bbox.join(','));
          url.searchParams.set('bbox-crs','CRS:84'); url.searchParams.set('crs','EPSG:3857');
          url.searchParams.set('width',width); url.searchParams.set('height',height);
          url.searchParams.set('f','image/png'); url.searchParams.set('transparent','true');
          ['parameter-name','datetime','elevation'].forEach(key => { if (want[key]) url.searchParams.set(key,want[key]); else url.searchParams.delete(key); });
          requested = url.href;
          if (!shown) requestText.textContent = requested;
          report('Loading the map image…', 'loading');
          const response = await fetch(url,{signal:controller.signal});
          if (!response.ok || response.status === 204) throw await requestError(response, want);
          if (!response.headers.get('content-type')?.startsWith('image/')) throw new Error('Map endpoint did not return an image.');
          // The instant drawn, which the request may have left to the server;
          // without the header, the one asked for.
          const header = response.headers.get('content-datetime');
          const drawn = header && !Number.isNaN(Date.parse(header)) ? new Date(header)
            : want.datetime ? new Date(want.datetime) : undefined;
          const bitmap = await createImageBitmap(await response.blob());
          if (current !== generation || controller.signal.aborted) { bitmap.close(); return; }
          const canvas = document.createElement('canvas');
          canvas.width = bitmap.width; canvas.height = bitmap.height;
          canvas.getContext('2d').drawImage(bitmap,0,0); bitmap.close();
          const coordinates = [[view.west,view.north],[view.east,view.north],[view.east,view.south],[view.west,view.south]];
          stageFrame({ type: 'canvas', canvas, coordinates, animate: false }, () => {
            if (current !== generation) return false;
            showing({ kind: 'image', want, drawn, href: requested, text: requested });
            return true;
          });
        } catch (error) {
          if (current !== generation) return;
          if (error.retry && attempt < RETRIES) retryLater(attempt, error.after);
          else failed(error, requested);
        } finally { clearTimeout(timeout); }
      }
      const VECTOR_LAYERS = ['map-vector-fill','map-vector-line','map-vector-points'];
      function clearVector() {
        VECTOR_LAYERS.forEach(id => { if (map.getLayer(id)) map.removeLayer(id); });
        if (map.getSource('map-vector')) map.removeSource('map-vector');
      }
      // Vector tiles take no time, parameter, style or level, and have no
      // frames to swap: the layers follow the view by themselves.
      async function renderVector() {
        const current = ++generation;
        pending?.abort(); clearFrames(); clearVector();
        noLegend('Vector tiles draw feature geometry in one color; there is no legend.');
        try {
          report('Loading tileset…');
          const tileset = await webMercatorTileset(request.vector.href, 'vector');
          if (current !== generation) return;
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
          requestText.textContent = tileset.template;
          report('Vector tiles · loading as you pan and zoom');
        } catch (error) {
          if (current !== generation) return;
          clearVector(); disableImageLink();
          report(error.message || 'Tiles unavailable.');
        }
      }
      // A pan or zoom: tiles follow the view by themselves; only a map
      // image is requested again, 300 ms after the view settles, with the
      // choices already applied. A resize that leaves the view as it was
      // changes nothing.
      const viewKey = () => { const b = map.getBounds(); return [b.getWest(), b.getSouth(), b.getEast(), b.getNorth(), element.clientWidth, element.clientHeight].join(','); };
      let lastView = viewKey();
      function viewChanged() {
        const key = viewKey();
        if (key === lastView) return;
        lastView = key;
        saveFragment();
        if (state.mode !== 'maps') return;
        // Invalidate at once so an old response cannot replace a new view.
        ++generation; pending?.abort(); clearTimeout(retryTimer); discardStaged(); clearTimeout(timer);
        timer = setTimeout(render,300);
      }
      map.on('moveend',viewChanged);
      new ResizeObserver(() => { if (element.clientWidth && element.clientHeight) map.resize(); }).observe(element);
      applyMode();
      updateTimeButtons();
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
      else if (!bounds.isEmpty()) map.fitBounds(bounds, { padding: 45, maxZoom: data.mapRequest ? 8 : 5, duration: 0 });
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
        if (map.getLayer('border-halo')) map.setPaintProperty('border-halo','line-color',colors.halo);
        if (map.getLayer('borders')) map.setPaintProperty('borders','line-color',colors.border);
        map.setPaintProperty('areas','fill-color',colors.ink); map.setPaintProperty('outlines','line-color',colors.ink);
        map.setPaintProperty('points','circle-color',colors.ink); map.setPaintProperty('points','circle-stroke-color',colors.land);
      });
      try {
        const response = await fetch(element.dataset.land);
        if (!response.ok) throw new Error('Backdrop unavailable');
        const land = await response.json();
        map.addSource('land',{type:'geojson',data:land,attribution:'Natural Earth · public domain'});
        const colors = palette();
        map.addLayer({id:'land',type:'fill',source:'land',paint:{'fill-color':colors.land}},'areas');
        // Keep borders above every replacement raster, with a halo so
        // boundaries remain visible over both dark and bright weather colors.
        map.addLayer({id:'border-halo',type:'line',source:'land',paint:{'line-color':colors.halo,'line-width':3,'line-opacity':0.75}},'outlines');
        map.addLayer({id:'borders',type:'line',source:'land',paint:{'line-color':colors.border,'line-width':1}},'outlines');
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
