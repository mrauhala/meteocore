// Reuse the preview's vendored MapLibre and minimal background style. The
// embedded snapshot is exactly this page, with no unfiltered second fetch.
(function () {
    'use strict';
    const status = document.getElementById('map-status');
    if (typeof maplibregl === 'undefined') return;
    try {
        const data = JSON.parse(document.getElementById('map-data').textContent);
        const map = new maplibregl.Map({
            container: 'feature-map',
            style: {
                version: 8,
                sources: {},
                layers: [{ id: 'background', type: 'background', paint: { 'background-color': '#eef2f6' } }]
            },
            center: [10, 30], zoom: 1
        });
        map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
        map.addControl(new maplibregl.ScaleControl({ unit: 'metric' }));
        // style.load does not wait for sources, unlike the map's load event.
        map.on('style.load', function () {
            map.addSource('features', { type: 'geojson', data: data });
            map.addLayer({ id: 'areas', type: 'fill', source: 'features',
                filter: ['==', ['geometry-type'], 'Polygon'],
                paint: { 'fill-color': '#0b66c3', 'fill-opacity': 0.3 } });
            map.addLayer({ id: 'outlines', type: 'line', source: 'features',
                filter: ['==', ['geometry-type'], 'Polygon'],
                paint: { 'line-color': '#0b66c3', 'line-width': 2 } });
            map.addLayer({ id: 'points', type: 'circle', source: 'features',
                filter: ['==', ['geometry-type'], 'Point'],
                paint: { 'circle-color': '#0b66c3', 'circle-radius': 6,
                    'circle-stroke-color': '#fff', 'circle-stroke-width': 1 } });
            const bounds = new maplibregl.LngLatBounds();
            function extend(coords) {
                if (typeof coords[0] === 'number' && typeof coords[1] === 'number') {
                    if (Number.isFinite(coords[0]) && Number.isFinite(coords[1])) bounds.extend(coords);
                } else coords.forEach(extend);
            }
            data.features.forEach(function (feature) { extend(feature.geometry.coordinates); });
            if (!bounds.isEmpty()) map.fitBounds(bounds, { padding: 35, maxZoom: 10, duration: 0 });
            status.textContent = 'Map shows the features on this page.';
        });
        map.on('error', function () { status.textContent = 'Map unavailable; geometry is listed below.'; });
    } catch (_) {
        status.textContent = 'Map unavailable; geometry is listed below.';
    }
}());
