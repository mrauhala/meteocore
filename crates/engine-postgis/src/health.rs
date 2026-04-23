//! Per-engine health state and ping task.
//!
//! `SELECT 1` at construction with a 2s deadline sets initial status;
//! a 30s background ping flips between `ready` and `degraded`. Implemented
//! in #110 alongside the /metrics surface.
