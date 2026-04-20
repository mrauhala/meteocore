//! Parameterized SQL builders.
//!
//! Every emitted SELECT ends in `LIMIT 10001` (locations: `LIMIT 1001`),
//! every identifier goes through `quote_ident`, every value is bound as
//! `$1..$n`. No string interpolation of request data. Implemented in #104.
