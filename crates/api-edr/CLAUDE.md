# api-edr crate — Claude Instructions

OGC API - EDR HTTP layer. Read the root `CLAUDE.md` first.

## CoverageJSON schema compliance (critical)

All CoverageJSON output MUST validate against the OGC CoverageJSON 1.0 schema
at `schemas/coveragejson.json`. Integration tests in
`tests/covjson_validation.rs` validate against it.

**When modifying `src/response.rs` or adding new CoverageJSON output, always
run `cargo test -p api-edr`.**

Key schema rules:

- **Coverage**: requires `type` ("Coverage"), `domain`, `parameters`,
  `ranges`.
- **Domain**: requires `type` ("Domain"), `axes`, `referencing`.
  `domainType` triggers axis constraints.
- **PointSeries**: x/y single-value axes, t string-values axis, z optional
  single-value axis.
- **Grid**: x/y numeric-values axes, t and z optional. NdArray shape:
  `[t, y, x]`, `[y, x]`, `[z, y, x]`, or `[t, z, y, x]`.
- **VerticalProfile**: x/y single-value, z numeric-values axis, t optional
  single-value. NdArray shape: `[z]`.
- **Parameter**: requires `observedProperty` with `label` as an i18n object
  (`{"en": "..."}`).
- **NdArray**: requires `shape` and `axisNames` when values has >1 item;
  `values.length` must equal the product of `shape`.
- **i18n objects**: keys must be BCP 47 language tags (e.g. `"en"`).
- **Reference systems**: spatial uses `GeographicCRS` with the CRS84 id;
  temporal uses `TemporalRS` with `"Gregorian"` calendar.

### Adding a new domain type

1. Add a variant to the `DomainDescription` enum in `ds-core/src/model.rs`.
2. Add a match arm in `build_domain()` in `src/response.rs`.
3. Check the schema's `domainBase.dependencies.domainType` for axis
   requirements.
4. Add a validation test in `tests/covjson_validation.rs`.

Currently implemented: `PointSeries`, `Grid`, `VerticalProfile`.

## Instances (forecast model runs, #337)

The shared machinery is `ds_core::instances` (see root CLAUDE.md). This crate
owns the instance-id string form:

- Routes: `GET /collections/{id}/instances`, `/instances/{instanceId}`,
  `/instances/{instanceId}/{position,area}`. No-instance routes default to
  the latest run.
- Collection metadata gains an `instances` data_query, and the OpenAPI spec
  advertises the instance paths — both gated on `get_instances()` being
  non-empty.
- Unknown instance id → 404 (`select_run` returns `None`).

## Misc

- Cross-section responses (`query_trajectory`, ODIM PVOL) are CoverageJSON
  `Section` with a composite `[t,x,y]` axis + numeric `z` axis.
- `f=png` time-series plots are why this crate (alone among API crates)
  depends on ds-render.
- A location with no data in the requested window returns `LocationNotFound`
  → 404 (an empty PointSeries would fail schema validation).
- When adding endpoints or params, update `api_definition()` in
  `src/handlers.rs` (OpenAPI).
