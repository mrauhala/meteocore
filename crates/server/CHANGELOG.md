# Changelog

## [0.8.0](https://github.com/mrauhala/meteocore/compare/v0.7.0...v0.8.0) (2026-08-17)


### Features

* **api-edr:** CoverageJSON Point domain type for scattered events ([#505](https://github.com/mrauhala/meteocore/issues/505)) ([a7f452e](https://github.com/mrauhala/meteocore/commit/a7f452e))
* **api:** Cache-Control + ETag/If-None-Match on EDR and Features responses ([#555](https://github.com/mrauhala/meteocore/issues/555)) ([d4a5cf7](https://github.com/mrauhala/meteocore/commit/d4a5cf7))
* **api:** machine-readable legend JSON — WMS FORMAT=application/json, Maps/Tiles /legend ([#563](https://github.com/mrauhala/meteocore/issues/563)) ([074d883](https://github.com/mrauhala/meteocore/commit/074d883))
* **core,render:** bundles v2 — per-parameter styles in bundles, slot-wise merge ([#561](https://github.com/mrauhala/meteocore/issues/561)) ([646ea2f](https://github.com/mrauhala/meteocore/commit/646ea2f))
* **core,render:** style name defaults to colormap reference, title to palette title ([#569](https://github.com/mrauhala/meteocore/issues/569)) ([f616828](https://github.com/mrauhala/meteocore/commit/f61682850627154e40fd93ac7f2a3955cb86335b))
* **edr:** draw the PVOL cross-section lowest-beam coverage floor ([#514](https://github.com/mrauhala/meteocore/issues/514)) ([#581](https://github.com/mrauhala/meteocore/issues/581)) ([21ad6d8](https://github.com/mrauhala/meteocore/commit/21ad6d89f04c9b5d3489017e8d9c54dc826fda0f))
* **engine-nowcast:** cell intelligence — severity rank, tracks, deviant-mover flag ([#545](https://github.com/mrauhala/meteocore/issues/545)) ([0d68208](https://github.com/mrauhala/meteocore/commit/0d68208a269228028b21976defea82754024b1cb))
* **engine-nowcast:** growth/decay profile mechanism (off by default — scene-wide gate failed) ([#547](https://github.com/mrauhala/meteocore/issues/547)) ([ae4a20c](https://github.com/mrauhala/meteocore/commit/ae4a20c6e5105f00ceb5f4d70dc304bb429cde02))
* **engine-nowcast:** motion stabilization — multi-pair estimation + cross-generation EMA ([#530](https://github.com/mrauhala/meteocore/issues/530)) ([9e140f8](https://github.com/mrauhala/meteocore/commit/9e140f8))
* **engine-nowcast:** object-based verification harness + per-generation skill logging ([#543](https://github.com/mrauhala/meteocore/issues/543)) ([a0e7316](https://github.com/mrauhala/meteocore/commit/a0e7316d059b0de8290d9536eba316055bc83bfb))
* **engine-nowcast:** per-cell growth/decay tendencies (v2.3 iteration 1, draft) ([#548](https://github.com/mrauhala/meteocore/issues/548)) ([b9c2ab8](https://github.com/mrauhala/meteocore/commit/b9c2ab8))
* **engine-nowcast:** phase-0 motion + advection + skill core ([#525](https://github.com/mrauhala/meteocore/issues/525)) ([7e41a83](https://github.com/mrauhala/meteocore/commit/7e41a83))
* **engine-nowcast:** phase-1 derived-collection engine — WMS/Maps/Tiles serving ([#527](https://github.com/mrauhala/meteocore/issues/527)) ([b96fa44](https://github.com/mrauhala/meteocore/commit/b96fa446bc22c43b2bec9e5bc98bb14c852ea3f6))
* **engine-postgis:** age-colored lightning WMS/Maps/Tiles layer for the events shape ([#509](https://github.com/mrauhala/meteocore/issues/509)) ([353dc9c](https://github.com/mrauhala/meteocore/commit/353dc9c2314edce0d5e35214020d6ba70356b956))
* **engine-postgis:** events shape — EDR area queries for non-station event tables ([#506](https://github.com/mrauhala/meteocore/issues/506)) ([fe027af](https://github.com/mrauhala/meteocore/commit/fe027af))
* **engine-postgis:** response-value budget replaces the 500-station area cap ([#500](https://github.com/mrauhala/meteocore/issues/500)) ([08953ac](https://github.com/mrauhala/meteocore/commit/08953ac))
* **grafana:** Nowcast + EDR hot-path rows; memory & postgis panel upgrades ([#552](https://github.com/mrauhala/meteocore/issues/552)) ([9cdc42c](https://github.com/mrauhala/meteocore/commit/9cdc42c))
* **nowcast:** per-cell lightning join — flash rate + Schultz jump flag ([#550](https://github.com/mrauhala/meteocore/issues/550)) ([fef1e71](https://github.com/mrauhala/meteocore/commit/fef1e719aa331829607f760dc9797c71e8f94fde))
* **observability:** redesigned Grafana dashboard + PVOL pixel-cache eviction metrics ([#469](https://github.com/mrauhala/meteocore/issues/469), [#476](https://github.com/mrauhala/meteocore/issues/476)) ([#477](https://github.com/mrauhala/meteocore/issues/477)) ([39e5b8e](https://github.com/mrauhala/meteocore/commit/39e5b8e699577e13f31df8a011e45045f00b58ef))
* **render,core,server:** built-in per-parameter default styles ([#320](https://github.com/mrauhala/meteocore/issues/320)) ([#562](https://github.com/mrauhala/meteocore/issues/562)) ([11e36e4](https://github.com/mrauhala/meteocore/commit/11e36e4788b6f151abc53e8c5a00367622d2feee))
* **render:** GRLevelX / RadarScope .pal palette import ([#570](https://github.com/mrauhala/meteocore/issues/570)) ([7a4e219](https://github.com/mrauhala/meteocore/commit/7a4e219597ef0622d702d6f23309f82c185cd224))
* **render:** named Palette model + single builtin colormap table ([#558](https://github.com/mrauhala/meteocore/issues/558)) ([c97ddb1](https://github.com/mrauhala/meteocore/commit/c97ddb170dccaaae7edb1af2357f33b717bcd3cb))
* **render:** single StyleContext resolver, styles built once for WMS/Maps/Tiles/EDR ([#559](https://github.com/mrauhala/meteocore/issues/559)) ([e201bca](https://github.com/mrauhala/meteocore/commit/e201bca2a4083c1aef36529769c575b415bc6a68))
* **server:** extend the filesystem watcher to colormaps_dir ([#571](https://github.com/mrauhala/meteocore/issues/571)) ([#579](https://github.com/mrauhala/meteocore/issues/579)) ([d43d920](https://github.com/mrauhala/meteocore/commit/d43d92069dd8a821ba42f8c4b02214f12f21cd8a))
* **server:** user-defined colormaps — [[colormaps]], colormaps_dir, cpt/GDAL/SLD import ([#560](https://github.com/mrauhala/meteocore/issues/560)) ([361d692](https://github.com/mrauhala/meteocore/commit/361d6923b45b4919476f86a40d11c5c28c0662fc))


### Bug Fixes

* **api-edr:** 500 instead of request-path panic on registry divergence ([#479](https://github.com/mrauhala/meteocore/issues/479)) ([#483](https://github.com/mrauhala/meteocore/issues/483)) ([1e7929c](https://github.com/mrauhala/meteocore/commit/1e7929c))
* **api-edr:** breached postgis area/row caps are HTTP 400, not opaque 500 ([#497](https://github.com/mrauhala/meteocore/issues/497)) ([6fda38e](https://github.com/mrauhala/meteocore/commit/6fda38e))
* **api-features:** emit items timeStamp at seconds precision with Z suffix ([#556](https://github.com/mrauhala/meteocore/issues/556)) ([673859d](https://github.com/mrauhala/meteocore/commit/673859d))
* **api:** 400 on unknown parameter-name in Maps/Tiles legend endpoints ([#568](https://github.com/mrauhala/meteocore/issues/568)) ([b7dff62](https://github.com/mrauhala/meteocore/commit/b7dff62))
* **api:** key rendered/meta-tile caches on the concrete latest run, not None ([#526](https://github.com/mrauhala/meteocore/issues/526)) ([99e7c6a](https://github.com/mrauhala/meteocore/commit/99e7c6a))
* **api:** per-parameter styles reach Maps/Tiles, WMS legends and GetCapabilities ([#566](https://github.com/mrauhala/meteocore/issues/566)) ([7a117b6](https://github.com/mrauhala/meteocore/commit/7a117b6))
* **ci:** cut release tags deterministically from the manifest ([#220](https://github.com/mrauhala/meteocore/issues/220)) ([#489](https://github.com/mrauhala/meteocore/issues/489)) ([ee14d1f](https://github.com/mrauhala/meteocore/commit/ee14d1f))
* **engine-nowcast:** emit cell-feature values at meaningful precision ([#554](https://github.com/mrauhala/meteocore/issues/554)) ([effdb0f](https://github.com/mrauhala/meteocore/commit/effdb0f))
* **engine-nowcast:** speed-based track gates + velocity clamp — kills 200 km/h phantom cells ([#553](https://github.com/mrauhala/meteocore/issues/553)) ([f019bb0](https://github.com/mrauhala/meteocore/commit/f019bb0))
* **engine-odim:** clear air is a measurement — z-pinned EDR series no longer 404s ([#495](https://github.com/mrauhala/meteocore/issues/495)) ([b8c4b34](https://github.com/mrauhala/meteocore/commit/b8c4b34))
* **engine-postgis:** surface the real DB error in metadata refresh ([#436](https://github.com/mrauhala/meteocore/issues/436)) ([#484](https://github.com/mrauhala/meteocore/issues/484)) ([94af4d1](https://github.com/mrauhala/meteocore/commit/94af4d1))
* **engines:** emit feature-property timestamps at seconds precision with Z suffix ([#557](https://github.com/mrauhala/meteocore/issues/557)) ([53aa465](https://github.com/mrauhala/meteocore/commit/53aa465))
* **render:** key raster caches on the RESOLVED timestep, not the requested time ([#508](https://github.com/mrauhala/meteocore/issues/508)) ([67c62f3](https://github.com/mrauhala/meteocore/commit/67c62f3))
* **render:** pal values below the lowest entry render transparent ([#572](https://github.com/mrauhala/meteocore/issues/572)) ([7c848c0](https://github.com/mrauhala/meteocore/commit/7c848c0))
* geo/XML safety tripwire in CI + three leftover Web Mercator copies ([#482](https://github.com/mrauhala/meteocore/issues/482)) ([#485](https://github.com/mrauhala/meteocore/issues/485)) ([61e1f2f](https://github.com/mrauhala/meteocore/commit/61e1f2f))


### Performance Improvements

* **engine-nowcast:** O(leads) trajectory integration — 6-12× faster generations ([#529](https://github.com/mrauhala/meteocore/issues/529)) ([2b9c03c](https://github.com/mrauhala/meteocore/commit/2b9c03c))
* **render:** pixel-proportional meta-tile budget instead of fixed 256-tile cap ([#491](https://github.com/mrauhala/meteocore/issues/491)) ([#492](https://github.com/mrauhala/meteocore/issues/492)) ([db87f90](https://github.com/mrauhala/meteocore/commit/db87f90f976e7cc5cb7f46fa5284c8813777b20e))
* **server:** incremental reload — rebuild only changed collections, keep unchanged engines live ([#576](https://github.com/mrauhala/meteocore/issues/576)) ([26ca8c6](https://github.com/mrauhala/meteocore/commit/26ca8c6ec387e0503896a8faf1763ffbbd407991))
* **server:** jemalloc global allocator + process/allocator memory gauges ([#493](https://github.com/mrauhala/meteocore/issues/493)) ([#494](https://github.com/mrauhala/meteocore/issues/494)) ([eab5447](https://github.com/mrauhala/meteocore/commit/eab5447ca869ef1b8c15085bc1f91a0cade1c9d7))


## [0.7.0](https://github.com/mrauhala/meteocore/compare/v0.6.0...v0.7.0) (2026-07-05)


### Features

* **3dtiles:** echo-top API representation + viewer toggle + reflectivity-scaled point size ([#370](https://github.com/mrauhala/meteocore/issues/370)) ([a30c4cf](https://github.com/mrauhala/meteocore/commit/a30c4cfee8e098242d936d3d68d6ae8556bd2f20))
* **api-3dtiles:** OGC 3D Tiles HTTP service ([#349](https://github.com/mrauhala/meteocore/issues/349)) ([#354](https://github.com/mrauhala/meteocore/issues/354)) ([4fc6053](https://github.com/mrauhala/meteocore/commit/4fc605394a6f9214270209b662b851854cf14dbb))
* **config:** per-collection keywords + license across all APIs ([#324](https://github.com/mrauhala/meteocore/issues/324)) ([f704cba](https://github.com/mrauhala/meteocore/commit/f704cbad8c2f2b4db05edb4f6b85c59c423ffa43))
* **ds-core,engine-odim:** storm-cell extraction + tracking core ([#367](https://github.com/mrauhala/meteocore/issues/367), 1/4) ([#404](https://github.com/mrauhala/meteocore/issues/404)) ([d926905](https://github.com/mrauhala/meteocore/commit/d9269056e8a18472c909e3841f98689e57aaa3d8))
* **edr:** model-run support — EDR instances + shared run machinery ([#337](https://github.com/mrauhala/meteocore/issues/337)) ([#338](https://github.com/mrauhala/meteocore/issues/338)) ([5491687](https://github.com/mrauhala/meteocore/commit/549168760880381af460c7ef0071bd3d865b0485))
* **engine-cap:** CAP v1.2 alert engine — Features + vector→raster WMS/Maps/Tiles ([#396](https://github.com/mrauhala/meteocore/issues/396)) ([#430](https://github.com/mrauhala/meteocore/issues/430)) ([26f11b9](https://github.com/mrauhala/meteocore/commit/26f11b9d9454b8df4595d712f15262eb0b102bd8))
* **engine-postgis:** background metadata refresh loop ([#110](https://github.com/mrauhala/meteocore/issues/110)) ([#441](https://github.com/mrauhala/meteocore/issues/441)) ([66d4afc](https://github.com/mrauhala/meteocore/commit/66d4afc4b2223ea5d9b3318fcf49666df53c7cf5))
* **engine-postgis:** live health monitoring + ops metrics ([#110](https://github.com/mrauhala/meteocore/issues/110)) ([#445](https://github.com/mrauhala/meteocore/issues/445)) ([225b2d4](https://github.com/mrauhala/meteocore/commit/225b2d4ecc44a9bcd60892c0bc5f9074aa190b2f))
* **engine-zarr:** Icechunk support (read-only, feature-gated) ([#335](https://github.com/mrauhala/meteocore/issues/335)) ([#336](https://github.com/mrauhala/meteocore/issues/336)) ([815b64a](https://github.com/mrauhala/meteocore/commit/815b64ac76b00de1551a2cdc375e02cc4cabafea))
* **engine-zarr:** WMS/Maps/Tiles rendering — Phase 3 ([#125](https://github.com/mrauhala/meteocore/issues/125)) ([#334](https://github.com/mrauhala/meteocore/issues/334)) ([23c3307](https://github.com/mrauhala/meteocore/commit/23c3307f5d91b381dc870b38dc47d7ba9501e1a4))
* **engine-zarr:** Zarr V2/V3 engine — Phase 1 local EDR ([#125](https://github.com/mrauhala/meteocore/issues/125)) ([#332](https://github.com/mrauhala/meteocore/issues/332)) ([88de7e9](https://github.com/mrauhala/meteocore/commit/88de7e908d891cdc582bc41a5e5c71b02822cd21))
* **server,ds-core:** reverse-proxy base URL detection (trust_proxy_headers) ([#12](https://github.com/mrauhala/meteocore/issues/12)) ([#415](https://github.com/mrauhala/meteocore/issues/415)) ([d4b9d8e](https://github.com/mrauhala/meteocore/commit/d4b9d8e05ae8ff9412dadcd2ed9f58354765eb72))
* **server:** auto-collection mode (--auto-collections &lt;dir&gt;) — phase 1 ([#411](https://github.com/mrauhala/meteocore/issues/411)) ([#413](https://github.com/mrauhala/meteocore/issues/413)) ([9006414](https://github.com/mrauhala/meteocore/commit/9006414c2f76702798dd22a15c0a48c6037692a9))
* **server:** CLI startup overrides (--host/--port/--config) + no-config auto-port boot ([#412](https://github.com/mrauhala/meteocore/issues/412)) ([9bcc43d](https://github.com/mrauhala/meteocore/commit/9bcc43d1e251cc47898e293ed0541f95a622fdfe))


### Bug Fixes

* **engine-odim,ds-render:** neutral, connected storm-cell track trails ([#367](https://github.com/mrauhala/meteocore/issues/367)) ([#409](https://github.com/mrauhala/meteocore/issues/409)) ([71c5f91](https://github.com/mrauhala/meteocore/commit/71c5f915d0dc899afd79298ff7ef231b303548c3))
* **server:** collections_dir watcher ignores read events — stops self-reload loop ([#424](https://github.com/mrauhala/meteocore/issues/424)) ([#425](https://github.com/mrauhala/meteocore/issues/425)) ([48316b7](https://github.com/mrauhala/meteocore/commit/48316b7a8b3fdf4d0593dabd4f80f4123b19e127))


### Performance Improvements

* **3dtiles:** content + voxel-grid caches — cached repeats 165× faster ([#378](https://github.com/mrauhala/meteocore/issues/378)) ([9d1e00f](https://github.com/mrauhala/meteocore/commit/9d1e00f995acf8d40e6bc730f786ba0d664b5b0f))
* **engine-geotiff:** decoded-chunk cache for local sources ([#463](https://github.com/mrauhala/meteocore/issues/463)) ([#467](https://github.com/mrauhala/meteocore/issues/467)) ([1bf82bb](https://github.com/mrauhala/meteocore/commit/1bf82bb70235f66d74d9bd98f1c01a8e8308724a))
* **engine-odim COMP:** process-global multi-entry composite LRU ([#212](https://github.com/mrauhala/meteocore/issues/212)) ([#419](https://github.com/mrauhala/meteocore/issues/419)) ([7719780](https://github.com/mrauhala/meteocore/commit/7719780cb8e0ea08834b3f1f6eeb76365e1792e1))
* **server:** reload preserves the warm render caches instead of rebuilding them (closes [#421](https://github.com/mrauhala/meteocore/issues/421)) ([#422](https://github.com/mrauhala/meteocore/issues/422)) ([71d6fed](https://github.com/mrauhala/meteocore/commit/71d6fedae98e4cc40a8a90c3618c565cfd54b420))

## [0.6.0](https://github.com/mrauhala/meteocore/compare/v0.5.1...v0.6.0) (2026-06-04)


### Features

* **api:** align OGC API Maps/Tiles collection metadata with the spec ([#261](https://github.com/mrauhala/meteocore/issues/261)) ([579773f](https://github.com/mrauhala/meteocore/commit/579773f546b9153f1e7410abb20e88554e9be52d))
* **engine-odim:** human-readable PVOL labels + site-prefixed WMS layer titles ([#315](https://github.com/mrauhala/meteocore/issues/315)) ([1c4cfee](https://github.com/mrauhala/meteocore/commit/1c4cfee74cb09c2cb35ad13a8b18db08524d1432))
* **engine-odim:** per-site PVOL collections (model B); param = bare quantity ([#282](https://github.com/mrauhala/meteocore/issues/282)) ([55c4b4f](https://github.com/mrauhala/meteocore/commit/55c4b4f5b4d89ae11be39bbeaa015b5d2fa0740c))
* **engine-odim:** radar sites as an OGC API - Features collection ([#285](https://github.com/mrauhala/meteocore/issues/285)) ([#316](https://github.com/mrauhala/meteocore/issues/316)) ([8c9a2ad](https://github.com/mrauhala/meteocore/commit/8c9a2adf2b143fd5ab913ecfc3bc53e52d9618e0))
* **server:** watch collections_dir and auto-reload on changes ([#318](https://github.com/mrauhala/meteocore/issues/318)) ([#319](https://github.com/mrauhala/meteocore/issues/319)) ([fd946aa](https://github.com/mrauhala/meteocore/commit/fd946aa7e9ee8f597027cc1f1f0c7227e1685397))


### Bug Fixes

* **server:** add WMS latency histogram buckets between 1s and 5s ([#230](https://github.com/mrauhala/meteocore/issues/230)) ([f750a2e](https://github.com/mrauhala/meteocore/commit/f750a2e8e100de17b181733c7060ff3c808af0f3))


### Performance Improvements

* **engine-odim:** lazy PVOL pixel loading — bounded RAM, non-blocking scan ([#290](https://github.com/mrauhala/meteocore/issues/290)) ([dce9cb3](https://github.com/mrauhala/meteocore/commit/dce9cb33b23dce0cde33def67be291631a240a2a))
* **render,api-wms:** internal meta-tiling for Web Mercator WMS GetMap ([#202](https://github.com/mrauhala/meteocore/issues/202)) ([#235](https://github.com/mrauhala/meteocore/issues/235)) ([aee7d5b](https://github.com/mrauhala/meteocore/commit/aee7d5b52ac2b57c2fa5ce165ccdc0d5bf48cc98))
* **server:** wire IntegerLutColorMap into the WMS/Maps/Tiles colorize path ([#250](https://github.com/mrauhala/meteocore/issues/250)) ([ff0d459](https://github.com/mrauhala/meteocore/commit/ff0d459121d51f894291d98a52e6effae4d42628))

## [0.5.1](https://github.com/mrauhala/meteocore/compare/v0.5.0...v0.5.1) (2026-05-24)


### Performance Improvements

* **server,engine-grib:** isolate poll loops from request runtime + skip settled GRIB runs ([#221](https://github.com/mrauhala/meteocore/issues/221)) ([#226](https://github.com/mrauhala/meteocore/issues/226)) ([1e5fb8d](https://github.com/mrauhala/meteocore/commit/1e5fb8dcb3376b9ba3a07d9b587bab62a2d34c78))

## [0.5.0](https://github.com/mrauhala/meteocore/compare/v0.4.0...v0.5.0) (2026-05-19)


### Features

* vertical (elevation/level) dimension for MapEngine + EdrEngine ([#200](https://github.com/mrauhala/meteocore/issues/200)) ([cbd4abd](https://github.com/mrauhala/meteocore/commit/cbd4abd8becb3085ccdf0a1ae780fa814f0b4f75))


### Performance Improvements

* **engine-geotiff:** coarse-grid projection for raster resampling — replaces per-pixel CRS projection in the WMS/Maps/Tiles resampler; ~10× faster TM35FIN renders (68.3 ms → 6.8 ms for a 1024² tile) ([#214](https://github.com/mrauhala/meteocore/issues/214))

## [0.4.0](https://github.com/mrauhala/meteocore/compare/v0.3.0...v0.4.0) (2026-05-18)


### Features

* **engine-odim:** EDR support for the odim-volume engine (M3a) ([#199](https://github.com/mrauhala/meteocore/issues/199)) ([fae2169](https://github.com/mrauhala/meteocore/commit/fae2169300c393768c5da7a294fd4f3fa3c0d58a))
* **engine-odim:** PVOL polar-volume reader + Cartesian display ([#187](https://github.com/mrauhala/meteocore/issues/187)) ([4cb87aa](https://github.com/mrauhala/meteocore/commit/4cb87aa7ac908b2863f3825f42e7db4e0e153ede))
* **engine-odim:** S3 object-store source (Phase 2) ([#182](https://github.com/mrauhala/meteocore/issues/182)) ([9a54d03](https://github.com/mrauhala/meteocore/commit/9a54d0334056daedbd392d10965ddfb2171a2e1d))


### Bug Fixes

* **server:** bind listen port before loading collections ([#191](https://github.com/mrauhala/meteocore/issues/191)) ([2eab30c](https://github.com/mrauhala/meteocore/commit/2eab30c3a02c3d4cc8cf43292e965aa0c64cb9e6))

## [0.3.0](https://github.com/mrauhala/meteocore/compare/v0.2.0...v0.3.0) (2026-05-15)


### Features

* **engine-odim:** EdrEngine — position + area queries (Phase 1.5) ([#177](https://github.com/mrauhala/meteocore/issues/177)) ([cbcd017](https://github.com/mrauhala/meteocore/commit/cbcd017f031aa0dd02137bb809b2f873d1bbd7c3))
* **engine-odim:** ODIM_H5 weather radar engine (Phase 1, MapEngine) ([#176](https://github.com/mrauhala/meteocore/issues/176)) ([253bf54](https://github.com/mrauhala/meteocore/commit/253bf54f9c1989c7d7ee4dcab88a400339c3cf85))
* **preview:** parameter dropdown + bounded time slider ([#157](https://github.com/mrauhala/meteocore/issues/157)) ([7cd874e](https://github.com/mrauhala/meteocore/commit/7cd874e6f720eaad7d8fff2691f08e02e2bea684))

## [0.2.0](https://github.com/mrauhala/meteocore/compare/v0.1.0...v0.2.0) (2026-05-12)


### Features

* add base_url config for absolute links in all API responses ([b13d27c](https://github.com/mrauhala/meteocore/commit/b13d27c728b622ff456496512dd67ec90e248a7a))
* add collection ID and file sizes to log messages ([45e1967](https://github.com/mrauhala/meteocore/commit/45e1967af24f89abfc57e11f6a905f65477073c2))
* add comprehensive Prometheus metrics and reorganize Grafana dashboard ([35a1bca](https://github.com/mrauhala/meteocore/commit/35a1bca01e66558f91e8ef24744b0e7cda2e4543))
* add data staleness tracking to querydata engine ([4a583a1](https://github.com/mrauhala/meteocore/commit/4a583a1d26b38c49d5babaef6321a788d9f4e2d7))
* add dynamic reload, health endpoint, and Prometheus metrics ([e93ec9e](https://github.com/mrauhala/meteocore/commit/e93ec9e1169364d1a5b5e122c02a13d9baf856d4))
* add GeoJSON engine with multi-collection support ([7f6f11b](https://github.com/mrauhala/meteocore/commit/7f6f11b5205f83548a010b189304def4c39e0e5f))
* add GeoTIFF engine with directory polling for raster data ([87a8453](https://github.com/mrauhala/meteocore/commit/87a8453961d5a1e81c79cd86d98e1f89912cd575))
* add GRIB engine for NWP forecast data ([77ce672](https://github.com/mrauhala/meteocore/commit/77ce672afa9f9bfcda9ba4442b52a2f8d9c651bb))
* add GRIB engine for NWP forecast data ([d13f968](https://github.com/mrauhala/meteocore/commit/d13f96848b1f01a2a158016fe10cf81d627b6437)), closes [#53](https://github.com/mrauhala/meteocore/issues/53)
* add load shedding, response compression, and conditional requests ([9017e63](https://github.com/mrauhala/meteocore/commit/9017e6319d34cf14d2fda584d8c300c3aeab6534))
* add OGC API - Features as separate service alongside EDR ([6ba5fbd](https://github.com/mrauhala/meteocore/commit/6ba5fbd2afbcd4657d976b1576768d003565632e))
* add OGC API Tiles endpoint with TileMatrixSet support ([00914ef](https://github.com/mrauhala/meteocore/commit/00914ef002fc48ac17cc3a3eb54a83afa970b0f0))
* add OpenAPI definitions and Swagger UI for EDR, Features, and Maps APIs ([8b7fa24](https://github.com/mrauhala/meteocore/commit/8b7fa24b0b7f56eb05cb25d4b6635be884e3ff60))
* add separate S3 config and dynamic date-based prefix pattern ([9107c66](https://github.com/mrauhala/meteocore/commit/9107c66f8894fbca03db0fbcc59457eabfaaee2c))
* add structured request logging middleware ([d2f3c51](https://github.com/mrauhala/meteocore/commit/d2f3c5111a52ca7354bfa30137e5d62bfd8865f3))
* add structured request logging middleware ([d1dd93a](https://github.com/mrauhala/meteocore/commit/d1dd93a4b652b5d538974858db25d306ac68a1b3))
* add temporal_start/temporal_end to health endpoint ([cc9108a](https://github.com/mrauhala/meteocore/commit/cc9108a3005f016dc8e48eff8ee25b9d2b371967))
* add WMS 1.3.0 support with COG overview rendering ([9d7384f](https://github.com/mrauhala/meteocore/commit/9d7384fe0a1696bb8e5cb82a35eb2d7ac5b2d212))
* **api-tiles:** MVT route via ?f=mvt content negotiation ([#127](https://github.com/mrauhala/meteocore/issues/127) Phase 2) ([bf9225b](https://github.com/mrauhala/meteocore/commit/bf9225b9441fe88f9e868cd61a13b95b478c6932))
* **api-tiles:** MVT route via ?f=mvt content negotiation ([#127](https://github.com/mrauhala/meteocore/issues/127) Phase 2) ([cb1133f](https://github.com/mrauhala/meteocore/commit/cb1133f69f1aa9790264d2da9c98506658f8bbaa))
* **config:** support collections_dir for per-file collection configs ([8acf5c7](https://github.com/mrauhala/meteocore/commit/8acf5c723be9560f667d2fea32d32b8b78e07ab3)), closes [#87](https://github.com/mrauhala/meteocore/issues/87)
* EDR area query with exact polygon clipping ([fb88d85](https://github.com/mrauhala/meteocore/commit/fb88d8566f91f07181e8283c209ec55e1aebce02))
* EDR-style temporal extent in health endpoint ([087a8fb](https://github.com/mrauhala/meteocore/commit/087a8fb3daf9a55532659ef5dbb9676b95b11f25))
* **engine-grib:** NOAA GFS support with source-unit-driven labels ([e01d2a2](https://github.com/mrauhala/meteocore/commit/e01d2a2f63b79b03be542878b2a89157b84bcf95))
* **engine-grib:** NOAA GFS support with source-unit-driven labels ([f938902](https://github.com/mrauhala/meteocore/commit/f9389028ca3392f721ff2184821634eb6d942cdc))
* GRIB rendering, health, and polling improvements ([41c83ee](https://github.com/mrauhala/meteocore/commit/41c83eea48954e0b0ee9efe3051679a92076deb9))
* initial metocean data server with OGC EDR API ([9db2883](https://github.com/mrauhala/meteocore/commit/9db2883f8057a9d8c14552455f4e7fff8f43c713))
* JSON logging and X-Request-ID correlation ([b4e040f](https://github.com/mrauhala/meteocore/commit/b4e040f1ead5c04aba9d28ef16af4ae4a159c176))
* JSON logging and X-Request-ID correlation ([a0b4c9e](https://github.com/mrauhala/meteocore/commit/a0b4c9e1cf7ccdb7cbda0271f50724bbf895638e))
* OGC API Maps Phase 3 — api-maps crate with JSON endpoints ([868ee2d](https://github.com/mrauhala/meteocore/commit/868ee2dfcc335f7c652ed0e0fe0da89eda252fb7))
* per-collection cache metrics and utilization gauges ([61edf05](https://github.com/mrauhala/meteocore/commit/61edf057e33eb3a23ea08e95264c05aef0ed797a))
* per-collection cache metrics and utilization gauges ([204fd4f](https://github.com/mrauhala/meteocore/commit/204fd4fde535c4e5f30495042ac8b3f4cc37db38))
* per-parameter default colormaps and precipitation_rate colormap ([5f6b4c9](https://github.com/mrauhala/meteocore/commit/5f6b4c92df3be3ce77b20a9cb711b79b5a592bb4))
* **preview:** embedded MapLibre SPA at /preview ([#126](https://github.com/mrauhala/meteocore/issues/126) Phase 2) ([#133](https://github.com/mrauhala/meteocore/issues/133)) ([a24a64b](https://github.com/mrauhala/meteocore/commit/a24a64bf97f9ce55980808343abdeddf9b842327))
* **preview:** manifest.json — unified discovery for the UI ([#126](https://github.com/mrauhala/meteocore/issues/126) Phase 1) ([#132](https://github.com/mrauhala/meteocore/issues/132)) ([ada14fe](https://github.com/mrauhala/meteocore/commit/ada14fe230b23c651428f568f4f200bcfbf23112))
* **preview:** raster layer rendering + style picker ([#126](https://github.com/mrauhala/meteocore/issues/126) Phase 3) ([#134](https://github.com/mrauhala/meteocore/issues/134)) ([3da360b](https://github.com/mrauhala/meteocore/commit/3da360be450d350373b2293e461203d232bef81f))
* **preview:** time slider + polished cards + opt-in layers ([#126](https://github.com/mrauhala/meteocore/issues/126) Phase 5) ([#137](https://github.com/mrauhala/meteocore/issues/137)) ([071e439](https://github.com/mrauhala/meteocore/commit/071e43908a05092a923ed78e35f9be095dd941f0))
* **preview:** vector tile layers + click popups ([#126](https://github.com/mrauhala/meteocore/issues/126) Phase 4) ([#135](https://github.com/mrauhala/meteocore/issues/135)) ([b39ddd5](https://github.com/mrauhala/meteocore/commit/b39ddd5d3035000bf18808503dab50f664743073))
* security hardening — admin auth, metrics fix, config validation, CORS ([aaa9e68](https://github.com/mrauhala/meteocore/commit/aaa9e6881b7c61a3cab829dfb5a81b348628708e))
* **server:** log error reason on 4xx/5xx responses ([e9b4520](https://github.com/mrauhala/meteocore/commit/e9b452028001240b4cf439e6b6cc62d1ff47fc04))
* **server:** wire engine-postgis into load/reload path ([#109](https://github.com/mrauhala/meteocore/issues/109)) ([9dd256b](https://github.com/mrauhala/meteocore/commit/9dd256bc38387edede6aa7a75a448a8e558cbbe1))
* style-to-parameter mapping for multi-parameter WMS rendering ([346dbe3](https://github.com/mrauhala/meteocore/commit/346dbe3552acb76d4c4e4b0e63073ee3cff71624))
* support collections_dir for per-file collection configs ([c3efaa5](https://github.com/mrauhala/meteocore/commit/c3efaa55558f770331e723c54b602042cdc40ca8))
* tier 3 robustness — backoff, poison recovery, COG logging, staleness ([939b0d0](https://github.com/mrauhala/meteocore/commit/939b0d063e2441608ef0a11b70b51cb120bb4120))
* wire querydata engine into server with poll loops ([c63000b](https://github.com/mrauhala/meteocore/commit/c63000b370b50c667ec77b5783f5e651c81665cf))
* WMS Phase 2 — styles, JPEG, legends, new colormaps ([03ddd4b](https://github.com/mrauhala/meteocore/commit/03ddd4bd1795a20cebc04976254b32b551f21b02))
* **wms:** add shared style bundles referenced by collections ([aaa0c93](https://github.com/mrauhala/meteocore/commit/aaa0c9321c019d82a9803a90839514a33ddaaefd)), closes [#95](https://github.com/mrauhala/meteocore/issues/95)


### Bug Fixes

* address PR review feedback ([845e1f3](https://github.com/mrauhala/meteocore/commit/845e1f30e6e2965ff99abbf59c611b2006ed270b))
* address review findings (perf, architecture, validation) ([46be06c](https://github.com/mrauhala/meteocore/commit/46be06c85146ac29a9e8bba8826abb59fdf64775))
* address review findings from comprehensive codebase review ([a8041ab](https://github.com/mrauhala/meteocore/commit/a8041ab27b1538551bf6443a3cdfb23bb7469262))
* cargo fmt formatting in main.rs ([ddb0390](https://github.com/mrauhala/meteocore/commit/ddb039041357c0a19fd4c9e35bbcfc5c933da433))
* **ci:** revert workspace.package inheritance for release-please ([#140](https://github.com/mrauhala/meteocore/issues/140)) ([165805e](https://github.com/mrauhala/meteocore/commit/165805e84de0a821419af5f1168d935ec7af36cf))
* **config:** reject duplicate extra names; resolve bundle once per collection ([25796c0](https://github.com/mrauhala/meteocore/commit/25796c0593de1d94f9bd4d7928d8334c5c4a45b3))
* critical review items — safety, shutdown, observability ([d401ad0](https://github.com/mrauhala/meteocore/commit/d401ad009e4f361acf7026ca3fe99d5ac18fb72a))
* **engine-postgis:** PR review round 3 ([93f4961](https://github.com/mrauhala/meteocore/commit/93f49615396f4bf7f22362c17319fb7b34f94ba7))
* graceful degradation on collection load failures ([b2fcf38](https://github.com/mrauhala/meteocore/commit/b2fcf38afc5d9bca5df8082290f4a20de8bd223b))
* move CORS layer to outermost position so all routes get headers ([7875351](https://github.com/mrauhala/meteocore/commit/78753513b3d26317a4e65d2269a863066e2a8a07))
* only return 503 when all collections have failed ([7011113](https://github.com/mrauhala/meteocore/commit/7011113e7b6b481db20f1ea897b86469c9d3ade4))
* only return 503 when all collections have failed ([3086b2b](https://github.com/mrauhala/meteocore/commit/3086b2bf03d6bed90e78940f2dc0f56f2bfe41a0))
* populate http_response_bytes_total from body size hint ([4b55795](https://github.com/mrauhala/meteocore/commit/4b557958a9e004fba6a79e23e73db22b444f00d0))
* populate http_response_bytes_total from body size hint ([974f264](https://github.com/mrauhala/meteocore/commit/974f2649496b0669c2c62348805691e76da21639))
* **pr-117:** address review feedback ([f0de6f1](https://github.com/mrauhala/meteocore/commit/f0de6f1870bf106b3d1feabab8983d87bb75d994))
* **pr-117:** redact WMS internal errors, dedupe log arms ([a78c50e](https://github.com/mrauhala/meteocore/commit/a78c50ea16947053555bc7b07e31f048319265c3))
* use shared rayon pool and serialize reload requests ([81291e5](https://github.com/mrauhala/meteocore/commit/81291e59c3c4cac64dccf6384027bd0db0c72e85))
* WMS trailing slash routing and landing page link ([0f47063](https://github.com/mrauhala/meteocore/commit/0f4706363b5283a8de43cf74eb91cd05ede9ec74))
* **wms:** block style_bundles in per-collection files; document incompatibilities ([7e23614](https://github.com/mrauhala/meteocore/commit/7e236149a02c7be044899d4c72c3ef01bd2910e6))
* **wms:** reject empty parameter on extras; warn on unresolved bundle ref ([6b19424](https://github.com/mrauhala/meteocore/commit/6b19424bafb7791d485bb007cee67447e549ed44))
* **wms:** scope bundle extras by parameter; cover resolve_bundle fallback ([158a31b](https://github.com/mrauhala/meteocore/commit/158a31b346f0aaa0ecf32d3b9299410ffa575660))


### Performance Improvements

* **render:** bump shared render semaphore to 2× cores (min 8) ([#148](https://github.com/mrauhala/meteocore/issues/148)) ([f6377e4](https://github.com/mrauhala/meteocore/commit/f6377e467d6f18504413b551201f4968917c5330))
