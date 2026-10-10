# Changelog

## [0.11.0](https://github.com/mrauhala/meteocore/compare/v0.10.0...v0.11.0) (2026-10-10)


### Features

* **edr:** repeating datetime interval Rn/date-time/duration ([#957](https://github.com/mrauhala/meteocore/issues/957)) ([c3970f4](https://github.com/mrauhala/meteocore/commit/c3970f43bcc91ccebc08f17edc05df76f237c8c2))
* **wind:** derive wind speed and direction from u/v components ([#960](https://github.com/mrauhala/meteocore/issues/960)) ([1b9b2cc](https://github.com/mrauhala/meteocore/commit/1b9b2cc9c45d2c961296b06fb89c0b2826804a57))
* **edr:** HTML for every data query, the location list and items ([#978](https://github.com/mrauhala/meteocore/issues/978)) ([83ad8a8](https://github.com/mrauhala/meteocore/commit/83ad8a89e8f3efe2992be9bbd8ba94d75ae08e71))
* **edr:** HTML pages hold all their JSON information and link members ([#987](https://github.com/mrauhala/meteocore/issues/987)) ([1d4aa0d](https://github.com/mrauhala/meteocore/commit/1d4aa0dcab3cc264809ad1cf6f39cf085450d5ab))
* **edr:** declare the EDR 1.2 conformance classes ([#981](https://github.com/mrauhala/meteocore/issues/981)) ([9800188](https://github.com/mrauhala/meteocore/commit/98001888363b1e97fafd21a1824021f69b4dfb32))
* **engine-cap:** persist the WIS2 alert set across restarts ([#1031](https://github.com/mrauhala/meteocore/issues/1031)) ([733ba56](https://github.com/mrauhala/meteocore/commit/733ba5624a8cb2ef666949139ddd0b3afec5b254))
* **engine-bufr:** persist the WIS2 report store across restarts ([#1035](https://github.com/mrauhala/meteocore/issues/1035)) ([70fc760](https://github.com/mrauhala/meteocore/commit/70fc760672eb073293ffe11fe66083efb1e28c59))


### Bug Fixes

* **edr:** filter /locations by bbox and datetime before paging ([#959](https://github.com/mrauhala/meteocore/issues/959)) ([2b4f3a2](https://github.com/mrauhala/meteocore/commit/2b4f3a295d2c3ce5ce67f04653f1819bc4854e25))
* **edr:** z returns only intersecting levels and caps z lists ([#973](https://github.com/mrauhala/meteocore/issues/973)) ([b885913](https://github.com/mrauhala/meteocore/commit/b88591312ab40675f038308f4ac633ce537a6eac))
* **edr:** complete /edr/api status codes, gate locations, crs on every data query ([#975](https://github.com/mrauhala/meteocore/issues/975)) ([d45f8fd](https://github.com/mrauhala/meteocore/commit/d45f8fd5bb895b9f829e3cce8ce46646fb89b7ca))
* **edr:** serve items as EDR GeoJSON and percent-encode location links ([#976](https://github.com/mrauhala/meteocore/issues/976)) ([a5dff53](https://github.com/mrauhala/meteocore/commit/a5dff5311d6c5a171b4b9540059dc88cb92f5011))
* **edr:** GRIB and Satellite datetime selects only intersecting steps ([#974](https://github.com/mrauhala/meteocore/issues/974)) ([7974784](https://github.com/mrauhala/meteocore/commit/7974784a2a94df00845115c8117bbecbbab55155))
* **edr:** collection links to query end points, typed license link, observedProperty id ([#980](https://github.com/mrauhala/meteocore/issues/980)) ([25a12b0](https://github.com/mrauhala/meteocore/commit/25a12b03c2c5f92096514d9bb69b8472f9dbac48))
* **edr:** keep every datetime-list instant on grids, no-step engines and CSV areas ([#972](https://github.com/mrauhala/meteocore/issues/972)) ([1cbd164](https://github.com/mrauhala/meteocore/commit/1cbd1646b2bab0eae0d62d280fae0337af904968))
* **edr:** return only cells that meet the area, every parameter by default ([#977](https://github.com/mrauhala/meteocore/issues/977)) ([b0ad41f](https://github.com/mrauhala/meteocore/commit/b0ad41f554bcacaf567262a638d33c79b1db15ee))
* **edr:** UCUM unit type, instances list links and instance titles ([#985](https://github.com/mrauhala/meteocore/issues/985)) ([d90179d](https://github.com/mrauhala/meteocore/commit/d90179d1a9da142195000acc4879e3169990f19e))
* **edr:** accept limit on trajectory and cube, PVOL cross-section z selects sweeps ([#986](https://github.com/mrauhala/meteocore/issues/986)) ([b24e357](https://github.com/mrauhala/meteocore/commit/b24e3571211e1e10769c1045cec1b7e86e08b85c))
* **edr:** ignore limit for PNG and HTML, name stations on data-query HTML pages ([#989](https://github.com/mrauhala/meteocore/issues/989)) ([839469d](https://github.com/mrauhala/meteocore/commit/839469dd27f32d49e7103a8e8210ca01ae2b23da))
* **metrics:** collection health gauges follow live status ([#991](https://github.com/mrauhala/meteocore/issues/991)) ([3508923](https://github.com/mrauhala/meteocore/commit/3508923d561537299d671098f139ab1c0ae88ebe))
* **odim:** a deadline or storage failure fails the PVOL read instead of blanking it ([#996](https://github.com/mrauhala/meteocore/issues/996)) ([3d1cfa5](https://github.com/mrauhala/meteocore/commit/3d1cfa527c11473eb1acf194fee1bd434b90738e))
* **engine-cap:** keep standard CAP properties filterable with no alerts loaded ([#1013](https://github.com/mrauhala/meteocore/issues/1013)) ([ec7f9ee](https://github.com/mrauhala/meteocore/commit/ec7f9ee18c212464c7bee51a8ef9e5cf7f75b27d))
* **health:** data_age_secs is the age of the newest data, not of the last poll ([#1017](https://github.com/mrauhala/meteocore/issues/1017)) ([9aee370](https://github.com/mrauhala/meteocore/commit/9aee370b289fcceefc44cf61defd8ed5e722bb08))
* **bufr:** decode the WIS2 SYNOP messages that still failed ([#1021](https://github.com/mrauhala/meteocore/issues/1021)) ([67a4434](https://github.com/mrauhala/meteocore/commit/67a44343a95e1960e540ced936f3d77ca2e34de6))
* **api-wms:** advertise MaxWidth, MaxHeight and LayerLimit in GetCapabilities ([#1014](https://github.com/mrauhala/meteocore/issues/1014)) ([82ef4cc](https://github.com/mrauhala/meteocore/commit/82ef4cc8e0b9276139e402bfa7597c1a8377a9b4))
* **discovery:** never load a publisher's in-progress dotfiles or temp files ([#1015](https://github.com/mrauhala/meteocore/issues/1015)) ([dc5c16f](https://github.com/mrauhala/meteocore/commit/dc5c16f70eb5c2355736963fe8c6d81c628ed8f3))
* **engine-grib:** give hour-window aggregates their own time axes ([#1019](https://github.com/mrauhala/meteocore/issues/1019)) ([f377fa4](https://github.com/mrauhala/meteocore/commit/f377fa43c20d696c54a99510c54216f4ab063f83))
* **engine-geotiff:** retry a failed remote COG header read next poll, not by full download ([#1020](https://github.com/mrauhala/meteocore/issues/1020)) ([d667dad](https://github.com/mrauhala/meteocore/commit/d667dadf29c36763dfcbfc30a043841f546dc04c))
* **storage:** give background whole-object fetches room for object_store's retry ([#1034](https://github.com/mrauhala/meteocore/issues/1034)) ([27b2f52](https://github.com/mrauhala/meteocore/commit/27b2f52b6123fe99c1a754233a4d53b89a9eef85))


### Performance Improvements

* **odim:** keep pre-warmed PVOL sweeps in the pixel cache ([#997](https://github.com/mrauhala/meteocore/issues/997)) ([eacafae](https://github.com/mrauhala/meteocore/commit/eacafaefca48db5a56823c24c533949e0ca3657c))
* **odim:** a cold PVOL pixel miss decodes only the requested moment ([#998](https://github.com/mrauhala/meteocore/issues/998)) ([0a34c82](https://github.com/mrauhala/meteocore/commit/0a34c82e763b440ba25abf739a000aa61d699508))
* **wms:** default the rendered-image cache to 256 MB ([#1016](https://github.com/mrauhala/meteocore/issues/1016)) ([fe2cdca](https://github.com/mrauhala/meteocore/commit/fe2cdcae6e9b0825b6f4e75e087ea29f06a1cbf8))
* **engine-geotiff:** pre-warm new remote COG frames for their first view ([#1018](https://github.com/mrauhala/meteocore/issues/1018)) ([e79939c](https://github.com/mrauhala/meteocore/commit/e79939c0a89817a87157c3bc0521ef6e29e3d09a))
* **edr,wms:** describe long run axes as ranges and page /instances ([#1032](https://github.com/mrauhala/meteocore/issues/1032)) ([1f5d350](https://github.com/mrauhala/meteocore/commit/1f5d350e29c2ee36b0a9fa339afc93f11c3a4645))

## [0.10.0](https://github.com/mrauhala/meteocore/compare/v0.9.0...v0.10.0) (2026-10-01)


### Features

* **wis2:** ds-wis2 consumer client crate + Wis2Config ([#690](https://github.com/mrauhala/meteocore/issues/690)) ([fd24158](https://github.com/mrauhala/meteocore/commit/fd241588f390b27b101fd56a31ffed3c94ebd275))
* **cap:** WIS2 push source — [cap.wis2] subscription, supersede/cancel, MeteoAlarm zone geometry ([#691](https://github.com/mrauhala/meteocore/issues/691)) ([7ebeb4d](https://github.com/mrauhala/meteocore/commit/7ebeb4d0769da6768e836c6121105f9728c20698))
* **bufr:** engine-bufr — WMO BUFR surface observations as EDR + Features ([#692](https://github.com/mrauhala/meteocore/issues/692)) ([3bb4e73](https://github.com/mrauhala/meteocore/commit/3bb4e739674032720b273635fa36bd5858097519))
* **bufr:** [bufr.wis2] — SYNOP observations pushed over WIS2 ([#698](https://github.com/mrauhala/meteocore/issues/698)) ([2b9fad1](https://github.com/mrauhala/meteocore/commit/2b9fad1c455582a745f8da40fc3e9d895a178734))
* **cap:** expose <parameter> and <eventCode> pairs as feature properties ([#699](https://github.com/mrauhala/meteocore/issues/699)) ([d02dc8d](https://github.com/mrauhala/meteocore/commit/d02dc8ddda24873df22cecd6e262f1e7c770c804))
* **bufr:** serve display units — K → °C, Pa → hPa, kg m-2 → mm ([#703](https://github.com/mrauhala/meteocore/issues/703)) ([c8f7427](https://github.com/mrauhala/meteocore/commit/c8f74278da4f9acdb1b8d9c39038f92f143ec499))
* **features:** filter items by feature properties ([#710](https://github.com/mrauhala/meteocore/issues/710)) ([48a8ea8](https://github.com/mrauhala/meteocore/commit/48a8ea8ef57af57864cf8ad320eb75adaecc11ca))
* **features:** add HTML representations for feature resources ([#711](https://github.com/mrauhala/meteocore/issues/711)) ([6c4fa2f](https://github.com/mrauhala/meteocore/commit/6c4fa2f3c83a9f4826ed35e8fd6945f2786ea562))
* **grib:** split sources into vertical collections ([#750](https://github.com/mrauhala/meteocore/issues/750)) ([95b5ba6](https://github.com/mrauhala/meteocore/commit/95b5ba639681c8de706bf66bb940c8707dfd8a53))
* **ogc:** shared OGC API root with Maps and Tiles building blocks, phase 1 of [#789](https://github.com/mrauhala/meteocore/issues/789) ([#791](https://github.com/mrauhala/meteocore/issues/791)) ([c784dfd](https://github.com/mrauhala/meteocore/commit/c784dfdd0b41d6856a756ed996dd800c3750fad4))
* **ogc:** Features at the shared OGC API root, phase 2 of [#789](https://github.com/mrauhala/meteocore/issues/789) ([#803](https://github.com/mrauhala/meteocore/issues/803)) ([37186e7](https://github.com/mrauhala/meteocore/commit/37186e7c807f8a567af816101838d7e8d21d5e97))
* **html:** API chips and a Maps / map tiles / vector tiles switch on collection pages ([#813](https://github.com/mrauhala/meteocore/issues/813)) ([3127011](https://github.com/mrauhala/meteocore/commit/31270112f7f4caaed6614d3851baa0e7efc5e60b))
* **landing:** stop advertising the per-API Maps, Tiles and Features services ([#815](https://github.com/mrauhala/meteocore/issues/815)) ([2bfd85f](https://github.com/mrauhala/meteocore/commit/2bfd85f0b5d2d96b8438301995f08c9cc0442a87))
* **geo:** geostationary projection and CF grid-mapping parser ([#820](https://github.com/mrauhala/meteocore/issues/820)) ([ecffaa3](https://github.com/mrauhala/meteocore/commit/ecffaa32b0b7034a4441a0e8b8a45968e5ec1999))
* **resample:** refine ProjectionGrid cells on a domain boundary ([#821](https://github.com/mrauhala/meteocore/issues/821)) ([b432d93](https://github.com/mrauhala/meteocore/commit/b432d933ce6abe4d3758cdd5a2b5c8fb0c5f0872))
* **map:** per-parameter time axes in WMS, Maps and Tiles ([#822](https://github.com/mrauhala/meteocore/issues/822)) ([99933ae](https://github.com/mrauhala/meteocore/commit/99933aef0ab06da69b35cd162a8903458bd4f2e4))
* **satellite:** engine-satellite, GOES-R ABI imagery over WMS, Maps and Tiles ([#824](https://github.com/mrauhala/meteocore/issues/824)) ([68a3a94](https://github.com/mrauhala/meteocore/commit/68a3a94a57ac56ea19256c00918db6b01c866955))
* **satellite:** EDR position, area and radius; per-parameter EDR extents ([#825](https://github.com/mrauhala/meteocore/issues/825)) ([15eb72a](https://github.com/mrauhala/meteocore/commit/15eb72a416c4262d5ff3906da976e68c977dea16))
* **preview:** per-parameter time axes in the time slider ([#827](https://github.com/mrauhala/meteocore/issues/827)) ([4b1bfed](https://github.com/mrauhala/meteocore/commit/4b1bfed7d7d278f51d5b730ed7c8f682bb85ff59))
* **satellite:** GOES-18 across the antimeridian ([#829](https://github.com/mrauhala/meteocore/issues/829)) ([53a023c](https://github.com/mrauhala/meteocore/commit/53a023c07a0aef5d62e5c69f7ebc47d370c24155))
* **satellite:** Himawari-9 from ISatSS tiles ([#831](https://github.com/mrauhala/meteocore/issues/831)) ([885d6cc](https://github.com/mrauhala/meteocore/commit/885d6ccf27474fa4a55c8f149f8ed6b87e0bcdb7))
* **edr:** accept media types in the f parameter ([#834](https://github.com/mrauhala/meteocore/issues/834)) ([ceb2da6](https://github.com/mrauhala/meteocore/commit/ceb2da6d7f5c1bf341eac1e76ef3734c442b3fb4))
* **server:** make render concurrency configurable with [server] render_concurrency ([#859](https://github.com/mrauhala/meteocore/issues/859)) ([931736b](https://github.com/mrauhala/meteocore/commit/931736b7a10c614229c53837a41eef1e80b3cad0))
* **maps,tiles:** advertise render parameters and document parameter-name ([#865](https://github.com/mrauhala/meteocore/issues/865)) ([0825419](https://github.com/mrauhala/meteocore/commit/0825419101e642561fe4500909fff1f0b6606c24))
* **nowcast:** serve signed significance contributions and one null rule per join group ([#866](https://github.com/mrauhala/meteocore/issues/866)) ([d8e78d7](https://github.com/mrauhala/meteocore/commit/d8e78d77989a1be4992f3e1b355f8f0bccb96092))
* **metrics:** render latency per collection and cache outcome ([#867](https://github.com/mrauhala/meteocore/issues/867)) ([e0d625b](https://github.com/mrauhala/meteocore/commit/e0d625b4bae6284fcbf82603a9348a6ce253b347))
* **edr,features:** log the error reason of EDR and Features failures ([#868](https://github.com/mrauhala/meteocore/issues/868)) ([df02d63](https://github.com/mrauhala/meteocore/commit/df02d63077e0260f56347706975e677e00e44128))
* **features:** CRS by reference, crs and bbox-crs on items ([#870](https://github.com/mrauhala/meteocore/issues/870)) ([ddddc59](https://github.com/mrauhala/meteocore/commit/ddddc59bf02a94b6dbcd01b94d5427c43906b397))
* **metrics:** render phase histogram and staged render rejections ([#876](https://github.com/mrauhala/meteocore/issues/876)) ([aa421ca](https://github.com/mrauhala/meteocore/commit/aa421ca8f738885d000f6c54bab80d152d87155a))
* **api-wms:** honour TRANSPARENT and BGCOLOR in GetMap ([#893](https://github.com/mrauhala/meteocore/issues/893)) ([2ce3d7e](https://github.com/mrauhala/meteocore/commit/2ce3d7e9c3c64f23f0e71dea4dfc48f0b6dfa204))
* **api-edr:** QUDT units, CF observed properties and distinct label/description in parameter metadata ([#895](https://github.com/mrauhala/meteocore/issues/895)) ([05543a6](https://github.com/mrauhala/meteocore/commit/05543a687f45258f26a411de8f46b5b49d80f8fb))
* **render:** let a named style list several parameter layers ([#900](https://github.com/mrauhala/meteocore/issues/900)) ([bdc597a](https://github.com/mrauhala/meteocore/commit/bdc597a05ddf4eb107969a795c14f8b98b5c2ff6))
* **render:** widen the default temperature palette to one scale for every level ([#901](https://github.com/mrauhala/meteocore/issues/901)) ([cec31f8](https://github.com/mrauhala/meteocore/commit/cec31f8feb1c407cc93ab01b2de6622437c09c93))
* **render:** RGB composite primitive with per-channel range, gamma and band differences ([#904](https://github.com/mrauhala/meteocore/issues/904)) ([0a398e4](https://github.com/mrauhala/meteocore/commit/0a398e45fc03c3a349c065bada83297df02a4da2))
* **satellite:** multi-band raster request that shares one coordinate map ([#905](https://github.com/mrauhala/meteocore/issues/905)) ([52923ec](https://github.com/mrauhala/meteocore/commit/52923ecc8d9218d2adb3f4dbc71f632103da9484))
* **satellite:** RGB composite definitions with their own time axis ([#909](https://github.com/mrauhala/meteocore/issues/909)) ([3393946](https://github.com/mrauhala/meteocore/commit/33939465a8ce5de7df499b0276dba76df02852a8))
* **maps:** serve RGB composites as their own WMS, Maps and Tiles layers ([#911](https://github.com/mrauhala/meteocore/issues/911)) ([8f8d9f3](https://github.com/mrauhala/meteocore/commit/8f8d9f355c63cf566ca805dde828ee927a4c086e))
* **satellite:** GMGSI hourly global IR mosaic as a display layer ([#906](https://github.com/mrauhala/meteocore/issues/906)) ([eadde6b](https://github.com/mrauhala/meteocore/commit/eadde6b2b09e326b1381e536381f43a0c583dfc3))
* **satellite:** built-in Airmass and Night Microphysics composite recipes ([#913](https://github.com/mrauhala/meteocore/issues/913)) ([4016bd8](https://github.com/mrauhala/meteocore/commit/4016bd875425c5eff631763ad505bbd7d61e8ec9))
* **render:** lossy WebP with a QUALITY parameter and a per-collection default ([#915](https://github.com/mrauhala/meteocore/issues/915)) ([bed60b7](https://github.com/mrauhala/meteocore/commit/bed60b7f7b621e14c1f5b8eb6b2f25951ecb18d7))
* **edr:** serve CoverageJSON as application/vnd.cov+json, the EDR 1.2 media type ([#931](https://github.com/mrauhala/meteocore/issues/931)) ([f58dc40](https://github.com/mrauhala/meteocore/commit/f58dc40e1f52b72032c9a4487c217c011eba74d8))
* **edr:** items query over the collection's FeatureEngine ([#939](https://github.com/mrauhala/meteocore/issues/939)) ([a172d24](https://github.com/mrauhala/meteocore/commit/a172d2425397dd988dca606d63a6530a8f6bdb50))
* **edr:** limit on data queries and paging for /locations (EDR 1.2) ([#934](https://github.com/mrauhala/meteocore/issues/934)) ([a6b283a](https://github.com/mrauhala/meteocore/commit/a6b283accb8b80a712c870eb3deec4419f8b0cee))
* **edr:** EDR 1.2 z and datetime grammar — ignore z without a vertical axis, open and recurring z intervals, datetime lists ([#936](https://github.com/mrauhala/meteocore/issues/936)) ([bb76f3a](https://github.com/mrauhala/meteocore/commit/bb76f3a64e0c86253731caf12fd2ffe6b051d854))
* **edr:** several location ids in one locations query (EDR 1.2 multiple_locations) ([#942](https://github.com/mrauhala/meteocore/issues/942)) ([b963dba](https://github.com/mrauhala/meteocore/commit/b963dba88356852906cbb5216bfcbe6520731cb7))
* **edr:** cube query with resolution-x/y/z on GRIB pressure and model levels ([#938](https://github.com/mrauhala/meteocore/issues/938)) ([e9edd23](https://github.com/mrauhala/meteocore/commit/e9edd2329e519eb8bcdcaa2aa12ccb56e7293286))
* **edr:** EDR GeoJSON output for point queries on station collections ([#941](https://github.com/mrauhala/meteocore/issues/941)) ([5cd103f](https://github.com/mrauhala/meteocore/commit/5cd103fa82cbba90e7a4b476f4b65b8b5796fbdb))
* **edr:** trajectory along-path sampling on GRIB, QueryData and Zarr, LINESTRING Z/M/ZM ([#937](https://github.com/mrauhala/meteocore/issues/937)) ([c250b68](https://github.com/mrauhala/meteocore/commit/c250b681501492ef826ea319000c53ebda78f474))


### Bug Fixes

* **server:** spawn CAP poll loops at boot and shut down cap/postgis/nowcast gracefully ([#688](https://github.com/mrauhala/meteocore/issues/688)) ([f0136cd](https://github.com/mrauhala/meteocore/commit/f0136cd54bf1e5b956505cbe7e237d2e43345830))
* **render:** key rendered caches on the engine's content version (stale CAP tiles) ([#701](https://github.com/mrauhala/meteocore/issues/701)) ([9079e79](https://github.com/mrauhala/meteocore/commit/9079e79729850461d05c8b4f1f43fe73f9282ecc))
* **cap:** union the per-geocode geometry hints of a multi-zone area ([#704](https://github.com/mrauhala/meteocore/issues/704)) ([e543b5c](https://github.com/mrauhala/meteocore/commit/e543b5c606d80f2ff48bf2ddc94f3f7e3eec1fb7))
* **cap:** correct warning lifecycle and API consistency ([#708](https://github.com/mrauhala/meteocore/issues/708)) ([3bbf5b8](https://github.com/mrauhala/meteocore/commit/3bbf5b8748ce4b299551afc85fb1999022b83d15))
* **cap:** expose future warning times while preserving the current default ([#714](https://github.com/mrauhala/meteocore/issues/714)) ([03b8c4f](https://github.com/mrauhala/meteocore/commit/03b8c4fc07f1642a3485e3550239fb6418dc621a))
* **edr:** bound request work and isolate synchronous queries ([#712](https://github.com/mrauhala/meteocore/issues/712)) ([387c95a](https://github.com/mrauhala/meteocore/commit/387c95a9a1f7242ffb8d4d338e5f58a9ba536ded))
* **render:** bound transient memory across raster APIs ([#713](https://github.com/mrauhala/meteocore/issues/713)) ([5391511](https://github.com/mrauhala/meteocore/commit/5391511ed38dab071d2e4b009708b8ad5c6b2184))
* **nowcast:** retain serving history across compatible reloads ([#732](https://github.com/mrauhala/meteocore/issues/732)) ([5992f92](https://github.com/mrauhala/meteocore/commit/5992f9221820606f3cb0f976d2b3c8d27bfa4dcb))
* **render:** use per-parameter units for styles and legends ([#733](https://github.com/mrauhala/meteocore/issues/733)) ([5652f85](https://github.com/mrauhala/meteocore/commit/5652f850255620718b7cce5519411f99b8655426))
* **edr:** bound location response memory through delivery ([#735](https://github.com/mrauhala/meteocore/issues/735)) ([dbac94b](https://github.com/mrauhala/meteocore/commit/dbac94bd7b0bda3aa8cf5a4c8691db4fb5ac7f5e))
* **grib:** correct field selection, sampling, and cache fills ([#749](https://github.com/mrauhala/meteocore/issues/749)) ([9007544](https://github.com/mrauhala/meteocore/commit/9007544a0b52f4a6cf83874760208a44660b151a))
* **grib:** preserve wgrib2 level identities and summarize warnings ([#752](https://github.com/mrauhala/meteocore/issues/752)) ([135b4d2](https://github.com/mrauhala/meteocore/commit/135b4d2e99ee3c0fb510415e317f8ee5ba70c9a3))
* **zarr:** bound gzip and zstd decode output ([#774](https://github.com/mrauhala/meteocore/issues/774)) ([8a663c4](https://github.com/mrauhala/meteocore/commit/8a663c4484b5fc376725daff2f288a272d4bf426))
* **zarr:** bound Blosc frames and admit decode scratch ([#775](https://github.com/mrauhala/meteocore/issues/775)) ([61e87d4](https://github.com/mrauhala/meteocore/commit/61e87d465dc67733d227ed891ddcb97e5bd23fd7))
* **zarr:** admit cache hits without cold decode workspace ([#776](https://github.com/mrauhala/meteocore/issues/776)) ([4122d9a](https://github.com/mrauhala/meteocore/commit/4122d9a2dd9e4613cf667b8670c262393dc00f29))
* **zarr:** reserve encoded and codec headroom for cold reads ([#781](https://github.com/mrauhala/meteocore/issues/781)) ([61db8a0](https://github.com/mrauhala/meteocore/commit/61db8a0912c66de817ec378114dde0d4bb9a2422))
* **zarr:** avoid duplicate decode reservations for coalesced reads ([#782](https://github.com/mrauhala/meteocore/issues/782)) ([34c04cd](https://github.com/mrauhala/meteocore/commit/34c04cddf66ea01adb519c176340ff2cb5d20cee))
* **zarr:** release Blosc scratch admission after each decode ([#783](https://github.com/mrauhala/meteocore/issues/783)) ([dee3cc4](https://github.com/mrauhala/meteocore/commit/dee3cc477a98d193d9971ce263ebc5c9ef9a15a0))
* **zarr:** release temporary admission between serial chunks ([#784](https://github.com/mrauhala/meteocore/issues/784)) ([331b9c1](https://github.com/mrauhala/meteocore/commit/331b9c157076fc25e6698e65e5a7784193aca799))
* **zarr:** prepay peak scratch for stacked Blosc codecs ([#785](https://github.com/mrauhala/meteocore/issues/785)) ([6802aab](https://github.com/mrauhala/meteocore/commit/6802aab027ff76cfa9fafa29398a8be1c0daad65))
* **preview:** avoid tile requests during initial zoom ([#786](https://github.com/mrauhala/meteocore/issues/786)) ([15ac33f](https://github.com/mrauhala/meteocore/commit/15ac33fe65a85024a0913412e5b0c285e2993632))
* **ogc:** registered link relations, tileset resources and mount-agnostic Maps/Tiles, phase 0 of [#789](https://github.com/mrauhala/meteocore/issues/789) ([#790](https://github.com/mrauhala/meteocore/issues/790)) ([1faf071](https://github.com/mrauhala/meteocore/commit/1faf0711c2e265a47bfc5029da797e682f3861ef))
* **ogc:** valid Common extents for vertical dimensions and CRS84 bounds ([#798](https://github.com/mrauhala/meteocore/issues/798)) ([7b4a80d](https://github.com/mrauhala/meteocore/commit/7b4a80d12df739a7ff97f0b45a271d152b320d90))
* **querydata:** render projected grids such as MEPS ([#799](https://github.com/mrauhala/meteocore/issues/799)) ([15dd7e3](https://github.com/mrauhala/meteocore/commit/15dd7e3502010635fb6605ced1405ef67d9e04a8))
* **querydata:** project LCC grids on the sphere their file declares ([#801](https://github.com/mrauhala/meteocore/issues/801)) ([ac83ca1](https://github.com/mrauhala/meteocore/commit/ac83ca1a1ae77d8d7cfe5e48c2842174cfb19755))
* **ogc:** advertise Maps and Tiles relations in registered form only ([#806](https://github.com/mrauhala/meteocore/issues/806)) ([f12216e](https://github.com/mrauhala/meteocore/commit/f12216ea0b5a3bbac7c1b3befa452e5486e96c49))
* **openapi:** tag every operation so API docs group them per collection ([#812](https://github.com/mrauhala/meteocore/issues/812)) ([017a7ea](https://github.com/mrauhala/meteocore/commit/017a7ea081e14eff10387fef567a27b76bae0054))
* **3dtiles:** the viewer's ?base override selects a same-origin path only ([#814](https://github.com/mrauhala/meteocore/issues/814)) ([dff88f1](https://github.com/mrauhala/meteocore/commit/dff88f134a8e82ee2c7478c2ba4ff6887969aeaf))
* **storage:** hourly prefix patterns, and no poll-time panic on a bad one ([#816](https://github.com/mrauhala/meteocore/issues/816)) ([042e405](https://github.com/mrauhala/meteocore/commit/042e40533e35d5ff830a1ccc387cfb12f6b09ac7))
* **geojson:** match the bbox filter against geometries, not envelopes ([#835](https://github.com/mrauhala/meteocore/issues/835)) ([8f5385f](https://github.com/mrauhala/meteocore/commit/8f5385f9047152c0ede972c9c9afbb07ead11592))
* **3dtiles:** reject an unknown point quantity before render admission ([#837](https://github.com/mrauhala/meteocore/issues/837)) ([b1c59fa](https://github.com/mrauhala/meteocore/commit/b1c59fa2ed2898cfb9d8a1877fc50fd19d91f72c))
* **features:** a single feature takes only f; any other parameter is a 400 ([#842](https://github.com/mrauhala/meteocore/issues/842)) ([935aa5b](https://github.com/mrauhala/meteocore/commit/935aa5b148ecd7217e8b94f4751fed073912e6fa))
* **edr:** unsupported position and area queries answer 404 like radius and trajectory ([#840](https://github.com/mrauhala/meteocore/issues/840)) ([addcdb4](https://github.com/mrauhala/meteocore/commit/addcdb407c1f449a0594c0203400bc9f5ec246f0))
* **mcp:** reject out-of-range limit and samples instead of clamping ([#845](https://github.com/mrauhala/meteocore/issues/845)) ([e5b2609](https://github.com/mrauhala/meteocore/commit/e5b2609adf6a929763a1224cf3b87c2ff065e732))
* **geotiff:** anchor filename templates so partial uploads never match ([#847](https://github.com/mrauhala/meteocore/issues/847)) ([f38073c](https://github.com/mrauhala/meteocore/commit/f38073c7b46ac0a257550ca05fcebf29f150b78d))
* **edr:** QueryData and PostGIS validate coordinates on direct engine calls ([#849](https://github.com/mrauhala/meteocore/issues/849)) ([9371e82](https://github.com/mrauhala/meteocore/commit/9371e82aaf21b517d72f89f70a752282b3fe87a1))
* **features:** datetime filters CSV stations and is a 400 where features have no time ([#843](https://github.com/mrauhala/meteocore/issues/843)) ([8b9ff70](https://github.com/mrauhala/meteocore/commit/8b9ff70ed057b71bb90472aad84d1841f9875022))
* **mcp:** get_cell_track's note follows why the walk stopped ([#844](https://github.com/mrauhala/meteocore/issues/844)) ([73c7a74](https://github.com/mrauhala/meteocore/commit/73c7a740a4f44fc4fda6d57833ba0cd60dd82a91))
* **geotiff:** project a sphere-based LCC on its sphere ([#846](https://github.com/mrauhala/meteocore/issues/846)) ([8b2c141](https://github.com/mrauhala/meteocore/commit/8b2c1411f4112db011665db2d2369d3af1498961))
* **render:** stretch normalized palettes over the style's range ([#848](https://github.com/mrauhala/meteocore/issues/848)) ([d11d321](https://github.com/mrauhala/meteocore/commit/d11d321634169c9fd75f2b169da8d0504d3536a9))
* **cap:** over max_alerts, evict upcoming and least severe warnings first ([#851](https://github.com/mrauhala/meteocore/issues/851)) ([ec01301](https://github.com/mrauhala/meteocore/commit/ec0130163c0c3d8fe2623fca0da26890691e463c))
* **mcp:** flag a future `at` and apply min_significance before paging ([#860](https://github.com/mrauhala/meteocore/issues/860)) ([6aa05ac](https://github.com/mrauhala/meteocore/commit/6aa05ac7a3ea4c25148866446b37a3cba041a812))
* **claude:** lint only this worktree's code in issue-batch's shared target dir ([#862](https://github.com/mrauhala/meteocore/issues/862)) ([a2cb10d](https://github.com/mrauhala/meteocore/commit/a2cb10dae329cf7ae391cadb790d79685540299c))
* **cap:** keep an advertised TIME instant inside every warning window ([#864](https://github.com/mrauhala/meteocore/issues/864)) ([dd17550](https://github.com/mrauhala/meteocore/commit/dd17550c45b33a5826988a448a61ff0e37d5f2d5))
* **claude:** issue-batch lessons from its first run ([#869](https://github.com/mrauhala/meteocore/issues/869)) ([2a89601](https://github.com/mrauhala/meteocore/commit/2a89601f87d913a46d4485be4c525db52fa2eeef))
* **render:** rescale a palette onto a style range entirely outside its values ([#873](https://github.com/mrauhala/meteocore/issues/873)) ([e8235ea](https://github.com/mrauhala/meteocore/commit/e8235ea223f645e579a8d07920f596aa1bdee4c3))
* **scripts:** name offending files in the multi-line safety-check passes ([#880](https://github.com/mrauhala/meteocore/issues/880)) ([da0407e](https://github.com/mrauhala/meteocore/commit/da0407e7904feb5503856792ecdf28c3817cb957))
* **engine-csv:** warn when a CSV file mixes UTC offsets ([#883](https://github.com/mrauhala/meteocore/issues/883)) ([3e80639](https://github.com/mrauhala/meteocore/commit/3e80639620cd38bb6f0675d0413c7d33b3b9fc4a))
* **mcp:** walk get_cell_track by retained frames with a by-id lookup ([#887](https://github.com/mrauhala/meteocore/issues/887)) ([1f2202c](https://github.com/mrauhala/meteocore/commit/1f2202c234de08dd9273001b3206138b406e8332))
* **render:** give GRIB upper-air and surface-pressure fields their own default styles ([#885](https://github.com/mrauhala/meteocore/issues/885)) ([baca7a9](https://github.com/mrauhala/meteocore/commit/baca7a9f0410698d26be73f01e9f234ea4ad436d))
* **engine-geotiff:** enforce the EDR area budget across timesteps, 400 not nulls ([#888](https://github.com/mrauhala/meteocore/issues/888)) ([dced18e](https://github.com/mrauhala/meteocore/commit/dced18edb6373f0cf72f629af47b959db7f7ca5e))
* **engine-odim:** antimeridian-aware WGS84 envelope for composites ([#891](https://github.com/mrauhala/meteocore/issues/891)) ([fb01004](https://github.com/mrauhala/meteocore/commit/fb010040149a178a01d87caf1634f2fb833a44e8))
* **engine-zarr:** return exact unaligned byte ranges from Blosc partial decoding ([#890](https://github.com/mrauhala/meteocore/issues/890)) ([8aeb79b](https://github.com/mrauhala/meteocore/commit/8aeb79ba98245552815c5da67776c07aef03420b))
* **core:** keep global grids registered across the pole in projected views ([#896](https://github.com/mrauhala/meteocore/issues/896)) ([02cfdcd](https://github.com/mrauhala/meteocore/commit/02cfdcda171d1fccbb5fc92f0317d407232132e4))
* **api-maps:** accept a CRS84 bbox crossing the antimeridian ([#903](https://github.com/mrauhala/meteocore/issues/903)) ([74ffd05](https://github.com/mrauhala/meteocore/commit/74ffd05b2980ed7a8ca18cbfc4748d6b7919a2b6))
* **satellite:** drop scans that leave the window and keep in-window scans resident ([#914](https://github.com/mrauhala/meteocore/issues/914)) ([83095c1](https://github.com/mrauhala/meteocore/commit/83095c19f30ef53017984e874105f37f3c2c5ed1))
* **satellite:** stop a reload's backfill from blanking the next scan ([#917](https://github.com/mrauhala/meteocore/issues/917)) ([57fa43b](https://github.com/mrauhala/meteocore/commit/57fa43be479a1a14647daf35faa455a4c4877a79))
* **edr:** title, description and crs_details on every data query link ([#935](https://github.com/mrauhala/meteocore/issues/935)) ([4896d30](https://github.com/mrauhala/meteocore/commit/4896d3081f472fd2da6a4a5a45cc87a416375c2b))
* **edr:** RFC 3339 instance ids per the MetOcean EDR profile ([#949](https://github.com/mrauhala/meteocore/issues/949)) ([f050e0e](https://github.com/mrauhala/meteocore/commit/f050e0ebd4e14e1b154452d0c5ed3bd6aaaa8478))
* **maps:** optional bbox, subset, center, scale-denominator and map response headers ([#952](https://github.com/mrauhala/meteocore/issues/952)) ([9dd65e2](https://github.com/mrauhala/meteocore/commit/9dd65e211b94fffa57b8bb62e18999e58931d70d))
* **conformance:** declare Maps Tilesets and Tiles DateTime, with the map and vector tile parameters they require ([#955](https://github.com/mrauhala/meteocore/issues/955)) ([8252e60](https://github.com/mrauhala/meteocore/commit/8252e60662c244c232c082d270ef7009fb37b09f))


### Performance Improvements

* **wms:** reuse tiles for EPSG:3067 and EPSG:3035 viewports ([#734](https://github.com/mrauhala/meteocore/issues/734)) ([ea2f528](https://github.com/mrauhala/meteocore/commit/ea2f5284aeb9935c769c46d5a33e1a61ab190a2e))
* **geotiff:** add opt-in bounded COG range batching ([#736](https://github.com/mrauhala/meteocore/issues/736)) ([89fc5eb](https://github.com/mrauhala/meteocore/commit/89fc5eb101429c1c2143351e999959074fa29fa5))
* **grib:** halve decoded grid buffers while preserving query precision ([#751](https://github.com/mrauhala/meteocore/issues/751)) ([0e1cd1d](https://github.com/mrauhala/meteocore/commit/0e1cd1d06457e2afad8c94a7300d1a70cd05adee))
* **grib:** batch position sampling and parallelize field reads ([#753](https://github.com/mrauhala/meteocore/issues/753)) ([011eb3a](https://github.com/mrauhala/meteocore/commit/011eb3a1e265b4e9fc7bbca9b9ed4b00041cd769))
* **grib:** download index sidecars concurrently ([#754](https://github.com/mrauhala/meteocore/issues/754)) ([363ad2b](https://github.com/mrauhala/meteocore/commit/363ad2bcfcdc51ad082526a79d8b48291a89c25b))
* **grib:** batch metadata probes using header ranges ([#755](https://github.com/mrauhala/meteocore/issues/755)) ([0a501ad](https://github.com/mrauhala/meteocore/commit/0a501ad346b153a71650300d7b75174ad0d1d466))
* **grib:** batch area and radius field reads ([#756](https://github.com/mrauhala/meteocore/issues/756)) ([2e4c363](https://github.com/mrauhala/meteocore/commit/2e4c3633321c93516d4e6c17eb68a454f8bad10a))
* **grib:** cache compressed messages across grid evictions ([#757](https://github.com/mrauhala/meteocore/issues/757)) ([e03e3a3](https://github.com/mrauhala/meteocore/commit/e03e3a3a34f3676da3ee2eb123bf81d089d98fc2))
* **grib:** cache discovery metadata and expose vertical axes in HTML ([#758](https://github.com/mrauhala/meteocore/issues/758)) ([4d23b3f](https://github.com/mrauhala/meteocore/commit/4d23b3f7e006e6f4a0b63b773e9fd7ebf2ea0bfb))
* **querydata:** render projected sources through the projection grid ([#836](https://github.com/mrauhala/meteocore/issues/836)) ([d45de92](https://github.com/mrauhala/meteocore/commit/d45de920c675eab76a44f1a5bfe6b60cd3ad9f24))
* **grib:** parse time_window once at load ([#850](https://github.com/mrauhala/meteocore/issues/850)) ([2c3b777](https://github.com/mrauhala/meteocore/commit/2c3b777d3029a09a923d5b19e89fc118f6e45355))
* **3dtiles:** move points and meshes onto a 3D content pool ([#861](https://github.com/mrauhala/meteocore/issues/861)) ([7177efa](https://github.com/mrauhala/meteocore/commit/7177efab3055e3d8ec2386ef8bc20af19e50bad4))
* **geotiff:** preload STAC item metadata in the poll cycle ([#874](https://github.com/mrauhala/meteocore/issues/874)) ([cce2965](https://github.com/mrauhala/meteocore/commit/cce2965e34f3f82316d13d34a6671be53a0eece1))
* **engine-geotiff:** cache decoded remote COG tiles across meta-tiles ([#875](https://github.com/mrauhala/meteocore/issues/875)) ([94ded6c](https://github.com/mrauhala/meteocore/commit/94ded6cc6e9b0edb6d231664da2ffa5caeae22f0))
* **nowcast:** sub-pixel motion vectors, local outlier gate, unsmoothed measured blocks ([#877](https://github.com/mrauhala/meteocore/issues/877)) ([fbf0299](https://github.com/mrauhala/meteocore/commit/fbf0299c3ebbfe23433cc448e2056f3242c82533))
* **render:** compact F32 raster values, produced by the GRIB map path ([#878](https://github.com/mrauhala/meteocore/issues/878)) ([04a76fb](https://github.com/mrauhala/meteocore/commit/04a76fb665dd7dd6f8595ce57df42e5e109b04ed))
* **querydata:** sample the grid extents once at load instead of per request ([#884](https://github.com/mrauhala/meteocore/issues/884)) ([319123b](https://github.com/mrauhala/meteocore/commit/319123b848cc0f31a3afb9f25d4dc0979b40f326))
* **render:** lossless WebP at effort 25, 1.6–3.2× faster to encode ([#916](https://github.com/mrauhala/meteocore/issues/916)) ([f1f35b8](https://github.com/mrauhala/meteocore/commit/f1f35b83de6ce90500ca9dc6c948cab1fc1c5395))


### config

* add ECMWF AIFS GRIB collection ([#761](https://github.com/mrauhala/meteocore/issues/761)) ([cc1b230](https://github.com/mrauhala/meteocore/commit/cc1b230e32db84b9898766a84efa797d0784d88e))
* enable IFS pressure levels, gusts and most-unstable CAPE ([#762](https://github.com/mrauhala/meteocore/issues/762)) ([b469319](https://github.com/mrauhala/meteocore/commit/b469319824a6e55be4df744e1d2d0b91ab2509e1))


### nowcast

* verify the production motion estimator alongside the baseline ([#730](https://github.com/mrauhala/meteocore/issues/730)) ([aa0dc9b](https://github.com/mrauhala/meteocore/commit/aa0dc9b6657bc6327674dc7939a979baf877ad02))

## [0.9.0](https://github.com/mrauhala/meteocore/compare/v0.8.1...v0.9.0) (2026-09-11)


### Features

* **cells:** significance ranking + fact sheets for tracked storm cells ([#602](https://github.com/mrauhala/meteocore/issues/602)) ([6f5e99b](https://github.com/mrauhala/meteocore/commit/6f5e99bcb1e48b99047374612be7d05f850d90d0))
* **nowcast:** impact context — what a cell is over and heading toward ([#603](https://github.com/mrauhala/meteocore/issues/603)) ([463e2ce](https://github.com/mrauhala/meteocore/commit/463e2ceeac772155c5ad2208a33b85eed716f16b))
* **features:** sortby on /items, and stop ignoring it silently ([#606](https://github.com/mrauhala/meteocore/issues/606)) ([727d9d9](https://github.com/mrauhala/meteocore/commit/727d9d96a3e7945b7bfd7825c437f54e6df30ddd))
* **mcp:** Model Context Protocol tools over tracked storm cells ([#608](https://github.com/mrauhala/meteocore/issues/608)) ([32209d3](https://github.com/mrauhala/meteocore/commit/32209d3d80714780f11584866da46fa51864856b))
* **cells:** demote persistent stationary echoes as likely clutter ([#614](https://github.com/mrauhala/meteocore/issues/614)) ([#615](https://github.com/mrauhala/meteocore/issues/615)) ([0789ff4](https://github.com/mrauhala/meteocore/commit/0789ff4fe5ca85000d4766e3bea6be4e2c368488))
* **cells:** lightning density, first flash, and jump magnitude in sigma ([#617](https://github.com/mrauhala/meteocore/issues/617)) ([e26be63](https://github.com/mrauhala/meteocore/commit/e26be6380342c03bdcc851663e3598586d9a1c88))
* **cells:** IC/CG split and CG polarity per storm cell ([#616](https://github.com/mrauhala/meteocore/issues/616) part 2) ([#618](https://github.com/mrauhala/meteocore/issues/618)) ([cad2dc0](https://github.com/mrauhala/meteocore/commit/cad2dc0fe5373c9f41719ece142a8066afa1d8e1))
* **cells:** path straightness and net displacement, so track_age stops being evidence ([#629](https://github.com/mrauhala/meteocore/issues/629)) ([#631](https://github.com/mrauhala/meteocore/issues/631)) ([90d19c6](https://github.com/mrauhala/meteocore/commit/90d19c6cf518839ac774c802ea2398c5928b843a))
* **mcp:** make the advertised sortable properties reachable ([#630](https://github.com/mrauhala/meteocore/issues/630)) ([#632](https://github.com/mrauhala/meteocore/issues/632)) ([388d04e](https://github.com/mrauhala/meteocore/commit/388d04e0ba847f23f6236d30394fa2511077f0c6))
* **cells:** births, deaths, pass-2 matches and velocity clamps as /metrics counters ([#643](https://github.com/mrauhala/meteocore/issues/643)) ([#656](https://github.com/mrauhala/meteocore/issues/656)) ([d899fd8](https://github.com/mrauhala/meteocore/commit/d899fd8d620263d2592d21bc5a6094132248db52))
* **cells:** nearest radar, range and lowest-beam height per storm cell ([#642](https://github.com/mrauhala/meteocore/issues/642)) ([#658](https://github.com/mrauhala/meteocore/issues/658)) ([c91af8a](https://github.com/mrauhala/meteocore/commit/c91af8ab1cdaedf80a0a0a5b95e2c60a81a0b811))
* **nowcast:** serve the motion field as EDR motion_u/motion_v/motion_quality ([#661](https://github.com/mrauhala/meteocore/issues/661)) ([#662](https://github.com/mrauhala/meteocore/issues/662)) ([1905b34](https://github.com/mrauhala/meteocore/commit/1905b34ecb2a7e9676119697c5df07d59345d801))
* **edr:** radius query type on every area-capable engine ([#513](https://github.com/mrauhala/meteocore/issues/513)) ([#670](https://github.com/mrauhala/meteocore/issues/670)) ([4363d2b](https://github.com/mrauhala/meteocore/commit/4363d2bdc160e3014bac4699a97ad2526cd1585d))
* **querydata:** EDR area and radius queries; api-edr README as the EDR status page ([#672](https://github.com/mrauhala/meteocore/issues/672)) ([1f4d030](https://github.com/mrauhala/meteocore/commit/1f4d03019fbe704b2b5a2bb377acf7b726c0f3e7))
* **zarr:** EDR area and radius queries ([#674](https://github.com/mrauhala/meteocore/issues/674)) ([76d7c30](https://github.com/mrauhala/meteocore/commit/76d7c30ff9f2e2dfe2dac4f675e4f6c4fd81f1af))
* **zarr:** forecast model runs as EDR instances and DIM_REFERENCE_TIME ([#337](https://github.com/mrauhala/meteocore/issues/337)) ([#677](https://github.com/mrauhala/meteocore/issues/677)) ([0423c54](https://github.com/mrauhala/meteocore/commit/0423c548f38d0c06b953da89b50e2cba73383ab1))


### Bug Fixes

* **clippy:** chunks_exact -> as_chunks across the workspace ([#607](https://github.com/mrauhala/meteocore/issues/607)) ([85ea8ba](https://github.com/mrauhala/meteocore/commit/85ea8ba286df64b0d0890a03c12d92cd5a662441))
* **mcp:** accept the deployment's own Host, not just loopback ([#609](https://github.com/mrauhala/meteocore/issues/609)) ([226a1ef](https://github.com/mrauhala/meteocore/commit/226a1ef804adb57bccca02cfa06ee95e07217f79))
* **cells:** stop asserting flags that were never computable ([#626](https://github.com/mrauhala/meteocore/issues/626)) ([31c8829](https://github.com/mrauhala/meteocore/commit/31c8829cdb98726a8ac3977d5d35ed999e6fb71c))
* **cells:** a cell with no strikes has a zero split, not an unknown one ([#628](https://github.com/mrauhala/meteocore/issues/628)) ([adfd68b](https://github.com/mrauhala/meteocore/commit/adfd68bba268e61e22922ac1a1132c9533f49d17))
* **cells:** damp severity and trend flapping on coherent tracks ([#623](https://github.com/mrauhala/meteocore/issues/623)) ([#627](https://github.com/mrauhala/meteocore/issues/627)) ([1893dac](https://github.com/mrauhala/meteocore/commit/1893dacf52784873ec2fa179272da25c4fc1944b))
* **cells:** clutter must not flag an echo that travelled, and ranks must not hole ([#637](https://github.com/mrauhala/meteocore/issues/637)) ([653f28a](https://github.com/mrauhala/meteocore/commit/653f28a9c73c9e5c3255c33a8cb86f6a9c80b0a9))
* **cells:** match on size and intensity similarity, not centroid distance alone ([#639](https://github.com/mrauhala/meteocore/issues/639)) ([#655](https://github.com/mrauhala/meteocore/issues/655)) ([96a3d0e](https://github.com/mrauhala/meteocore/commit/96a3d0e76c20ff5daf7c691b6c361e2822dcc3b7))
* **cells:** predict aged tracks with their own velocity, not the ambient field ([#639](https://github.com/mrauhala/meteocore/issues/639)) ([#657](https://github.com/mrauhala/meteocore/issues/657)) ([becd1bb](https://github.com/mrauhala/meteocore/commit/becd1bbffd53938c4b0d7f8fccbd3c62f29f0fbd))
* **cells:** rank on the rounded significance, so page order and rank agree ([#644](https://github.com/mrauhala/meteocore/issues/644)) ([#659](https://github.com/mrauhala/meteocore/issues/659)) ([4bb1a05](https://github.com/mrauhala/meteocore/commit/4bb1a0565242a955026d8a02588e21b771e2e3e4))
* **cells:** flags become bonus terms outside the denominator; trend is signed and calibrated ([#645](https://github.com/mrauhala/meteocore/issues/645)) ([#660](https://github.com/mrauhala/meteocore/issues/660)) ([dcb8a1d](https://github.com/mrauhala/meteocore/commit/dcb8a1da219cfa7619098a8f016da90823322b5d))
* **grib:** wrap longitude in extract_bbox; area queries on 180°-first global grids 502'd ([#663](https://github.com/mrauhala/meteocore/issues/663)) ([#664](https://github.com/mrauhala/meteocore/issues/664)) ([4253d39](https://github.com/mrauhala/meteocore/commit/4253d39133a838e8a97e6d8272b094b641b54fc1))
* **edr:** declare EDR 1.1 queries/html/oas30 conformance classes; drop bogus PostGIS `location` query type ([#669](https://github.com/mrauhala/meteocore/issues/669)) ([769cf39](https://github.com/mrauhala/meteocore/commit/769cf3924a3ece73a4935036a1a601574f3843fd))
* **edr:** area/radius mask the polygon on GRIB, nowcast and PostGIS observations mode ([#671](https://github.com/mrauhala/meteocore/issues/671)) ([#678](https://github.com/mrauhala/meteocore/issues/678)) ([b4f03d2](https://github.com/mrauhala/meteocore/commit/b4f03d261678a14524202a011e3d3de5d28ea75c))


### security

* **deps:** clear all RustSec advisories and gate on cargo-audit ([#595](https://github.com/mrauhala/meteocore/issues/595)) ([2ee3813](https://github.com/mrauhala/meteocore/commit/2ee381395c068ab9c8cdeb60193380b2a64904c3))

## [0.8.1](https://github.com/mrauhala/meteocore/compare/v0.8.0...v0.8.1) (2026-08-18)


### Bug Fixes

* **docker:** pin builder+runtime to one Debian release (trixie) — glibc mismatch ([#582](https://github.com/mrauhala/meteocore/issues/582)) ([ea32212](https://github.com/mrauhala/meteocore/commit/ea32212a137a1d7ff6d1a4846794eadeef137877))

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
