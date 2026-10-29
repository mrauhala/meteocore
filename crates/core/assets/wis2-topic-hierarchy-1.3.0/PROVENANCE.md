# WIS2 Topic Hierarchy 1.3.0

The earth-system-discipline levels of the WMO WIS2 Topic Hierarchy, against
which `ds_core::discovery` validates a collection's WIS2 topic (#1356).

- Release: https://github.com/wmo-im/wis2-topic-hierarchy/tree/1.3.0
- Source: `topic-hierarchy/earth-system-discipline`, one `index.csv` per
  level (two `index-flat.csv` for the space-based observations' satellites
  and instruments)
- Licence: Apache-2.0, `LICENSE` copied from the release

`earth-system-discipline.tsv` is generated from those files by
`scripts/vendor-wis2-topic-hierarchy.py 1.3.0`: one row per valid path, its
status and its description, sorted by path. It is a flattened form, not a
copy, and is not edited by hand. Two upstream quirks are handled there:
the rows of `weather/maritime-safety-information` lack the empty Source
column, and `weather/space-based-observations/goes-14/imager` is listed twice,
the second time described as the sounder; the first row is kept.

- `earth-system-discipline.tsv`: SHA-256 `ce28271a0136113c62d3e42f9a2316372bc4a618b6e04c2bcdfdcd6bf2f2c0a8`
- `LICENSE`: SHA-256 `c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4`
