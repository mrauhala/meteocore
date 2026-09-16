# ds-bufr — Claude instructions

Read README.md for provenance and the supported subset. Keep both upstream
licenses. Do not edit generated tables to introduce national descriptors;
centre/version dispatch belongs in engine-bufr/src/decode.rs.

Numeric values use i128 mantissas: the bit reader accepts up to 64 bits, but
adding the signed reference or an increment must not wrap. Missing numeric
increments use their own width mask. Character compressed lengths are bytes.

Validate format changes with independently encoded ecCodes fixtures, including
missing values, following-field alignment, and unsupported/truncated messages.
Run `cargo test -p ds-bufr -p engine-bufr` and workspace Clippy before committing.
