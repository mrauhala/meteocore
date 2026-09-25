// Shared locator for collection extents and this page's feature geometry.
// Natural Earth is bundled locally; no third-party map service is contacted.
(function initializeMap() {
  'use strict';
  // Controls precede the sidebar in server-rendered markup. Bind only once
  // the whole document exists, including the selected style's legend.
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded',initializeMap,{once:true});
    return;
  }
  const status = document.getElementById('map-status');
  const element = document.getElementById('feature-map');
  if (typeof maplibregl === 'undefined') return;
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
    map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
    map.addControl(new maplibregl.ScaleControl({ unit: 'metric' }));
    const inspector = document.createElement('div');
    inspector.className = 'mini-inspector';
    inspector.setAttribute('aria-live', 'polite');
    const selection = document.createElement('div');
    selection.className = 'map-selection';
    status.after(selection);
    if (element.dataset.quicklook === 'true') selection.after(inspector);
    function inspect(feature) {
      inspector.replaceChildren();
      const eyebrow = document.createElement('span'); eyebrow.className = 'eyebrow'; eyebrow.textContent = 'QUICK LOOK'; inspector.append(eyebrow);
      const heading = document.createElement('h3'); heading.textContent = feature.properties.label || String(feature.id); inspector.append(heading);
      const facts = document.createElement('div'); facts.className = 'mini-facts';
      Object.entries(feature.properties.facts || { Geometry: feature.geometry?.type || 'Not available' }).forEach(([label,value]) => {
        const fact = document.createElement('div'); const key = document.createElement('small'); key.textContent = label;
        const text = document.createElement('strong'); text.textContent = value === null ? 'Not available' : String(value);
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
      const style = document.getElementById('map-style');
      const time = document.getElementById('map-time');
      const level = document.getElementById('map-level');
      const previous = document.getElementById('map-time-prev');
      const next = document.getElementById('map-time-next');
      const help = document.getElementById('map-help');
      const submit = form.querySelector('button.primary');
      const mapsHelp = help?.textContent;
      const styleField = style.closest('label');
      const timeField = time.closest('.map-time-field');
      const levelField = level?.closest('label');
      function updateTimeButtons() {
        if (!previous || !next) return;
        previous.disabled = time.selectedIndex === 1;
        next.disabled = time.selectedIndex === time.options.length - 1;
      }
      function stepTime(direction) {
        time.selectedIndex = time.selectedIndex === 0
          ? (direction < 0 ? time.options.length - 1 : 1)
          : time.selectedIndex + direction;
        updateTimeButtons(); clearTimeout(timer); render();
      }
      previous?.addEventListener('click',() => stepTime(-1));
      next?.addEventListener('click',() => stepTime(1));
      time.addEventListener('change',updateTimeButtons);
      updateTimeButtons();
      const link = document.getElementById('map-image-link');
      const requestText = document.getElementById('map-image-request');
      const legendImage = document.getElementById('map-legend-image');
      const legendLink = document.getElementById('map-legend-link');
      const legendStatus = document.getElementById('map-legend-status');
      let legendGeneration = 0, activeLegend, legendObjectUrl;
      // Vector-only collections have no legend panel.
      async function showLegend(href, title) {
        if (!legendStatus || activeLegend === href) return;
        activeLegend = href;
        const current = ++legendGeneration;
        legendImage.hidden = true; legendLink.hidden = true;
        legendStatus.textContent = 'Legend not advertised for this style.';
        if (!href) return;
        let url;
        try { url = new URL(href, location.href); } catch (_) { return; }
        if (url.origin !== location.origin || !['http:','https:'].includes(url.protocol)) return;
        legendStatus.textContent = 'Loading legend…';
        let objectUrl;
        try {
          url.searchParams.set('f','png');
          // Legends are cacheable, but an operator can change a style in place.
          // Revalidate on selection so a cached old palette cannot label new pixels.
          const response = await fetch(url,{cache:'no-cache',signal:AbortSignal.timeout(15000)});
          if (!response.ok || !response.headers.get('content-type')?.startsWith('image/')) throw new Error('Legend unavailable');
          const blob = await response.blob();
          if (current !== legendGeneration) return;
          objectUrl = URL.createObjectURL(blob);
          const image = new Image();
          image.src = objectUrl;
          await image.decode();
          if (current !== legendGeneration) { URL.revokeObjectURL(objectUrl); return; }
          if (legendObjectUrl) URL.revokeObjectURL(legendObjectUrl);
          legendObjectUrl = objectUrl;
          legendImage.src = objectUrl; legendImage.alt = `${title} · parameter, units and color scale`;
          legendImage.hidden = false; legendLink.hidden = false; legendStatus.textContent = '';
          url.searchParams.set('f','json'); legendLink.href = url.href;
          legendLink.setAttribute('aria-label',`${title} legend as JSON`);
        } catch (_) {
          if (objectUrl) URL.revokeObjectURL(objectUrl);
          if (current === legendGeneration) { activeLegend = undefined; legendStatus.textContent = 'Legend unavailable.'; }
        }
      }
      function noLegend(text) {
        if (!legendStatus) return;
        activeLegend = undefined; ++legendGeneration;
        legendImage.hidden = true; legendLink.hidden = true; legendStatus.textContent = text;
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
        return time.value ? (time.type === 'datetime-local' ? new Date(`${time.value}Z`).toISOString() : time.value) : '';
      }
      function sameOrigin(href) {
        // Tile URL templates carry literal `{z}`-style placeholders.
        try {
          const url = new URL(href.replace(/[{}]/g,''), location.href);
          return url.origin === location.origin && ['http:','https:'].includes(url.protocol);
        } catch (_) { return false; }
      }
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
            return {
              template: item.href.replace('{tileMatrix}','{z}').replace('{tileRow}','{y}').replace('{tileCol}','{x}'),
              minzoom: zooms.length ? Math.min(...zooms) : 0,
              maxzoom: zooms.length ? Math.max(...zooms) : 22,
              self: tileset.links.find(l => l.rel === 'self')?.href || listHref
            };
          })().catch(error => { tilesets.delete(key); throw error; }));
        }
        return tilesets.get(key);
      }
      function applyMode() {
        const mode = currentMode();
        const vector = mode === 'vector';
        [styleField, timeField, levelField, submit].forEach(field => { if (field) field.hidden = vector; });
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
          : mode === 'tiles' ? 'Map tiles load as you pan and zoom. Choose a style, time or level, then Update map.'
          : 'Vector tiles load as you pan and zoom: feature geometry in one outline color.';
      }
      async function renderTiles(mode) {
        const current = ++generation;
        pending?.abort(); clearTimeout(timer); clearImage(); clearTiles();
        const vector = mode === 'vector';
        const option = style.selectedOptions[0];
        const selectedStyle = vector ? 'Vector tiles' : option.textContent;
        const selectedTime = selectedTimeValue();
        try {
          status.textContent = 'Loading tileset…';
          const tileset = await webMercatorTileset(vector ? request.vector.href : option.dataset.tiles, vector ? 'vector' : 'map');
          if (current !== generation) return;
          const params = new URLSearchParams();
          if (!vector && selectedTime) params.set('datetime',selectedTime);
          if (!vector && level?.value) params.set('elevation',level.value);
          const query = params.toString();
          const template = query ? `${tileset.template}${tileset.template.includes('?') ? '&' : '?'}${query}` : tileset.template;
          const source = { type: vector ? 'vector' : 'raster', tiles: [template], minzoom: tileset.minzoom, maxzoom: tileset.maxzoom };
          if (!vector) source.tileSize = 256;
          if (extentBounds && !extentBounds.isEmpty()) source.bounds = extentBounds.toArray().flat();
          const before = map.getLayer('border-halo') ? 'border-halo' : 'outlines';
          if (vector) {
            const colors = palette(), layer = request.vector.layer;
            map.addSource('map-vector',source);
            const kind = types => ['match',['geometry-type'],types,true,false];
            map.addLayer({id:'map-vector-fill',type:'fill',source:'map-vector','source-layer':layer,filter:kind(['Polygon','MultiPolygon']),paint:{'fill-color':colors.ink,'fill-opacity':0.2}},before);
            map.addLayer({id:'map-vector-line',type:'line',source:'map-vector','source-layer':layer,filter:['!',kind(['Point','MultiPoint'])],paint:{'line-color':colors.ink,'line-width':1.5}},before);
            map.addLayer({id:'map-vector-points',type:'circle',source:'map-vector','source-layer':layer,filter:kind(['Point','MultiPoint']),paint:{'circle-color':colors.ink,'circle-radius':4,'circle-stroke-color':colors.land,'circle-stroke-width':1}},before);
            noLegend('Vector tiles draw feature geometry in one color; there is no legend.');
          } else {
            map.addSource('map-tiles',source);
            map.addLayer({id:'map-tiles',type:'raster',source:'map-tiles',paint:{'raster-opacity':0.85,'raster-fade-duration':0}},before);
            showLegend(option.dataset.legend, selectedStyle);
          }
          link.href = tileset.self; link.removeAttribute('aria-disabled');
          requestText.textContent = template;
          status.textContent = vector ? 'Vector tiles · loading as you pan and zoom'
            : `Map tiles · ${selectedStyle} · ${selectedTime || 'collection default time'}`;
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
      async function renderImage() {
        if (!element.clientWidth || !element.clientHeight) return;
        const current = ++generation;
        discardStaged(); clearTiles();
        pending?.abort(); pending = new AbortController();
        const controller = pending;
        const timeout = setTimeout(() => controller.abort(), 15000);
        let requestedUrl;
        try {
          const selectedStyle = style.selectedOptions[0].textContent;
          const selectedLegend = style.selectedOptions[0].dataset.legend;
          const selectedTime = selectedTimeValue();
          const url = new URL(style.value, location.href);
          if (url.origin !== location.origin || !['http:','https:'].includes(url.protocol)) throw new Error('Map endpoint must use this server.');
          const bounds = map.getBounds();
          // A single image covers at most one world. Do not clamp latitude:
          // map bounds and the server's Web Mercator rendering must agree.
          const west = Math.max(-180,bounds.getWest());
          const east = Math.min(180,bounds.getEast());
          const south = bounds.getSouth(), north = bounds.getNorth();
          if (!(west < east && south < north)) throw new Error('Pan back to the collection to view data.');
          const width = Math.min(1024,Math.max(1,Math.round(element.clientWidth)));
          const height = Math.min(768,Math.max(1,Math.round(element.clientHeight)));
          url.searchParams.set('bbox',[west,south,east,north].join(','));
          url.searchParams.set('bbox-crs','CRS:84'); url.searchParams.set('crs','EPSG:3857');
          url.searchParams.set('width',width); url.searchParams.set('height',height);
          url.searchParams.set('f','image/png'); url.searchParams.set('transparent','true');
          if (selectedTime) url.searchParams.set('datetime',selectedTime); else url.searchParams.delete('datetime');
          if (level?.value) url.searchParams.set('z',level.value); else url.searchParams.delete('z');
          requestedUrl = url.href;
          if (!activeId) requestText.textContent = requestedUrl;
          status.textContent = activeId ? 'Updating map data… Showing the previous image until ready.' : 'Loading map data…';
          const response = await fetch(url,{signal:controller.signal});
          if (!response.ok) throw new Error(`Map request failed (HTTP ${response.status}). Adjust the time or retry.`);
          if (!response.headers.get('content-type')?.startsWith('image/')) throw new Error('Map endpoint did not return an image.');
          const bitmap = await createImageBitmap(await response.blob());
          if (current !== generation || controller.signal.aborted) { bitmap.close(); return; }
          const canvas = document.createElement('canvas');
          canvas.width = bitmap.width; canvas.height = bitmap.height;
          canvas.getContext('2d').drawImage(bitmap,0,0); bitmap.close();
          const coordinates = [[west,north],[east,north],[east,south],[west,south]];
          // Stage the replacement invisibly. Keep the displayed image and
          // request link until MapLibre has uploaded the new source's pixels.
          const nextId = activeId === 'map-data-0' ? 'map-data-1' : 'map-data-0';
          const onRender = () => {
            if (current !== generation || !map.isSourceLoaded(nextId)) return;
            map.off('render',onRender);
            map.setPaintProperty(nextId,'raster-opacity',0.85);
            if (activeId) { map.removeLayer(activeId); map.removeSource(activeId); }
            activeId = nextId; staged = undefined;
            showLegend(selectedLegend, selectedStyle);
            link.href = requestedUrl; link.removeAttribute('aria-disabled');
            requestText.textContent = requestedUrl;
            status.textContent = `Map data loaded · ${selectedStyle} · ${selectedTime || 'collection default time'}`;
          };
          staged = {id:nextId,onRender};
          map.addSource(nextId,{type:'canvas',canvas,coordinates,animate:false});
          map.addLayer({id:nextId,type:'raster',source:nextId,paint:{'raster-opacity':0,'raster-fade-duration':0}},map.getLayer('border-halo') ? 'border-halo' : 'outlines');
          map.on('render',onRender); map.triggerRepaint();
        } catch (error) {
          if (current !== generation) return;
          clearImage();
          activeLegend = undefined;
          ++legendGeneration; legendImage.hidden = true; legendLink.hidden = true;
          legendStatus.textContent = 'Legend will appear after the map loads.';
          disableImageLink();
          if (requestedUrl) requestText.textContent = requestedUrl;
          status.textContent = error.name === 'AbortError' ? 'Map request timed out. Use Update map to retry.' : (error.message || 'Map data unavailable. Use Update map to retry.');
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
        const labels=[];
        function labelCountries() {
          labels.forEach(marker=>marker.remove()); labels.length=0;
          if (map.getZoom()>7) return;
          const visible=land.features.filter(f => f.properties.x != null && f.properties.y != null && map.getBounds().contains([f.properties.x,f.properties.y]));
          visible.slice(0,12).forEach(f => {
            const label=document.createElement('span');label.className='country-label';label.textContent=f.properties.name;
            const marker = new maplibregl.Marker({element:label}).setLngLat([f.properties.x,f.properties.y]).addTo(map);
            label.setAttribute('role','presentation'); label.setAttribute('aria-hidden','true'); label.removeAttribute('tabindex'); labels.push(marker);
          });
        }
        labelCountries();map.on('moveend',labelCountries);
      } catch (_) { if (!data.mapRequest) status.textContent += ' Geographic backdrop unavailable.'; }
    });
    map.on('error', event => {
      // A tile that fails to load leaves the rest of the tiled view usable.
      if (['map-tiles','map-vector'].includes(event.sourceId)) { status.textContent = 'Some tiles failed to load; the rest of the view is shown.'; return; }
      status.textContent = 'Map unavailable; coordinates remain available below.';
    });
  } catch (_) { status.textContent = 'Map unavailable; coordinates remain available below.'; }
})();
