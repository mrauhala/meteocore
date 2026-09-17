// Render exactly this response page. No second feature fetch or external tiles.
(() => {
  'use strict';
  const status = document.getElementById('map-status');
  if (typeof maplibregl === 'undefined') return;
  try {
    const data = JSON.parse(document.getElementById('map-data').textContent);
    const palette = () => {
      const css = getComputedStyle(document.documentElement);
      return { background: css.getPropertyValue('--surface').trim(), ink: css.getPropertyValue('--accent').trim() };
    };
    const map = new maplibregl.Map({
      container: 'feature-map',
      style: { version: 8, sources: {}, layers: [{ id: 'background', type: 'background', paint: { 'background-color': palette().background } }] },
      center: [10, 30], zoom: 1
    });
    map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
    map.addControl(new maplibregl.ScaleControl({ unit: 'metric' }));
    map.on('style.load', () => {
      const color = palette().ink;
      map.addSource('features', { type: 'geojson', data });
      map.addLayer({ id: 'areas', type: 'fill', source: 'features', filter: ['==', ['geometry-type'], 'Polygon'], paint: { 'fill-color': color, 'fill-opacity': 0.22 } });
      map.addLayer({ id: 'outlines', type: 'line', source: 'features', filter: ['!=', ['geometry-type'], 'Point'], paint: { 'line-color': color, 'line-width': 2 } });
      map.addLayer({ id: 'points', type: 'circle', source: 'features', filter: ['==', ['geometry-type'], 'Point'], paint: { 'circle-color': color, 'circle-radius': 7, 'circle-stroke-color': palette().background, 'circle-stroke-width': 2 } });
      const bounds = new maplibregl.LngLatBounds();
      function extend(coords) {
        if (!Array.isArray(coords)) return;
        if (typeof coords[0] === 'number' && typeof coords[1] === 'number') {
          if (Number.isFinite(coords[0]) && Number.isFinite(coords[1])) bounds.extend(coords);
        } else coords.forEach(extend);
      }
      function geometry(g) {
        if (!g) return;
        if (g.geometries) g.geometries.forEach(geometry);
        else extend(g.coordinates);
      }
      data.features.forEach(feature => geometry(feature.geometry));
      if (!bounds.isEmpty()) map.fitBounds(bounds, { padding: 35, maxZoom: 10, duration: 0 });
      status.textContent = 'Geometry only · current response page. Select a shape to inspect overlapping features.';
      document.addEventListener('workbench-theme', () => {
        const colors = palette();
        map.setPaintProperty('background', 'background-color', colors.background);
        map.setPaintProperty('areas', 'fill-color', colors.ink);
        map.setPaintProperty('outlines', 'line-color', colors.ink);
        map.setPaintProperty('points', 'circle-color', colors.ink);
        map.setPaintProperty('points', 'circle-stroke-color', colors.background);
      });
      const selection = document.createElement('div');
      selection.className = 'map-selection';
      selection.setAttribute('aria-live', 'polite');
      status.after(selection);
      map.on('click', event => {
        const { x, y } = event.point;
        const hits = map.queryRenderedFeatures([[x - 5, y - 5], [x + 5, y + 5]], { layers: ['areas', 'outlines', 'points'] });
        const unique = new Map(hits.map(feature => [feature.properties.href, feature]));
        selection.replaceChildren();
        if (!unique.size) return;
        const heading = document.createElement('p');
        heading.textContent = `${unique.size} feature${unique.size === 1 ? '' : 's'} here`;
        selection.append(heading);
        unique.forEach(feature => {
          const href = new URL(feature.properties.href, location.href);
          if (href.origin !== location.origin) return;
          const link = document.createElement('a');
          link.className = 'btn';
          link.href = href.href;
          link.textContent = feature.properties.label || String(feature.id);
          selection.append(link);
        });
      });
    });
    map.on('error', () => { status.textContent = 'Map unavailable; geometry is listed below.'; });
  } catch (_) {
    status.textContent = 'Map unavailable; geometry is listed below.';
  }
})();
