# Plan amendment — storm-cell intelligence, after the 2026-08-24 field test

Amends the storm-cell intelligence plan against a field-test specification
written by a model that consumed the live `/mcp` surface across 2026-08-24
frames 09:55Z–13:15Z.

Tracked as epic #624; the Phase 3.5 items below are #620–#623.

The original plan's phases stand. What the field test changes is **the order**
and **one structural assumption**. Nothing below invalidates Phases 1–3, which
shipped; it re-scopes what comes next.

---

## What the test confirmed

The plan's own bets held up under a real consumer:

- **`significance_reasons` is the most valuable field in the payload.** The
  plan argued explainability was load-bearing rather than decorative and put
  `contributions` in the core type for it. The test's verdict: *"the strongest
  feature in the payload — genuine per-cell explainability. It needs a fix,
  not replacement."*
- **Track identity is sound.** Cell 95 cross-checked over 90 minutes: 50.8 km
  displacement, bearing 153°, implying 9.4 m/s against a reported 9–10 m/s,
  `track_age` advancing exactly 18 frames. The tracker's association and
  motion estimates are internally consistent.
- **Retention depth is adequate** — 20-frame tracks observed, walks
  terminating on sample count rather than a retention wall.
- **Ranking heuristic framing survived contact.** The test never mistook
  significance for a warning, which is what the disclaimer plumbing was for.

## What it falsified

**The plan assumed the significance model's weakness was calibration. It is
not. It is that the model has no notion of whether a cell is meteorological
at all.**

The plan named this once — `beam_quality` as "a discount, not a bonus" — and
treated it as a Phase 5 concern arriving with the volume join. The field test
shows it is the *first* problem, not a later refinement:

> The location 65.315N 26.07E produced a rank-1 "very_severe" cell **four
> times in three hours under three different track ids** (1, 93, 226), never
> displacing more than ~1 km, intensity oscillating 49–63.5 dBZ, zero
> lightning throughout. Saarijärvi (62.78N 25.20E) did the same under ids 2
> and 229.

### Why the shipped mitigation cannot catch this

`ds_core::cell_facts::is_likely_clutter` (#614/#615) requires
`speed_ms.is_some() && age >= CLUTTER_MIN_AGE` (6 frames). Every redetection
arrives as a **fresh track at age 1 with no velocity**, so the function
returns `false` on exactly the cells it was written for.

This is not a tuning problem. **The tracker is structurally blind to
recurrence at a location**, because recurrence is defined across track
identities and the tracker only sees one identity at a time. No per-frame,
per-track field can express it.

### The correction

A **rolling site-level echo climatology**, maintained per grid cell,
independent of the cell tracker: how often has this pixel produced an echo
over the last 24 h, and over the last 30 d? A fixed target approaches 1.0.
Attach the value at **detection** time, so it survives the track-id change
that currently hides the problem.

Two windows, not one — transient anaprop ducting and permanent targets (masts,
wind farms, terrain) have different signatures and warrant different
treatment.

This is the highest value-per-unit-effort item now open, and it is a
prerequisite for the ranking being trustworthy at all. It moves ahead of the
LLM lane entirely.

### A trap found while fixing the related defects

Making an unknown flag **absent** from the term set (rather than 0.0) looks
like the right application of the plan's "absent terms renormalize" rule. It
is not, for per-cell missingness: dropping a term shrinks the denominator, so
every remaining term weighs more and the cell scores **higher**. A re-detected
fixed echo is always a newborn, so renormalizing on unknown motion would have
*promoted* precisely the clutter this work is trying to demote.

The rule needs splitting, and the plan should say so:

- **Source not wired** → absent, renormalize. Affects every cell equally, so
  no cell gains a relative advantage. This is the case the rule was written
  for.
- **This cell lacks the data** → present at 0.0. No bonus, no claim. The
  payload still reports `null`; the *scorer* must not reward not knowing.

## What it added that the plan never considered

**Domain and coverage marking (spec D5/D6).** The plan treated the composite
domain as a given. The test found cells over Russian Karelia, off Latvia, off
Saaremaa, and over Estonia with nothing distinguishing them from Finnish
cells — and the day's largest cell (162, 28.7 km², 50.5 dBZ) carrying a null
impact label purely because it sat over the Gulf of Finland.

This governs **data availability, not just labelling**: outside the composite,
ancillary joins do not apply, and `flash_count: 0` asserts an absence that was
never observed. Lightning ingest was confirmed working that day — the zeros
were genuine *inside* Finnish coverage and would have been wrong outside it.

The plan's tri-state discipline was written for "is a source wired" and
"did the join run". It needs a third axis: **is this cell inside the source's
coverage**. Same class of bug as the IC/CG gating (#618), one level up.

**Footprint before impact.** The plan has impact matching as Phase 2
(shipped, centroid-in-polygon). The test's judgement is that centroid-based
impact is a convenience label rather than impact analysis, and that
`footprint` is the prerequisite for the real thing. That aligns with #551 /
#408, already in the backlog — the amendment is that footprint should be
understood as blocking *impact*, not merely as a nicer geometry.

---

## Revised phase order

| Was | Now | Rationale |
|---|---|---|
| Phase 4 — NWP environment | **Deferred** | Cannot interpret radar you don't trust. Environment normalizes Block C fields that do not exist yet. |
| Phase 5 — 2D↔3D volume join | **Split.** Quality/provenance half moves first | `nearest_radar_distance_km`, `beam_height_m`, `beam_blockage_fraction` are Block A prerequisites, cheaper than the full join, and directly serve clutter rejection. |
| Phase 6 — `ds-llm` job runner | **Deferred, unchanged in design** | The plan's own logic: the LLM narrates the fact sheet, so a fact sheet that ranks a wind farm first produces fluent prose about a wind farm. Narration must not precede trust. |
| — | **New Phase 3.5 — trust the payload** | Site climatology + quality gating + the null-contract defects. Everything below. |

### Phase 3.5 — makes the existing product trustworthy

1. **Site echo climatology** (24 h + 30 d windows, per grid cell, tracker-
   independent) → `site_echo_frequency`, `recurrence_count_at_location`,
   `static_target_probability`. Attached at detection.
2. **`nonmet_probability`** as an explicit suppressor of `significance`,
   superseding the `likely_clutter` heuristic rather than tuning it.
3. **Beam geometry from the polar volumes** — the Block A subset that does not
   need the full cell join.
4. **Coverage marking** — `in_composite_domain`, `country_code`,
   `lightning_coverage`; null every ancillary join outside its coverage.
5. **Null-contract defects** (D1, D7, `retained_frames`, `stopped_because`) —
   cheap, independent, and correctness rather than polish.
6. **Severity hysteresis** — the binner has none, so ±2 dB of `max_dbz` noise
   renders as a storm exploding and collapsing every five minutes (cell 156:
   nine severity changes across one coherent 50-minute track).

### Unchanged

Blocks B (dual-pol), C (vertical structure), D (Doppler) and the hazard
products above them remain correctly ordered in the spec's own P1/P2, and
remain gated on ingest MeteoCore does not have yet. The spec's rule —
*"`mesh_mm` / `posh` / `poh` must not be published without the ingredient
fields above them"* — is the same principle as this plan's numeric guard on
narration, and should be adopted verbatim.

The LLM lane's design needs no revision. Its position in the queue does.
