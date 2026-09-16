# ds-bufr

Internal BUFR decoder derived from [tinybufr 0.1.3](https://crates.io/crates/tinybufr/0.1.3)
([upstream](https://github.com/ciscorn/tinybufr)), distributed under the original
MIT OR Apache-2.0 licenses retained here. Source copied from the published
0.1.3 crate; its `.cargo_vcs_info.json` records commit
`9108eada5c73c866efaabf794c8da390966bc8c0` with `dirty: true`, so the published
crate archive is the authoritative baseline. Generated WMO tables are unchanged.
Unused Arrow integration/examples and upstream development dependencies are omitted.
The upstream JMA local-table module is retained behind the optional `jma` feature
for provenance; no workspace crate enables it or installs those tables. It does
not imply support for JMA feeds in the server.

Local changes for #693:

- Identify character fields by Table B unit, including strings under 32 bits.
- Decode compressed strings: reference field + six-bit **byte** count; zero
  repeats the reference, otherwise read one string per subset.
- Implement 208YYY character widths (bytes), with 208000 cancellation.
- Read numeric fields through 64 bits with i128 mantissas to preserve unsigned
  values plus signed references and compressed increments without wrapping.
- Recognize compressed missing values from the increment's own all-ones mask.
- Exclude character/code/flag fields from numeric width/scale operators, and
  reset operator state between uncompressed subsets.
- Bound reads to section 4; reject truncated delayed replication descriptors.
- Clone master tables for engine-owned centre/version-specific local registries.

The engine-facing boundary remains `engine-bufr/src/decode.rs`. This is not a
complete BUFR implementation: operators 203/204/207/22x remain unsupported.
Operator 206 is accepted only when the following local descriptor has a known
Table B entry: decoding uses that entry's width. Skipping unknown local fields
using the 206 width is unsupported; descriptor resolution rejects them before
reading data. The inherited unused pending-operator state has been removed.
Master-table version selection and general national-table loading are unchanged.
Do not infer that every live WIS2 message is supported from these format tests.

Independent encoding checks use ecCodes 2.47.0; fixtures and regeneration filters
are in `testdata/bufr-decoder/`. The compressed string layout is also checked
against [ecCodes BufrDataArray decode_string_array](https://github.com/ecmwf/eccodes/blob/develop/src/eccodes/accessor/BufrDataArray.cc).
