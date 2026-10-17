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
//   * Only an entry with map tiles gets a layer switch. One without says so
//     and links to its collection page instead of offering a switch that
//     draws nothing.
//   * A multi-parameter entry opens on the engine's default parameter, the
//     one the manifest marks `default` (what Maps and WMS draw without
//     `parameter-name`), not on the alphabetical first.
//   * Below 640 px the sidebar is a bottom sheet (see app.css).
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
    const sidebarEl = document.getElementById('sidebar');
    const sheetToggle = document.getElementById('sheet-toggle');
    // Matches the app.css breakpoint that turns the sidebar into a sheet.
    const phoneLayout = window.matchMedia('(max-width: 639px)');

    // Gives each time slider a unique id for its `<label for>`.
    let sliderCount = 0;

    setUpSheet();

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
    // Sidebar / bottom sheet
    // ------------------------------------------------------------------

    // The header button folds the phone sheet down to its header. The
    // sheet's height is published as `--sheet-offset` so MapLibre's bottom
    // controls sit above it (CSSOM, which the page's CSP allows).
    function setUpSheet() {
        sheetToggle.addEventListener('click', function () {
            const folded = sidebarEl.classList.toggle('folded');
            sheetToggle.setAttribute('aria-expanded', folded ? 'false' : 'true');
            // The visible text follows the state; the accessible name
            // (aria-label) stays fixed and aria-expanded carries the state.
            sheetToggle.textContent = folded ? 'Show list' : 'Hide list';
        });
        function publishSheetOffset() {
            const offset = phoneLayout.matches
                ? Math.max(0, window.innerHeight - sidebarEl.getBoundingClientRect().top)
                : 0;
            document.documentElement.style.setProperty('--sheet-offset', offset + 'px');
        }
        if (typeof ResizeObserver === 'function') {
            new ResizeObserver(publishSheetOffset).observe(sidebarEl);
        }
        window.addEventListener('resize', publishSheetOffset);
        publishSheetOffset();
    }

    // Map padding that keeps a fitted extent out from under the sidebar:
    // past its right edge on a desktop, above the sheet on a phone. The
    // right and top margins clear the navigation control.
    function fitPadding() {
        const rect = sidebarEl.getBoundingClientRect();
        const width = window.innerWidth;
        const height = window.innerHeight;
        const padding = phoneLayout.matches
            ? { top: 24, right: 56, bottom: Math.max(24, height - rect.top + 24), left: 24 }
            : { top: 40, right: 64, bottom: 40, left: Math.max(24, rect.right + 24) };
        // fitBounds does not move at all when the padding leaves no room,
        // so give up the sidebar's side before that happens.
        const minimum = 64;
        if (width - padding.left - padding.right < minimum) {
            padding.left = 24;
            padding.right = 24;
        }
        if (height - padding.top - padding.bottom < minimum) {
            padding.top = 24;
            padding.bottom = 24;
        }
        return padding;
    }

    // ------------------------------------------------------------------
    // Card construction
    // ------------------------------------------------------------------

    // The parameter the card opens on: the engine's default, which the
    // manifest marks, else the first (alphabetical) entry.
    function defaultParameter(collection) {
        const parameters = Array.isArray(collection.parameters) ? collection.parameters : [];
        const marked = parameters.find(function (p) { return p.default === true; });
        if (marked) return marked.name;
        return parameters.length > 0 ? parameters[0].name : null;
    }

    function hasMapLayer(collection) {
        return Boolean(collection.tiles && (collection.tiles.raster || collection.tiles.vector));
    }

    function buildCollectionCard(collection) {
        const state = {
            enabled: false,
            hasZoomed: false,
            timeIndex: null,
            // The time axis the slider scrubs: the selected parameter's own
            // when it has one, else the collection's (see `timeAxisFor`).
            timeValues: null,
            // Selected parameter name for multi-parameter raster collections.
            // null = use the server's default. The dropdown is only rendered
            // when `parameters.length > 1`, but state holds the default
            // entry's name unconditionally so URL builders can append it
            // without an extra null-check.
            parameter: defaultParameter(collection),
            // Set by the time slider: re-binds it to another parameter's axis.
            rebindTimeAxis: null
        };
        state.timeValues = timeAxisFor(collection, state.parameter).values;

        const li = document.createElement('li');
        li.className = 'collection';

        const card = document.createElement('div');
        card.className = 'card';
        li.appendChild(card);

        // -- Header row: enable switch + title. Only an entry with map tiles
        //    gets the switch: on any other it would draw nothing. --
        const header = document.createElement('div');
        header.className = 'card-header';
        card.appendChild(header);

        const displayTitle = collection.title || collection.id;
        const hasTiles = hasMapLayer(collection);
        let checkbox = null;
        if (hasTiles) {
            const toggle = document.createElement('label');
            toggle.className = 'switch';
            checkbox = document.createElement('input');
            checkbox.type = 'checkbox';
            checkbox.checked = false;
            // Each switch needs its own name: the label holds no text.
            // The same text is the hover tooltip for mouse users.
            checkbox.setAttribute('aria-label', 'Show ' + displayTitle + ' on the map');
            toggle.title = 'Show ' + displayTitle + ' on the map';
            toggle.appendChild(checkbox);
            const slider = document.createElement('span');
            slider.className = 'switch-slider';
            toggle.appendChild(slider);
            header.appendChild(toggle);
        }

        const titleBlock = document.createElement('div');
        titleBlock.className = 'card-title-block';
        const title = document.createElement('div');
        title.className = 'card-title';
        title.textContent = displayTitle;
        titleBlock.appendChild(title);
        const subtitle = document.createElement('div');
        subtitle.className = 'card-subtitle';
        subtitle.textContent = collection.id;
        titleBlock.appendChild(subtitle);
        header.appendChild(titleBlock);

        // -- API badges: the APIs that serve the collection (the manifest
        //    derives them from the registries, not the configured `apis`). --
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
        if (hasTiles) {
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
                    if (longestTimeAxis(collection) > 1) {
                        attachTimeSlider(collection, controlsHost, state, layerHandles);
                    }
                } catch (err) {
                    console.error(
                        'Failed to attach layers for collection ' + collection.id + ':',
                        err
                    );
                }
            });
        }

        // -- Footer: actions. Without map tiles there is nothing to zoom
        //    to on this map (it has no basemap): say so, and link to the
        //    collection's own page. --
        const footer = document.createElement('div');
        footer.className = 'card-footer';
        if (hasTiles && collection.spatial_extent) {
            const zoomBtn = document.createElement('button');
            zoomBtn.type = 'button';
            zoomBtn.className = 'link';
            zoomBtn.textContent = 'Zoom to extent';
            zoomBtn.addEventListener('click', function () {
                fitMapToExtent(collection.spatial_extent);
            });
            footer.appendChild(zoomBtn);
        }
        if (!hasTiles) {
            const note = document.createElement('span');
            note.className = 'no-layer';
            note.textContent = 'No map layer';
            footer.appendChild(note);
            const page = samePageOriginPath(collection.page_url);
            if (page) {
                const pageLink = document.createElement('a');
                pageLink.className = 'link';
                pageLink.href = page;
                pageLink.textContent = 'Collection page';
                footer.appendChild(pageLink);
            }
        }
        if (footer.childNodes.length > 0) card.appendChild(footer);

        if (!checkbox) return li;

        // -- Wire toggle to layer visibility --
        checkbox.addEventListener('change', function () {
            state.enabled = checkbox.checked;
            card.classList.toggle('active', state.enabled);
            if (state.enabled && !state.hasZoomed && collection.spatial_extent) {
                // Reach the initial viewport before showing the source. Flying
                // with it visible requests intermediate zoom levels as well as
                // the final tiles, multiplying cold reads for large grids.
                fitMapToExtent(collection.spatial_extent, false);
                state.hasZoomed = true;
            }
            layerHandles.forEach(function (h) {
                h.setVisible(state.enabled);
            });
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

    function fitMapToExtent(extent, animate = true) {
        const south = Math.max(extent[1], -85);
        const north = Math.min(extent[3], 85);
        // West > east crosses the antimeridian: unwrap the east edge so the
        // fit spans the seam, not the rest of the globe.
        const east = extent[2] < extent[0] ? extent[2] + 360 : extent[2];
        const bounds = new maplibregl.LngLatBounds([extent[0], south], [east, north]);
        map.fitBounds(bounds, { padding: fitPadding(), maxZoom: 10, duration: 600, animate });
    }

    // A manifest URL as a path on this page's origin, like the tile
    // templates: the server's configured base can name another host
    // (127.0.0.1 vs localhost). Anything not a path is dropped.
    function samePageOriginPath(url) {
        if (typeof url !== 'string') return null;
        const path = url.replace(/^https?:\/\/[^/]+/i, '');
        return path.charAt(0) === '/' && path.charAt(1) !== '/' ? path : null;
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

    function temporalValues(temporal) {
        if (!temporal) return null;
        if (Array.isArray(temporal.values) && temporal.values.length > 0) return temporal.values;
        return null;
    }

    // The time axis of one parameter: its own `temporal_extent` when the
    // manifest carries one (a product that lags the others in the
    // collection), else the collection's — the union, which would offer
    // instants a lagging parameter doesn't have.
    function timeAxisFor(collection, parameter) {
        const own = Array.isArray(collection.parameters)
            ? collection.parameters.find(p => p.name === parameter)
            : undefined;
        const temporal = own && own.temporal_extent ? own.temporal_extent : collection.temporal_extent;
        return {
            values: temporalValues(temporal),
            defaultTime: temporal?.default || null
        };
    }

    // Longest time axis over the collection and its parameters: the slider
    // is built when any parameter can scrub, and hidden while the selected
    // one can't.
    function longestTimeAxis(collection) {
        let longest = (temporalValues(collection.temporal_extent) || []).length;
        (collection.parameters || []).forEach(function (p) {
            longest = Math.max(longest, (temporalValues(p.temporal_extent) || []).length);
        });
        return longest;
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
                opt.textContent = parameterLabel(p);
                if (p.name === state.parameter) opt.selected = true;
                paramSelect.appendChild(opt);
            });
            paramSelect.addEventListener('change', function () {
                if (state.parameter === paramSelect.value) return;
                state.parameter = paramSelect.value;
                if (state.rebindTimeAxis) {
                    state.rebindTimeAxis(timeAxisFor(collection, state.parameter));
                }
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

    // An option's text: the name and title once each, then the unit. A
    // title that already starts with the name ("DBZH — Reflectivity") or
    // ends with it in parentheses ("2 Metre Temperature (2t)") is shown
    // alone, not as "DBZH — DBZH — Reflectivity".
    function parameterLabel(p) {
        const title = p.title || '';
        let text;
        if (!title || title === p.name) {
            text = p.name;
        } else if (startsWithWord(title, p.name) || title.endsWith('(' + p.name + ')')) {
            text = title;
        } else {
            text = p.name + ' — ' + title;
        }
        const unit = p.unit ? ' (' + p.unit + ')' : '';
        return unit && !text.endsWith(unit) ? text + unit : text;
    }

    // `text` begins with `word` as a whole word: "t2m temperature" starts
    // with "t2m", "temperature" does not start with "t".
    function startsWithWord(text, word) {
        if (!word || !text.startsWith(word)) return false;
        return !/[A-Za-z0-9_]/.test(text.charAt(word.length));
    }

    // Convert an OGC API Tiles URL template
    //   /collections/{id}/map/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
    // into the form MapLibre raster sources understand
    //   /collections/{id}/map/tiles/WebMercatorQuad/{z}/{y}/{x}
    // Placeholder names come from the axum route (see preview.rs), not from
    // the generic `{tms}`/`{z}` which the server would reject.
    function tileUrlFor(collection, styleId, time, parameter) {
        const raster = collection.tiles.raster;
        // 'default' style uses the plain …/map/tiles route, not /styles/default/...
        // An RGB composite has only the default style.
        const useStyled = styleId && styleId !== 'default' && raster.styled_url_template
            && !isComposite(collection, parameter);
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
            tiles: [vectorTileUrlFor(collection)]
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
            // The preview draws vector tiles without a time: the server
            // encodes the collection's default features. (It filters them
            // by `datetime` when a request gives one, #794.)
            refreshForTime: function () {}
        };
    }

    function vectorTileUrlFor(collection) {
        let template = collection.tiles.vector.url_template;
        template = template.replace('{tileMatrixSetId}', 'WebMercatorQuad');
        template = template.replace('{tileMatrix}', '{z}');
        template = template.replace('{tileRow}', '{y}');
        template = template.replace('{tileCol}', '{x}');
        // MapLibre's vector-tile worker rejects relative URLs. Re-anchor to
        // page origin so 127.0.0.1↔localhost mismatch in `server.base_url`
        // doesn't trip CSP `connect-src 'self'`.
        template = template.replace(/^https?:\/\/[^/]+/i, '');
        return window.location.origin + template;
    }

    // An RGB composite parameter (#819): its colours come from its channels,
    // so it renders with the default style only.
    function isComposite(collection, parameter) {
        return Array.isArray(collection.parameters)
            && collection.parameters.some(function (p) {
                return p.name === parameter && p.composite === true;
            });
    }

    function appendTimeParam(template, time) {
        if (!time) return template;
        // Map tiles honour `datetime`; including it also keys MapLibre's
        // per-source HTTP cache by time. The preview sends none to vector
        // tiles, which then hold the collection's default features.
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

    function attachTimeSlider(collection, controlsHost, state, layerHandles) {
        let values = [];
        // Track the index that's been pushed to the source so we can skip a
        // no-op refresh when the user scrubs and returns to the same step
        // (each setTiles call re-fetches the visible tiles even with the
        // same URL).
        let appliedIndex = null;
        // Debounce handle: rapid release events on the slider collapse to
        // one server-side render burst instead of N piled-up bursts.
        let pendingRefresh = null;

        const row = document.createElement('div');
        row.className = 'time-slider';

        // A real label, and the formatted time as the value a screen reader
        // announces instead of the step index.
        sliderCount += 1;
        const inputId = 'time-slider-' + sliderCount;
        const label = document.createElement('label');
        label.className = 'control-label';
        label.htmlFor = inputId;
        label.textContent = 'Time';
        row.appendChild(label);

        const valueLabel = document.createElement('div');
        valueLabel.className = 'time-value';
        row.appendChild(valueLabel);

        const input = document.createElement('input');
        input.type = 'range';
        input.id = inputId;
        input.min = '0';
        input.step = '1';
        input.addEventListener('input', function () {
            state.timeIndex = parseInt(input.value, 10);
            showTime(values[state.timeIndex]);
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

        function showTime(iso) {
            const text = formatSliderTime(iso);
            valueLabel.textContent = text;
            input.setAttribute('aria-valuetext', text);
        }

        const ticks = document.createElement('div');
        ticks.className = 'time-ticks';
        const firstTick = document.createElement('span');
        const lastTick = document.createElement('span');
        ticks.appendChild(firstTick);
        ticks.appendChild(lastTick);
        row.appendChild(ticks);

        // Bind the slider to a time axis. `keepTime` (a parameter switch)
        // stays on the instant being viewed, or the axis's nearest to it, so
        // two products compare at one moment. Otherwise the engine-owned
        // default (alerts: now) is selected, with latest as the fallback for
        // forecast and older manifests. An axis with a single instant hides
        // the slider and leaves the time to the server's default.
        function bind(axis, keepTime) {
            if (pendingRefresh !== null) {
                clearTimeout(pendingRefresh);
                pendingRefresh = null;
            }
            values = axis.values || [];
            state.timeValues = values;
            if (values.length <= 1) {
                state.timeIndex = null;
                appliedIndex = null;
                row.hidden = true;
                return;
            }
            let index = keepTime ? nearestTimeIndex(values, keepTime) : -1;
            if (index < 0 && axis.defaultTime !== null) {
                index = values.findIndex(t => Date.parse(t) === Date.parse(axis.defaultTime));
            }
            state.timeIndex = index >= 0 ? index : values.length - 1;
            appliedIndex = state.timeIndex;
            input.max = String(values.length - 1);
            input.value = String(state.timeIndex);
            showTime(values[state.timeIndex]);
            firstTick.textContent = formatSliderTime(values[0]);
            lastTick.textContent = formatSliderTime(values[values.length - 1]);
            row.hidden = false;
        }

        bind(timeAxisFor(collection, state.parameter), null);
        state.rebindTimeAxis = function (axis) {
            bind(axis, currentTime(state));
        };
        controlsHost.appendChild(row);
    }

    function nearestTimeIndex(values, iso) {
        const target = Date.parse(iso);
        if (Number.isNaN(target)) return -1;
        let best = -1;
        let bestDistance = Infinity;
        values.forEach(function (t, i) {
            const distance = Math.abs(Date.parse(t) - target);
            if (distance < bestDistance) {
                best = i;
                bestDistance = distance;
            }
        });
        return best;
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
