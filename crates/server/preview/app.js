// MeteoCore preview SPA.
//
// Phase 3 scope: render every `tiles.raster` collection as a MapLibre raster
// layer, with a sidebar checkbox to toggle visibility and (when more than one
// style exists) a dropdown to swap styles live. Vector tiles + time slider
// land in later phases.
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

    // `map.on('load')` so MapLibre's style is ready before we add raster
    // sources/layers. Adding sources before style-load throws.
    //
    // Absolute manifest path: relative `manifest.json` would only resolve
    // correctly when the URL ends in `/`, but the route table accepts both
    // `/preview` and `/preview/`. Full proxy-prefix support (relative paths
    // + `<base>` + redirect to enforce trailing slash) is later-phase work.
    map.on('load', function () {
        fetch('/preview/manifest.json')
            .then(function (r) {
                if (!r.ok) throw new Error('HTTP ' + r.status);
                return r.json();
            })
            .then(renderManifest)
            .catch(function (err) {
                statusEl.textContent = 'Failed to load manifest: ' + err.message;
                console.error('manifest fetch failed', err);
            });
    });

    function renderManifest(manifest) {
        const collections = manifest.collections || [];
        // We fetch a single page (default limit=100). The total can exceed
        // that; advertise the shortfall so users don't think the list is
        // complete. Defensive read on `pagination`: a future schema change
        // or a partial parse shouldn't crash the SPA before the map
        // renders. Fall back to the rendered count.
        const rendered = collections.length;
        const total =
            manifest.pagination && typeof manifest.pagination.total === 'number'
                ? manifest.pagination.total
                : rendered;
        const noun = total === 1 ? 'collection' : 'collections';
        statusEl.textContent =
            total === 0
                ? 'No collections registered.'
                : rendered < total
                    ? rendered + ' of ' + total + ' ' + noun + ' (paginated)'
                    : total + ' ' + noun;

        fitMapToCollections(collections);

        // Stable iteration order matches the manifest, which sorts by id.
        // `map.addLayer()` without `beforeId` appends to the top of the
        // draw stack, so the *last* collection iterated (alphabetically
        // latest id) renders on top — same convention as the comment at
        // `attachRasterLayer` ("collections later in the manifest stack
        // on top"). Operators wanting a different stack can re-order
        // collections in config.
        collections.forEach(function (c) {
            const li = document.createElement('li');
            li.className = 'collection';

            const header = document.createElement('div');
            header.className = 'collection-header';

            const title = document.createElement('span');
            title.className = 'title';
            title.textContent = c.title || c.id;
            header.appendChild(title);

            const apis = document.createElement('span');
            apis.className = 'apis';
            apis.textContent = (c.apis || []).join(' · ');
            header.appendChild(apis);

            li.appendChild(header);

            if (c.tiles && c.tiles.raster) {
                attachRasterLayer(c, li);
            }

            listEl.appendChild(li);
        });
    }

    function fitMapToCollections(collections) {
        const bounds = collections.reduce(function (acc, c) {
            if (!c.spatial_extent) return acc;
            const e = c.spatial_extent;
            // Clamp to valid Mercator latitudes — a bbox at the poles
            // (e.g. ECMWF's [-180,-90,180,90]) projects to infinity in
            // web Mercator. MapLibre then accepts the bounds but `fitBounds`
            // resolves to a white-canvas max-zoom view because no finite
            // pixel rectangle contains the bbox.
            const south = Math.max(e[1], -85);
            const north = Math.min(e[3], 85);
            const sw = [e[0], south];
            const ne = [e[2], north];
            if (!acc) return new maplibregl.LngLatBounds(sw, ne);
            acc.extend(sw);
            acc.extend(ne);
            return acc;
        }, null);
        if (bounds) map.fitBounds(bounds, { padding: 60, duration: 0 });
    }

    // -----------------------------------------------------------------
    // Raster layer wiring
    // -----------------------------------------------------------------

    function attachRasterLayer(collection, li) {
        const styles = collection.styles || [];
        let currentStyle = styles.length > 0 ? styles[0].id : 'default';

        const sourceId = 'src-' + collection.id;
        const layerId = 'layer-' + collection.id;
        const initialUrl = tileUrlFor(collection, currentStyle);

        // No `attribution` field: MapLibre renders attribution via
        // `innerHTML` inside its AttributionControl, so a server config
        // entry like `title = "<img src=x onerror=alert(1)>"` would
        // execute script in the preview page. The title already appears
        // in the sidebar (escaped via `textContent`), so the on-map
        // attribution is redundant anyway.
        map.addSource(sourceId, {
            type: 'raster',
            tiles: [initialUrl],
            tileSize: 256
        });
        // Insert above the background but below subsequent layers, so
        // collections later in the manifest stack on top.
        map.addLayer({
            id: layerId,
            type: 'raster',
            source: sourceId,
            paint: { 'raster-opacity': 1 }
        });

        // -- Toggle --
        const controls = document.createElement('div');
        controls.className = 'controls';

        const toggle = document.createElement('label');
        toggle.className = 'toggle';
        const checkbox = document.createElement('input');
        checkbox.type = 'checkbox';
        checkbox.checked = true;
        checkbox.addEventListener('change', function () {
            const visibility = checkbox.checked ? 'visible' : 'none';
            map.setLayoutProperty(layerId, 'visibility', visibility);
        });
        toggle.appendChild(checkbox);
        const toggleText = document.createElement('span');
        toggleText.textContent = 'Show layer';
        toggle.appendChild(toggleText);
        controls.appendChild(toggle);

        // -- Style picker (only when there's a real choice) --
        if (styles.length > 1) {
            const styleLabel = document.createElement('label');
            styleLabel.className = 'style-picker';
            const styleLabelText = document.createElement('span');
            styleLabelText.textContent = 'Style';
            styleLabel.appendChild(styleLabelText);

            const select = document.createElement('select');
            styles.forEach(function (s) {
                const opt = document.createElement('option');
                opt.value = s.id;
                opt.textContent = s.title || s.id;
                if (s.id === currentStyle) opt.selected = true;
                select.appendChild(opt);
            });
            select.addEventListener('change', function () {
                currentStyle = select.value;
                const url = tileUrlFor(collection, currentStyle);
                const source = map.getSource(sourceId);
                if (source && typeof source.setTiles === 'function') {
                    source.setTiles([url]);
                } else {
                    // Silent failure would leave the dropdown out of sync
                    // with the rendered tiles. Vendored MapLibre 5.24.0 does
                    // expose `setTiles`, so this only fires if the binary is
                    // re-vendored against a build that dropped the API.
                    console.warn(
                        'MapLibre source.setTiles unavailable; style swap ' +
                        'requested for ' + collection.id + ' did not apply.'
                    );
                }
            });
            styleLabel.appendChild(select);
            controls.appendChild(styleLabel);
        }

        li.appendChild(controls);
    }

    // Convert an OGC API Tiles URL template
    //   /tiles/.../{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
    // into the form MapLibre raster sources understand
    //   /tiles/.../WebMercatorQuad/{z}/{y}/{x}
    // MapLibre substitutes {z}/{x}/{y}; the OGC `tileMatrix` is z, `tileRow`
    // is y, and `tileCol` is x. The placeholder *names* in the manifest
    // come from the axum route (`{tileMatrixSetId}` / `{tileMatrix}` —
    // see preview.rs:552-554), not from the generic `{tms}` / `{z}` that
    // the server would reject. Substituting `WebMercatorQuad` as a literal
    // path segment fixes the TMS to web-Mercator — the only one the
    // raster source supports today.
    function tileUrlFor(collection, styleId) {
        const raster = collection.tiles.raster;
        // The `default` style is rendered by the plain `/tiles/...` route,
        // not by `/styles/default/tiles/...`. Both endpoints currently
        // produce identical bytes, but the plain one bypasses the style
        // lookup entirely and is the canonical URL — picking it here keeps
        // network traces and HTTP-cache keys consistent across the default
        // case whether a user has explicitly selected "default" or never
        // touched the picker.
        const useStyled = styleId && styleId !== 'default' && raster.styled_url_template;
        let template = useStyled ? raster.styled_url_template : raster.url_template;
        template = template.replace('{tileMatrixSetId}', 'WebMercatorQuad');
        template = template.replace('{tileMatrix}', '{z}');
        template = template.replace('{tileRow}', '{y}');
        template = template.replace('{tileCol}', '{x}');
        if (useStyled) {
            template = template.replace('{styleId}', encodeURIComponent(styleId));
        }
        // Coerce to a same-origin path. The manifest emits absolute URLs
        // built from `server.base_url` (e.g. `http://127.0.0.1:8000/…`),
        // but the user may access the preview on a different origin
        // (`http://localhost:8000/…`). CSP `connect-src 'self'` matches
        // origins literally, so a 127.0.0.1↔localhost mismatch blocks
        // every tile request. The preview SPA is always served by the
        // same binary that serves the tiles, so dropping the origin and
        // letting the browser resolve against the page's location is
        // always safe — and survives the dev-mode host alias and any
        // reverse-proxy host rewriting.
        template = template.replace(/^https?:\/\/[^/]+/i, '');
        return template;
    }
})();
