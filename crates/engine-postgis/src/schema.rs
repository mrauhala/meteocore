//! Schema-mapping DSL and identifier whitelist helpers.
//!
//! Three shapes — `long` (EAV), `wide` (column-per-param), `per_parameter`
//! (table-per-param) — all validated at config load into typed structures
//! the query builder uses directly. Implemented in #101.
