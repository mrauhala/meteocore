# Vendored schemas

## OGC API Common Parts 2 and 4

These two OpenAPI 3.0 bundles are unmodified upstream files, retrieved on
2026-09-24 from commit
[`3828187a8fc5e98386743afcdd0c0d650855956f`](https://github.com/opengeospatial/ogcapi-common/tree/3828187a8fc5e98386743afcdd0c0d650855956f).
Both identify the OGC License in `info.license`. The commit is the same Common
repository baseline recorded in the [implementation matrix](../docs/ogc-api-common-matrix.md).

| File | Pinned source | SHA-256 |
|---|---|---|
| `ogcapi-common-2.bundled.json` | [Part 2](https://raw.githubusercontent.com/opengeospatial/ogcapi-common/3828187a8fc5e98386743afcdd0c0d650855956f/collections/openapi/ogcapi-common-2.bundled.json) | `70014d8ead7cf3252d96e468e942f817d75e92021af6d7357104a519f3870937` |
| `ogcapi-common-4.bundled.json` | [Part 4](https://raw.githubusercontent.com/opengeospatial/ogcapi-common/3828187a8fc5e98386743afcdd0c0d650855956f/discovery/openapi/ogcapi-common-4.bundled.json) | `e090a94a65f443dfeac1a710037f9530499f14f5bf86be67b2a10549d4f0422c` |

Run the offline response contract checks with:

```sh
cargo test --locked -p server --test common_discovery
```

The suite validates real router responses against each bundle's `200`
`application/json` schema for `/`, `/conformance`, `/collections` and
`/collections/{collectionId}`. It covers EDR, Maps, raster Tiles, vector-only
Tiles and Features, including filtered/paged/empty lists, navigation, missing
extents, regular/irregular time grids and vertical metadata. Schema selection
uses these standard resource paths; current API mount prefixes belong to the
fixture setup and can change when discovery moves to a shared service root.

The test helper retains local component references, fails on missing/external
references, and enables format validation. It uses JSON Schema Draft 4 with
OpenAPI `nullable: true` converted to a nullable type in memory. The checked-in
bytes are never rewritten. Negative controls check required properties, nested
links, bbox lengths, timestamps, grid descriptors and counts; an open interval
checks nullable handling.

These are response-shape checks, not a complete conformance assessment. At this
commit the two bundles have identical response schemas; Part 4 adds discovery
parameters whose semantics remain covered by the behavioral tests. The bundles'
`/api` schema only requires an object, so this suite does not count that as
OpenAPI validation. HTML and error responses are not schema-validated here;
existing tests check their behavior. In particular, current errors use
`{code, description}`, while the bundles describe Problem Details with `type`.
The extent schema deliberately permits additional dimensions through an open
extension branch: accepting EDR vertical metadata does not prove Uniform
Additional Dimensions conformance. Part 4 conformance remains undeclared.

To update the baseline, download both files at an explicit upstream commit,
record the new source links/date/hashes here, review the schema and normative
changes, and run the contract suite. Review failures rather than relaxing the
schemas or dropping response fields for validation. Update the implementation
matrix's evidence separately; a bundle refresh does not establish full class
conformance or replace its historical specification assessment.

## OGC 2D Tile Matrix Set 2.0 (`tms-2.0/`)

The JSON schemas of [OGC 17-083r4](https://docs.ogc.org/is/17-083r4/17-083r4.html),
retrieved unmodified on 2026-09-25 from `https://schemas.opengis.net/tms/2.0/json/`:
`tileSet.json`, `tileMatrixSet.json` and the files they reference, transitively.
They are JSON Schema 2019-09 with relative `$ref`s; the test helper
(`crates/server/tests/common_discovery/tms.rs`) serves these copies for their
`schemas.opengis.net` URIs and fails on any other reference, so validation stays
offline. The contract suite validates every tileset (list entries and
resources) and tile matrix set the shared root and the per-API Tiles service
advertise.

| File | SHA-256 |
|---|---|
| `2DBoundingBox.json` | `e2a1c65a4d421846e68fcf0a6191b75788a9f96cfa580483c5b8ba81b3284fa5` |
| `2DPoint.json` | `4898b39f8c9ebcb17ff74f3c8f605eee3057ec8a664a0f3542f3042ea6ba3980` |
| `crs.json` | `c5a0d724c90b7f8b80344b7786451c66bd96deac991a8541878b1a688522e358` |
| `dataType.json` | `352451f8a4f620c0403abc6b2b2eef333fcfe3f4f2ae77eab46c1be5de0676e6` |
| `geospatialData.json` | `0be2437ea443a21c8dc849e87d84d2fd5affbf29f7e7e3bf126770c38ddb8e0b` |
| `link.json` | `97f618793ebe21dac725aa6155247c8ccc2e4b0794950bb493f0b060e88f3baa` |
| `projJSON.json` | `7e0397706ae6845686c316acaca72d7f10dab94e6d4f7e69a5906a58b55523a1` |
| `propertiesSchema.json` | `26f714dac852b5505686c9101edec499e01b99d0309629809fc8c90907bcf24d` |
| `style.json` | `0bb07c80ff0edb6a2e644477cf0d59d9a402fcb63ee86f661f6532f35ed83841` |
| `tileMatrix.json` | `df3887b96d5943ee31d2678156bb3fd3e7273e52a58ec84639ffff26c4ea3468` |
| `tileMatrixLimits.json` | `b1ae65b7fe4bd35d5fcb04fba895e3eee39b4e08ce864c2d96009a16bcd91607` |
| `tileMatrixSet.json` | `0d5d9564e5aa34d3531754fbe995788716365e2c5710c92a2d400ae7e509680a` |
| `tilePoint.json` | `5d636234baaf25c3c19c690f41eaae39eecf9cc2600b42f05a6bb0e5119e0ee6` |
| `tileSet.json` | `4ed13447f836656e9e1b30dccf1287002c3e4397c933da18886fe0a8e43caf0a` |
| `timeStamp.json` | `9bff92435baffa5079ab8e261fd1bddaf115fd025f1487891956b7940887d2f5` |
| `variableMatrixWidth.json` | `d4d7634ea976bfa405cfc808cf0d5c1b57a7170b6864f2d163fc10c8545753f1` |

To update, download the same closure from `schemas.opengis.net/tms/2.0/json/`,
record the date and hashes here, and run the contract suite.

## Other schemas

- `ogcapi-edr-1.1-bundled.json`: EDR collection response validation.
- `coveragejson.json`: CoverageJSON output validation.
- `edr-locations-geojson.json`: EDR location GeoJSON validation.
- `openapi-3.0.json`: generic OpenAPI document validation (Features `/api`).
