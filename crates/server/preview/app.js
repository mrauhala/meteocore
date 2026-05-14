// MeteoCore preview SPA.
//
// Renders the manifest into MapLibre layers and a sidebar of collection cards.
//
// Per-collection model:
//   * Layers start hidden — the user opts in by toggling each card. Showing
//     every collection by default produces visual noise when a deployment has
//     more than a handful (and is expensive for tile-heavy raster layers).
//   * First enable zooms the map to that collection's spatial extent so the
//     user doesn't have to hunt for the data.
//   * Cards display title, description, API surface, spatial bbox, and (when
//     present) a time slider scrubbing through the temporal extent's discrete
//     timesteps.
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
                    paint: { 'background-color': '#0f172a' }
                }
            ]
        },
        center: [10, 30],
        zoom: 1,
        attributionControl: { compact: true }
    });

    map.addControl(new maplibregl.NavigationControl({ showCompass: false }));
    map.addControl(new maplibregl.ScaleControl({ unit: 'metric' }), 'bottom-right');

    const statusEl = document.getElementById('status');
    const listEl = document.getElementById('collections');

    // Shared across every vector collection so a click in one collection
    // dismisses an open popup from another.
    let activePopup = null;

    // Fetch the manifest in parallel with map initialization. Sources can't
    // be added until the style is parsed, so the card factory defers
    // `addSource`/`addLayer` behind `mapReady`. We poll `isStyleLoaded()`
    // instead of subscribing to `load` because the `load` event is gated on
    // glyph/sprite resolution and a minimal style with only a background
    // layer doesn't always fire it in MapLibre 5.

    fetch('/preview/manifest.json')
        .then(function (r) {
            if (!r.ok) throw new Error('HTTP ' + r.status);
            return r.json();
        })
        .then(renderManifest)
        .catch(function (err) {
            statusEl.textContent = 'Failed to load manifest: ' + err.message;
            statusEl.classList.add('error');
            console.error('manifest fetch failed', err);
        });

    function renderManifest(manifest) {
        const collections = manifest.collections || [];
        // Honest pagination message — `pagination.total` can exceed the
        // returned page length (server caps at 100 by default). Showing only
        // `total` would mislead operators with >100 collections into thinking
        // every entry is in the sidebar.
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
                    : total + ' ' + noun + ' available';

        if (collections.length === 0) return;

        collections.forEach(function (c) {
            listEl.appendChild(buildCollectionCard(c));
        });
    }

    // Schedule `fn` to run once the map style is ready to accept
    // `addSource`/`addLayer` calls. `map.isStyleLoaded()` is not reliable
    // here — in some browser-throttled tabs it never returns true — so we
    // probe by calling `addSource`'s own readiness check on a throwaway
    // marker source. On failure we retry on the next `styledata`, `data`,
    // or `idle` event; whichever fires first re-triggers the probe.
    function whenStyleReady(map, fn) {
        const MARKER = '__readiness_probe__';
        let done = false;
        let timer = null;
        // Every entry into `attempt()` first detaches whatever the previous
        // attempt registered. Combined with `data` *not* being a trigger
        // (it fires on every tile load/error/unload — way too noisy),
        // this guarantees at most one queued listener-per-event + one
        // pending timer regardless of how many times `attempt` re-enters.
        function attempt() {
            if (done) return;
            map.off('styledata', attempt);
            map.off('idle', attempt);
            if (timer !== null) {
                clearTimeout(timer);
                timer = null;
            }
            try {
                map.addSource(MARKER, { type: 'geojson', data: { type: 'FeatureCollection', features: [] } });
                map.removeSource(MARKER);
                done = true;
                fn();
            } catch (e) {
                map.once('styledata', attempt);
                map.once('idle', attempt);
                // setTimeout covers the throttled-tab case where neither
                // map event fires.
                timer = setTimeout(attempt, 250);
            }
        }
        attempt();
    }

    // ------------------------------------------------------------------
    // Card construction
    // ------------------------------------------------------------------

    function buildCollectionCard(collection) {
        const state = {
            enabled: false,
            hasZoomed: false,
            timeIndex: null,
            timeValues: temporalValues(collection),
            // Selected parameter name for multi-parameter raster collections.
            // null = use the server's default. The dropdown is only rendered
            // when `parameters.length > 1`, but state holds the first entry's
            // name unconditionally so URL builders can append it without an
            // extra null-check.
            parameter:
                Array.isArray(collection.parameters) && collection.parameters.length > 0
                    ? collection.parameters[0].name
                    : null
        };

        const li = document.createElement('li');
        li.className = 'collection';

        const card = document.createElement('div');
        card.className = 'card';
        li.appendChild(card);

        // -- Header row: enable switch + title --
        const header = document.createElement('div');
        header.className = 'card-header';
        card.appendChild(header);

        const toggle = document.createElement('label');
        toggle.className = 'switch';
        toggle.title = 'Toggle layer visibility';
        const checkbox = document.createElement('input');
        checkbox.type = 'checkbox';
        checkbox.checked = false;
        toggle.appendChild(checkbox);
        const slider = document.createElement('span');
        slider.className = 'switch-slider';
        toggle.appendChild(slider);
        header.appendChild(toggle);

        const titleBlock = document.createElement('div');
        titleBlock.className = 'card-title-block';
        const title = document.createElement('div');
        title.className = 'card-title';
        title.textContent = collection.title || collection.id;
        titleBlock.appendChild(title);
        const subtitle = document.createElement('div');
        subtitle.className = 'card-subtitle';
        subtitle.textContent = collection.id;
        titleBlock.appendChild(subtitle);
        header.appendChild(titleBlock);

        // -- API badges --
        if (collection.apis && collection.apis.length > 0) {
            const badges = document.createElement('div');
            badges.className = 'badges';
            collection.apis.forEach(function (api) {
                const badge = document.createElement('span');
                // `classList.add` validates each token rather than letting a
                // space in `api` silently inject an extra class. The known
                // values never contain whitespace today, but matching the
                // file-level "every dynamic value reaches the DOM safely"
                // hygiene comment is cheap.
                badge.classList.add('badge', 'badge-' + api);
                badge.textContent = api;
                badges.appendChild(badge);
            });
            card.appendChild(badges);
        }

        // -- Description --
        if (collection.description) {
            const desc = document.createElement('p');
            desc.className = 'card-desc';
            desc.textContent = collection.description;
            card.appendChild(desc);
        }

        // -- Metadata: spatial extent, time, etc. --
        const meta = document.createElement('dl');
        meta.className = 'card-meta';
        if (collection.spatial_extent) {
            appendMeta(meta, 'Extent', formatExtent(collection.spatial_extent));
        }
        if (collection.temporal_extent) {
            const t = collection.temporal_extent;
            if (t.start && t.end) {
                appendMeta(meta, 'Time', formatTimeRange(t.start, t.end));
            }
            if (t.total_values) {
                let label = t.total_values + ' timesteps';
                if (t.truncated) label += ' (sliced)';
                appendMeta(meta, 'Steps', label);
            }
        }
        if (collection.tiles) {
            const repr = [];
            if (collection.tiles.raster) repr.push('raster');
            if (collection.tiles.vector) repr.push('vector');
            if (repr.length > 0) appendMeta(meta, 'Tiles', repr.join(' + '));
        }
        if (meta.childNodes.length > 0) card.appendChild(meta);

        // -- Layer controls (style picker, time slider, etc.) --
        const controlsHost = document.createElement('div');
        controlsHost.className = 'controls';
        card.appendChild(controlsHost);

        // -- Wire up sources and layers. `addSource` throws
        // "Style is not done loading" if called before the style finishes
        // parsing, so we attempt-then-retry until it succeeds. Listening on
        // `load` alone is unreliable for our minimal background-only style
        // (the event sometimes doesn't fire at all), but `styledata`
        // continues to fire as the map updates so it eventually unblocks.
        const layerHandles = [];
        // Slider has to land *after* the layer handles exist (else its
        // refresh callback fires against an empty array) AND in the same
        // microtask sequence as the picker/toggle children (else style-pick
        // and slider can end up in opposite orders in throttled tabs where
        // `whenStyleReady` resolves asynchronously). Doing the slider attach
        // inside the callback addresses both. Guard with a tile-presence
        // check: an EDR-only collection has a `temporal_extent` but no tile
        // layer to drive — surfacing a slider there is just noise.
        const hasTiles =
            collection.tiles && (collection.tiles.raster || collection.tiles.vector);
        whenStyleReady(map, function () {
            try {
                if (collection.tiles && collection.tiles.raster) {
                    layerHandles.push(attachRasterLayer(collection, controlsHost, state));
                }
                if (collection.tiles && collection.tiles.vector) {
                    layerHandles.push(attachVectorLayer(collection, controlsHost, state));
                }
                if (state.enabled) {
                    layerHandles.forEach(function (h) { h.setVisible(true); });
                }
                if (hasTiles && state.timeValues && state.timeValues.length > 1) {
                    attachTimeSlider(controlsHost, state, layerHandles);
                }
            } catch (err) {
                console.error(
                    'Failed to attach layers for collection ' + collection.id + ':',
                    err
                );
            }
        });

        // -- Footer: actions --
        const footer = document.createElement('div');
        footer.className = 'card-footer';
        if (collection.spatial_extent) {
            const zoomBtn = document.createElement('button');
            zoomBtn.type = 'button';
            zoomBtn.className = 'link';
            zoomBtn.textContent = 'Zoom to extent';
            zoomBtn.addEventListener('click', function () {
                fitMapToExtent(collection.spatial_extent);
            });
            footer.appendChild(zoomBtn);
        }
        if (footer.childNodes.length > 0) card.appendChild(footer);

        // -- Wire toggle to layer visibility --
        checkbox.addEventListener('change', function () {
            state.enabled = checkbox.checked;
            card.classList.toggle('active', state.enabled);
            layerHandles.forEach(function (h) {
                h.setVisible(state.enabled);
            });
            if (state.enabled && !state.hasZoomed && collection.spatial_extent) {
                fitMapToExtent(collection.spatial_extent);
                state.hasZoomed = true;
            }
        });

        return li;
    }

    function appendMeta(dl, label, value) {
        const dt = document.createElement('dt');
        dt.textContent = label;
        const dd = document.createElement('dd');
        dd.textContent = value;
        dl.appendChild(dt);
        dl.appendChild(dd);
    }

    function fitMapToExtent(extent) {
        const south = Math.max(extent[1], -85);
        const north = Math.min(extent[3], 85);
        const bounds = new maplibregl.LngLatBounds([extent[0], south], [extent[2], north]);
        map.fitBounds(bounds, { padding: 80, maxZoom: 10, duration: 600 });
    }

    function formatExtent(e) {
        // Compact human-readable bbox: "19.3°E, 59.8°N → 31.6°E, 70.1°N".
        // Derive the hemisphere letter from the sign so a western-hemisphere
        // dataset doesn't render as "W -10.50°" (doubly-signed).
        return formatLon(e[0]) + ', ' + formatLat(e[1]) + ' → ' +
               formatLon(e[2]) + ', ' + formatLat(e[3]);
    }

    function formatLon(lon) {
        return Math.abs(lon).toFixed(2) + '°' + (lon < 0 ? 'W' : 'E');
    }

    function formatLat(lat) {
        return Math.abs(lat).toFixed(2) + '°' + (lat < 0 ? 'S' : 'N');
    }

    function formatTimeRange(startIso, endIso) {
        const start = new Date(startIso);
        const end = new Date(endIso);
        if (Number.isNaN(start.getTime()) || Number.isNaN(end.getTime())) {
            return startIso + ' → ' + endIso;
        }
        const sameDay =
            start.getUTCFullYear() === end.getUTCFullYear() &&
            start.getUTCMonth() === end.getUTCMonth() &&
            start.getUTCDate() === end.getUTCDate();
        if (sameDay) {
            return formatDate(start) + ' · ' + formatTimeOnly(start) + ' → ' + formatTimeOnly(end);
        }
        return formatDateTime(start) + ' → ' + formatDateTime(end);
    }

    function formatDate(d) {
        return d.toISOString().slice(0, 10);
    }
    function formatTimeOnly(d) {
        return d.toISOString().slice(11, 16) + 'Z';
    }
    function formatDateTime(d) {
        return d.toISOString().slice(0, 16).replace('T', ' ') + 'Z';
    }

    function temporalValues(collection) {
        const t = collection.temporal_extent;
        if (!t) return null;
        if (Array.isArray(t.values) && t.values.length > 0) return t.values;
        return null;
    }

    // ------------------------------------------------------------------
    // Raster layer
    // ------------------------------------------------------------------

    function attachRasterLayer(collection, controlsHost, state) {
        const styles = collection.styles || [];
        let currentStyle = styles.length > 0 ? styles[0].id : 'default';

        const sourceId = 'src-' + collection.id;
        const layerId = 'layer-' + collection.id;

        // No `attribution`: MapLibre renders it via innerHTML.
        map.addSource(sourceId, {
            type: 'raster',
            tiles: [tileUrlFor(collection, currentStyle, currentTime(state), state.parameter)],
            tileSize: 256
        });
        map.addLayer({
            id: layerId,
            type: 'raster',
            source: sourceId,
            // Hidden until the operator opts in via the card toggle. Layout
            // properties are honoured before paint, so this is enough — no
            // need to also gate the source.
            layout: { visibility: 'none' },
            paint: { 'raster-opacity': 1 }
        });

        // -- Parameter picker (multi-parameter raster engines only). Rendered
        //    above the style picker so the controls flow top-to-bottom in a
        //    "narrow my view" order: parameter → style → time.
        if (Array.isArray(collection.parameters) && collection.parameters.length > 1) {
            const paramLabel = document.createElement('label');
            paramLabel.className = 'control-row';
            const paramText = document.createElement('span');
            paramText.className = 'control-label';
            paramText.textContent = 'Parameter';
            paramLabel.appendChild(paramText);

            const paramSelect = document.createElement('select');
            collection.parameters.forEach(function (p) {
                const opt = document.createElement('option');
                opt.value = p.name;
                const titleText = p.title && p.title !== p.name
                    ? p.name + ' — ' + p.title
                    : p.name;
                opt.textContent = p.unit ? titleText + ' (' + p.unit + ')' : titleText;
                if (p.name === state.parameter) opt.selected = true;
                paramSelect.appendChild(opt);
            });
            paramSelect.addEventListener('change', function () {
                if (state.parameter === paramSelect.value) return;
                state.parameter = paramSelect.value;
                refreshSource();
            });
            paramLabel.appendChild(paramSelect);
            controlsHost.appendChild(paramLabel);
        }

        // -- Style picker (only if there's a real choice) --
        if (styles.length > 1) {
            const styleLabel = document.createElement('label');
            styleLabel.className = 'control-row';
            const styleText = document.createElement('span');
            styleText.className = 'control-label';
            styleText.textContent = 'Style';
            styleLabel.appendChild(styleText);

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
                refreshSource();
            });
            styleLabel.appendChild(select);
            controlsHost.appendChild(styleLabel);
        }

        function refreshSource() {
            const src = map.getSource(sourceId);
            if (src && typeof src.setTiles === 'function') {
                src.setTiles([tileUrlFor(collection, currentStyle, currentTime(state), state.parameter)]);
            }
        }

        return {
            setVisible: function (visible) {
                map.setLayoutProperty(layerId, 'visibility', visible ? 'visible' : 'none');
            },
            refreshForTime: refreshSource
        };
    }

    // Convert an OGC API Tiles URL template
    //   /tiles/.../{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
    // into the form MapLibre raster sources understand
    //   /tiles/.../WebMercatorQuad/{z}/{y}/{x}
    // Placeholder names come from the axum route (see preview.rs), not from
    // the generic `{tms}`/`{z}` which the server would reject.
    function tileUrlFor(collection, styleId, time, parameter) {
        const raster = collection.tiles.raster;
        // 'default' style uses the plain /tiles/... route, not /styles/default/...
        const useStyled = styleId && styleId !== 'default' && raster.styled_url_template;
        let template = useStyled ? raster.styled_url_template : raster.url_template;
        template = template.replace('{tileMatrixSetId}', 'WebMercatorQuad');
        template = template.replace('{tileMatrix}', '{z}');
        template = template.replace('{tileRow}', '{y}');
        template = template.replace('{tileCol}', '{x}');
        if (useStyled) {
            template = template.replace('{styleId}', encodeURIComponent(styleId));
        }
        // Strip manifest's absolute origin so 127.0.0.1↔localhost mismatch in
        // `server.base_url` doesn't trip CSP `connect-src 'self'`.
        template = template.replace(/^https?:\/\/[^/]+/i, '');
        template = appendTimeParam(template, time);
        return appendParameterName(template, parameter);
    }

    // ------------------------------------------------------------------
    // Vector layer
    // ------------------------------------------------------------------

    function attachVectorLayer(collection, controlsHost, state) {
        const sourceId = 'vsrc-' + collection.id;
        const fillLayerId = 'vfill-' + collection.id;
        const lineLayerId = 'vline-' + collection.id;
        const pointLayerId = 'vpoint-' + collection.id;

        // No `attribution`: MapLibre renders it via innerHTML.
        map.addSource(sourceId, {
            type: 'vector',
            tiles: [vectorTileUrlFor(collection, currentTime(state))]
        });

        const layerIds = [fillLayerId, lineLayerId, pointLayerId];

        // `match` over `==` catches `MultiPolygon` too (68/308
        // municipalities in the test data are MultiPolygon).
        map.addLayer({
            id: fillLayerId,
            type: 'fill',
            source: sourceId,
            'source-layer': collection.id,
            filter: ['match', ['geometry-type'], ['Polygon', 'MultiPolygon'], true, false],
            layout: { visibility: 'none' },
            paint: {
                'fill-color': '#60a5fa',
                'fill-opacity': 0.35
            }
        });
        map.addLayer({
            id: lineLayerId,
            type: 'line',
            source: sourceId,
            'source-layer': collection.id,
            filter: ['match', ['geometry-type'], ['Polygon', 'MultiPolygon'], true, false],
            layout: { visibility: 'none' },
            paint: {
                'line-color': '#2563eb',
                'line-width': 1.5
            }
        });
        map.addLayer({
            id: pointLayerId,
            type: 'circle',
            source: sourceId,
            'source-layer': collection.id,
            filter: ['match', ['geometry-type'], ['Point', 'MultiPoint'], true, false],
            layout: { visibility: 'none' },
            paint: {
                'circle-radius': 5,
                'circle-color': '#60a5fa',
                'circle-stroke-color': '#ffffff',
                'circle-stroke-width': 1.5
            }
        });

        map.on('click', layerIds, function (e) {
            if (!e.features || e.features.length === 0) return;
            const feature = e.features[0];
            // MapLibre doesn't auto-dismiss; replace any existing popup.
            if (activePopup) activePopup.remove();
            activePopup = new maplibregl.Popup({ closeButton: true, maxWidth: '320px' })
                .setLngLat(e.lngLat)
                .setDOMContent(buildPopupBody(collection, feature))
                .addTo(map);
            activePopup.on('close', function () {
                activePopup = null;
            });
        });

        layerIds.forEach(function (id) {
            map.on('mouseenter', id, function () {
                map.getCanvas().style.cursor = 'pointer';
            });
            map.on('mouseleave', id, function () {
                map.getCanvas().style.cursor = '';
            });
        });

        return {
            setVisible: function (visible) {
                const v = visible ? 'visible' : 'none';
                layerIds.forEach(function (id) {
                    map.setLayoutProperty(id, 'visibility', v);
                });
            },
            refreshForTime: function () {
                const src = map.getSource(sourceId);
                if (src && typeof src.setTiles === 'function') {
                    src.setTiles([vectorTileUrlFor(collection, currentTime(state))]);
                }
            }
        };
    }

    function vectorTileUrlFor(collection, time) {
        let template = collection.tiles.vector.url_template;
        template = template.replace('{tileMatrixSetId}', 'WebMercatorQuad');
        template = template.replace('{tileMatrix}', '{z}');
        template = template.replace('{tileRow}', '{y}');
        template = template.replace('{tileCol}', '{x}');
        // MapLibre's vector-tile worker rejects relative URLs. Re-anchor to
        // page origin so 127.0.0.1↔localhost mismatch in `server.base_url`
        // doesn't trip CSP `connect-src 'self'`.
        template = template.replace(/^https?:\/\/[^/]+/i, '');
        template = window.location.origin + template;
        return appendTimeParam(template, time);
    }

    function appendTimeParam(template, time) {
        if (!time) return template;
        // Bypass MapLibre's per-source HTTP cache for time changes by including
        // the timestamp in the query string. Tile handler already honours the
        // `datetime` query param at crates/api-tiles/src/handlers.rs:577.
        const sep = template.indexOf('?') === -1 ? '?' : '&';
        return template + sep + 'datetime=' + encodeURIComponent(time);
    }

    function appendParameterName(template, parameter) {
        if (!parameter) return template;
        // Matches EDR's `parameter-name=` convention; api-maps + api-tiles
        // accept this as a non-OGC bridge until the standardised form lands.
        const sep = template.indexOf('?') === -1 ? '?' : '&';
        return template + sep + 'parameter-name=' + encodeURIComponent(parameter);
    }

    // ------------------------------------------------------------------
    // Time slider
    // ------------------------------------------------------------------

    function attachTimeSlider(controlsHost, state, layerHandles) {
        const values = state.timeValues;
        // Default to the latest timestep so an opt-in toggle shows current
        // conditions, matching the server's default `&time=` resolution.
        state.timeIndex = values.length - 1;
        // Track the index that's been pushed to the source so we can skip a
        // no-op refresh when the user scrubs and returns to the same step
        // (each setTiles call re-fetches the visible tiles even with the
        // same URL).
        let appliedIndex = state.timeIndex;
        // Debounce handle: rapid release events on the slider collapse to
        // one server-side render burst instead of N piled-up bursts.
        let pendingRefresh = null;

        const row = document.createElement('div');
        row.className = 'time-slider';

        const label = document.createElement('div');
        label.className = 'control-label';
        label.textContent = 'Time';
        row.appendChild(label);

        const valueLabel = document.createElement('div');
        valueLabel.className = 'time-value';
        valueLabel.textContent = formatSliderTime(values[state.timeIndex]);
        row.appendChild(valueLabel);

        const input = document.createElement('input');
        input.type = 'range';
        input.min = '0';
        input.max = String(values.length - 1);
        input.step = '1';
        input.value = String(state.timeIndex);
        input.addEventListener('input', function () {
            state.timeIndex = parseInt(input.value, 10);
            valueLabel.textContent = formatSliderTime(values[state.timeIndex]);
        });
        input.addEventListener('change', function () {
            if (pendingRefresh !== null) clearTimeout(pendingRefresh);
            pendingRefresh = setTimeout(function () {
                pendingRefresh = null;
                if (state.timeIndex === appliedIndex) return;
                appliedIndex = state.timeIndex;
                layerHandles.forEach(function (h) {
                    if (h.refreshForTime) h.refreshForTime();
                });
            }, 200);
        });
        row.appendChild(input);

        const ticks = document.createElement('div');
        ticks.className = 'time-ticks';
        const firstTick = document.createElement('span');
        firstTick.textContent = formatSliderTime(values[0]);
        const lastTick = document.createElement('span');
        lastTick.textContent = formatSliderTime(values[values.length - 1]);
        ticks.appendChild(firstTick);
        ticks.appendChild(lastTick);
        row.appendChild(ticks);

        controlsHost.appendChild(row);
    }

    function currentTime(state) {
        if (state.timeIndex === null || !state.timeValues) return null;
        return state.timeValues[state.timeIndex];
    }

    function formatSliderTime(iso) {
        const d = new Date(iso);
        if (Number.isNaN(d.getTime())) return iso;
        return formatDateTime(d);
    }

    // ------------------------------------------------------------------
    // Popup
    // ------------------------------------------------------------------

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
            return value.toLocaleString(undefined, { maximumSignificantDigits: 6 });
        }
        return String(value);
    }
})();
