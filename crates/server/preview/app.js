// MeteoCore preview SPA.
//
// Renders the manifest into MapLibre layers:
//   * `tiles.raster` collections → raster source + raster layer, with a
//     visibility toggle and a style picker (when more than one style is
//     configured).
//   * `tiles.vector` collections → vector source + circle/line/fill layers,
//     with a visibility toggle and a click handler that opens a popup
//     listing the feature's properties.
//
// Time slider lands in a later phase.
//
// XSS hygiene: every dynamic value reaches the DOM through `textContent` or
// as a typed `<option>` value. Never use innerHTML with manifest data.

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

            // Per-collection error isolation: `map.addSource` and
            // `map.addLayer` throw synchronously on duplicate IDs,
            // missing style, and similar boundary conditions. An
            // uncaught throw inside `forEach` aborts the whole loop, so
            // one malformed collection would silently drop every
            // collection after it from the sidebar. Catch + log; the
            // sidebar entry without controls is still informative.
            if (c.tiles && c.tiles.raster) {
                try {
                    attachRasterLayer(c, li);
                } catch (err) {
                    console.error('attachRasterLayer failed for', c.id, err);
                }
            }
            if (c.tiles && c.tiles.vector) {
                try {
                    attachVectorLayer(c, li);
                } catch (err) {
                    console.error('attachVectorLayer failed for', c.id, err);
                }
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
        // Append to the top of the draw stack (no `beforeId` argument).
        // Per the iterator comment above, collections later in the
        // manifest render above earlier ones — that ordering is enforced
        // here by the natural append-to-top behaviour.
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

    // -----------------------------------------------------------------
    // Vector layer wiring
    // -----------------------------------------------------------------

    // Three style layers per source — split by geometry-type so the same
    // collection can carry mixed geometries without per-feature paint
    // overrides. The MVT layer name (`source-layer`) equals the collection
    // id; the ds-mvt encoder uses the same convention server-side.
    function attachVectorLayer(collection, li) {
        const sourceId = 'vsrc-' + collection.id;
        const fillLayerId = 'vfill-' + collection.id;
        const lineLayerId = 'vline-' + collection.id;
        const pointLayerId = 'vpoint-' + collection.id;

        // No `attribution` field: MapLibre renders attribution via
        // `innerHTML` inside its AttributionControl — a server config
        // title like `<img src=x onerror=alert(1)>` would execute
        // script in the preview page. Mirrors the raster source.
        map.addSource(sourceId, {
            type: 'vector',
            tiles: [vectorTileUrlFor(collection)]
        });

        // `match` (not `==`) so the filter catches Multi-variants too.
        // The MVT decoder bundled in MapLibre reconstructs a
        // `Geometry::MultiPolygon` as GeoJSON `MultiPolygon`, so the
        // expression `['geometry-type']` returns `'MultiPolygon'` —
        // an `== 'Polygon'` filter silently drops every multi-feature.
        // In the test data being opted in here, 68/308 municipalities
        // and 9/19 regions are MultiPolygon, so the bug would render
        // a third of the dataset invisible.
        map.addLayer({
            id: fillLayerId,
            type: 'fill',
            source: sourceId,
            'source-layer': collection.id,
            filter: ['match', ['geometry-type'], ['Polygon', 'MultiPolygon'], true, false],
            paint: {
                'fill-color': '#4299e1',
                'fill-opacity': 0.18
            }
        });

        // Lines double as polygon outlines so MultiPolygon boundaries stay
        // visible even when fills overlap. `LineString`/`MultiLineString`
        // arms are dead today (ds-core's `Geometry` enum has no LineString
        // variant), but harmless future-proofing.
        map.addLayer({
            id: lineLayerId,
            type: 'line',
            source: sourceId,
            'source-layer': collection.id,
            filter: [
                'match',
                ['geometry-type'],
                ['Polygon', 'MultiPolygon', 'LineString', 'MultiLineString'],
                true,
                false
            ],
            paint: {
                'line-color': '#2b6cb0',
                'line-width': 1
            }
        });

        map.addLayer({
            id: pointLayerId,
            type: 'circle',
            source: sourceId,
            'source-layer': collection.id,
            filter: ['match', ['geometry-type'], ['Point', 'MultiPoint'], true, false],
            paint: {
                'circle-radius': 5,
                'circle-color': '#4299e1',
                'circle-stroke-color': '#ffffff',
                'circle-stroke-width': 1.5
            }
        });

        const interactiveLayers = [fillLayerId, lineLayerId, pointLayerId];

        // Track the currently-open popup so successive clicks replace
        // rather than stack. MapLibre does not auto-dismiss existing
        // popups when a new one is added, so without this, clicking
        // municipality A then municipality B would leave both panels
        // open. `null` after `.remove()` keeps the check simple.
        let activePopup = null;
        map.on('click', interactiveLayers, function (e) {
            if (!e.features || e.features.length === 0) return;
            const feature = e.features[0];
            if (activePopup) activePopup.remove();
            activePopup = new maplibregl.Popup({ closeButton: true, maxWidth: '320px' })
                .setLngLat(e.lngLat)
                .setDOMContent(buildPopupBody(collection, feature))
                .addTo(map);
            activePopup.on('close', function () {
                activePopup = null;
            });
        });

        // Cursor feedback. Tracked separately per layer because MapLibre
        // dispatches mouseenter/mouseleave per-layer, not per-collection.
        interactiveLayers.forEach(function (id) {
            map.on('mouseenter', id, function () {
                map.getCanvas().style.cursor = 'pointer';
            });
            map.on('mouseleave', id, function () {
                map.getCanvas().style.cursor = '';
            });
        });

        // Sidebar control — single checkbox flips all three layers together.
        const controls = document.createElement('div');
        controls.className = 'controls';

        const toggle = document.createElement('label');
        toggle.className = 'toggle';
        const checkbox = document.createElement('input');
        checkbox.type = 'checkbox';
        checkbox.checked = true;
        checkbox.addEventListener('change', function () {
            const visibility = checkbox.checked ? 'visible' : 'none';
            interactiveLayers.forEach(function (id) {
                map.setLayoutProperty(id, 'visibility', visibility);
            });
        });
        toggle.appendChild(checkbox);
        const toggleText = document.createElement('span');
        toggleText.textContent = 'Vector layer';
        toggle.appendChild(toggleText);
        controls.appendChild(toggle);

        li.appendChild(controls);
    }

    function vectorTileUrlFor(collection) {
        // Same placeholder rewrite as `tileUrlFor()` for rasters — same
        // four OGC placeholder names emitted by the manifest
        // (`{tileMatrixSetId}` / `{tileMatrix}` / `{tileRow}` / `{tileCol}`,
        // not the generic `{tms}` / `{z}`). Plus the URL already carries
        // `?f=mvt` from the manifest, so MapLibre fetches MVT-encoded
        // bytes. Finally, strip the absolute origin so a 127.0.0.1↔
        // localhost mismatch doesn't trip CSP `connect-src 'self'`.
        let template = collection.tiles.vector.url_template;
        template = template.replace('{tileMatrixSetId}', 'WebMercatorQuad');
        template = template.replace('{tileMatrix}', '{z}');
        template = template.replace('{tileRow}', '{y}');
        template = template.replace('{tileCol}', '{x}');
        template = template.replace(/^https?:\/\/[^/]+/i, '');
        return template;
    }

    function buildPopupBody(collection, feature) {
        const root = document.createElement('div');
        root.className = 'popup';

        const heading = document.createElement('h3');
        heading.textContent = collection.title || collection.id;
        root.appendChild(heading);

        const rows = [];
        if (feature.id !== undefined && feature.id !== null && feature.id !== '') {
            rows.push(['id', String(feature.id)]);
        }
        const props = feature.properties || {};
        Object.keys(props)
            .sort()
            .forEach(function (key) {
                rows.push([key, formatPropertyValue(props[key])]);
            });

        if (rows.length === 0) {
            const empty = document.createElement('p');
            empty.className = 'popup-empty';
            empty.textContent = 'No properties.';
            root.appendChild(empty);
            return root;
        }

        rows.forEach(function (kv) {
            const row = document.createElement('div');
            row.className = 'popup-row';
            const k = document.createElement('span');
            k.className = 'k';
            k.textContent = kv[0];
            const v = document.createElement('span');
            v.className = 'v';
            v.textContent = kv[1];
            row.appendChild(k);
            row.appendChild(v);
            root.appendChild(row);
        });
        return root;
    }

    function formatPropertyValue(value) {
        if (value === null || value === undefined) return '—';
        if (typeof value === 'number' && !Number.isInteger(value)) {
            // Trim noisy floating-point trails without losing precision the
            // user actually asked for. 6 sig figs covers area / population /
            // perimeter values for the radar+admin-boundaries preview without
            // turning every popup into a wall of decimal noise.
            return value.toLocaleString(undefined, { maximumSignificantDigits: 6 });
        }
        return String(value);
    }
})();
