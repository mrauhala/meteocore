# BUFR live-failure regressions (#1008)

Real WIS2 SYNOP messages that the decoder failed on, one small file per
cause, fetched from public WIS2 Global Caches on 2026-10-10. Every file
decodes element by element exactly as ecCodes 2.47.0 `bufr_dump` does (a
non-UTF-8 name byte excepted: ecCodes prints `?`, this decoder U+FFFD). The
engine tests in `crates/engine-bufr/tests/integration.rs` assert values taken
from `bufr_dump -p` of the same files.

Two files are single subsets that ecCodes extracted from a larger bulletin
(`set extractSubset = N; set doExtractSubsets = 1; write;` with
`bufr_filter`): the original bulletins are 16 KB each. The values are the
producer's; only the packing is ecCodes'.

| File | Source | Failed with | Cause |
| --- | --- | --- | --- |
| `ca-eccc-msc_307091_compressed.bufr` | ca-eccc-msc `ISAA03_CWAO_100300_RRC`, 3 compressed subsets behind a GTS heading | `failed to fill whole buffer` | compressed delayed replication factor read without its six-bit increment width (also ru-roshydromet, kz-kazhydromet, by-belgidromet) |
| `kz-kazhydromet_307080_compressed_nul-padded-name.bufr` | kz-kazhydromet `WIGOS_0-398-0-36691_20261010T030000` | `Invalid character data` | station name is 11 bytes of `0xFF` then NUL padding, missing to ecCodes; also compressed delayed replication. Its position is missing, so the subset is skipped (`no_position`) |
| `jp-jma_307080_master-v13.bufr` | jp-jma `A_ISIL01RJTD092100_C_RJTD_20261009212012_120` | `failed to fill whole buffer` | master table version 13: the 302045 radiation elements were narrower before version 14 (also it-meteoam; il-ims `international_*` failed the same way as `Invalid character data`) |
| `bb-barbadosmetservices_307083.bufr` | bb-barbadosmetservices `WIGOS_0-52-130-78954_20261009T210000` | `failed to fill whole buffer` | the generated Table D had lost 307083's leading `301090 302031` |
| `cy-dom_307092_associated-fields.bufr` | cy-dom `A_ISAD59LCLK092040_C_LCNC_20261009204700`, subset 1 of 54 | `Operator descriptor 2 04 018 not supported` | 307092 uses associated fields (204018 + 031021) |
| `il-ims_203-changed-reference.bufr` | il-ims `aws_2026100921.bufr`, subset 80 of 83 (Zemah, −200 m) | `Operator 2 03 014 not supported` | `203014 007030 007031 203255` redefines the height references to −5000 |
| `cl-meteochile_non-ascii-name.bufr` | cl-meteochile `synop-onehours/WIGOS_0-152-0-320049_20261009T210000` | `Invalid character data` | station name ends in byte `0xDB`, not ASCII or UTF-8 |
| `de-dwd_local-v8.bufr` | one message of a de-dwd `bda01,synop_bufr_GER` bulletin, local table version 8 | `Table B entry not found` for 004215 | DWD local elements beyond those registered before |
| `de-dwd_synop-and-supplement.bufr` | the SYNOP message (local version 0) and the national supplement (local version 8) of station 10022 from one de-dwd bulletin, subset 1 of each, concatenated in bulletin order | `Table B entry not found` for 020193 | the supplement starts with DWD local 020193; decoded, it must merge with the SYNOP report rather than replace it |

cy-dom also publishes `NIL` bulletins, three bytes of text, which stay
`not a BUFR message` (`kind="not_bufr"`); the tests use an inline `b"NIL"`.
