// MeteoCore preview SPA.
//
// v1 scope (Phase 2): boot a MapLibre canvas, fetch /preview/manifest.json,
// list every collection in the sidebar. Layer rendering (raster, vector tiles,
// time slider) lands in Phase 3+.
//
// XSS hygiene: every dynamic value reaches the DOM through `textContent` only.
// Never use innerHTML with manifest data.

(function () {
    'use strict';

    const map = new maplibregl.Map({
        container: 'map',
        style: {
            version: 8,
            sources: {},
            layers: [
                {
                    id: 'background',
                    type: 'background',
                    paint: { 'background-color': '#e2e8f0' }
                }
            ]
        },
        center: [10, 30],
        zoom: 1,
        attributionControl: { compact: true }
    });

    map.addControl(new maplibregl.NavigationControl({ showCompass: false }));

    const statusEl = document.getElementById('status');
    const listEl = document.getElementById('collections');

    // Absolute path matches the asset references in `index.html` (also
    // `/preview/...`). A relative `manifest.json` would only resolve
    // correctly when the page URL ends in `/`, and the route table accepts
    // both `/preview` and `/preview/`. Full proxy-prefix support (relative
    // paths + a `<base>` tag + a server redirect to enforce the trailing
    // slash) is tracked for a later phase.
    fetch('/preview/manifest.json')
        .then(function (r) {
            if (!r.ok) throw new Error('HTTP ' + r.status);
            return r.json();
        })
        .then(function (manifest) {
            const total = manifest.pagination.total;
            statusEl.textContent =
                total === 0
                    ? 'No collections registered.'
                    : total + (total === 1 ? ' collection' : ' collections');

            // Fit the map to the union of collection extents so the user
            // sees roughly the right region on first load.
            const bounds = manifest.collections.reduce(function (acc, c) {
                if (!c.spatial_extent) return acc;
                const e = c.spatial_extent;
                if (!acc) return new maplibregl.LngLatBounds([e[0], e[1]], [e[2], e[3]]);
                acc.extend([e[0], e[1]]);
                acc.extend([e[2], e[3]]);
                return acc;
            }, null);
            if (bounds) map.fitBounds(bounds, { padding: 60, duration: 0 });

            manifest.collections.forEach(function (c) {
                const li = document.createElement('li');

                const title = document.createElement('span');
                title.className = 'title';
                title.textContent = c.title || c.id;
                li.appendChild(title);

                const apis = document.createElement('span');
                apis.className = 'apis';
                apis.textContent = (c.apis || []).join(' · ');
                li.appendChild(apis);

                listEl.appendChild(li);
            });
        })
        .catch(function (err) {
            statusEl.textContent = 'Failed to load manifest: ' + err.message;
            console.error('manifest fetch failed', err);
        });
})();
