// Shared locator for collection extents and this page's feature geometry.
// Natural Earth is bundled locally; no third-party map service is contacted.
(() => {
  'use strict';
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
    map.on('style.load', async () => {
      const color = palette().ink;
      map.addSource('features', { type: 'geojson', data });
      map.addLayer({ id: 'areas', type: 'fill', source: 'features', filter: ['==', ['geometry-type'], 'Polygon'], paint: { 'fill-color': color, 'fill-opacity': 0.18 } });
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
      if (!bounds.isEmpty()) map.fitBounds(bounds, { padding: 45, maxZoom: 5, duration: 0 });
      status.textContent = element.dataset.quicklook === 'true' ? 'Current page · select a shape for a quick look.' : 'Advertised geometry · general-purpose locator.';
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
        map.addLayer({id:'borders',type:'line',source:'land',paint:{'line-color':palette().border,'line-width':0.8}},'areas');
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
      } catch (_) { status.textContent += ' Geographic backdrop unavailable.'; }
    });
    map.on('error', () => { status.textContent = 'Map unavailable; coordinates remain available below.'; });
  } catch (_) { status.textContent = 'Map unavailable; coordinates remain available below.'; }
})();
