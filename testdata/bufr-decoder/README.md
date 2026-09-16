# BUFR decoder regression fixtures

Synthetic format fixtures generated with **ecCodes 2.47.0** from its BUFR4 sample.
These are not observed or operator-confirmed weather examples. The deliberately
huge pressure in two fixtures exercises a raw numeric value above u32.

Regenerate from this directory (adjust sample path for your installation):

```sh
for filter in *.filter; do
  bufr_filter -o "${filter%.filter}.bufr" "$filter" /opt/homebrew/share/eccodes/samples/BUFR4.tmpl
done
bufr_dump -p compressed-strings-wide.bufr
```

| Fixture | Expected decoded content |
| --- | --- |
| compressed-strings-wide | Three subsets; NORTH/SOUTH/EAST STATION; 40-character width through 208040; pressures 101300/101310/101320 Pa encoded in 46 bits; temperatures 280.15/281.15/282.15 K |
| constant-string-missing-increment | Three subsets with SAME STATION (zero compressed string byte count); middle temperature missing, others 280.15/282.15 K |
| short-string-wide-number | One subset, ABC encoded in 24 bits through 208003; pressure 42949672960 Pa (raw 4294967296); temperature 283.15 K |
| dwd-local-hour | Same numeric/string checks plus centre 78, local version 8, 004214 actual observation hour 10 before latitude/longitude |

Filters are the complete independent encoding recipe; engine integration tests
assert decoded station IDs, names, coordinates, times, and observation values.
DWD 004214 is verified against ecCodes local tables 2–8 for centre 78; existing
020237/238/239 entries occur in local table 8. Tests also reject applying these
entries to another centre or unsupported local version.
