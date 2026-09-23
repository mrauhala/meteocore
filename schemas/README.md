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

## Other schemas

- `ogcapi-edr-1.1-bundled.json`: EDR collection response validation.
- `coveragejson.json`: CoverageJSON output validation.
- `edr-locations-geojson.json`: EDR location GeoJSON validation.
- `openapi-3.0.json`: generic OpenAPI document validation (Features `/api`).
