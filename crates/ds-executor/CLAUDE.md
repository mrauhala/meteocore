# ds-executor — Claude instructions

Shared Tokio render admission/execution and the one query runtime, plus the
engines' fail-fast source byte budget, with no axum or engine dependencies.
Do not put Tokio in ds-core or ds-render; core only carries the absolute
thread-local deadline. Capture it explicitly when fanning out onto Rayon.

The process-wide queue counts memory and CPU admission waiters; cache hits bypass it.
Raster admission reserves memory before CPU, sharing one absolute deadline.
`acquire_raster_planes` charges a render holding several value planes (an
RGB composite's bands) 16 B per output pixel per plane past the first
(`budget::raster_bytes`); `acquire_raster` is its one-plane case.
`acquire_layered_raster` admits a render that draws several layers into one
image (WMS GetMap with several `LAYERS`, #1181): it charges
`budget::layered_raster_bytes`, one layer's `raster_bytes` for the most
planes a layer holds plus 4 B per output pixel for the image below, never a
charge per layer, and its deadline is the render deadline once per layer
(`layered_timeout`). The layers run in turn in one slot.
No CPU slot is held while waiting for memory. CPU permits belong to workers,
never HTTP futures awaiting a running worker. The raster handlers move the
`Arc<RenderPermit>` into the job, so it survives HTTP cancellation; the job
also encodes the empty and error images (#1093), so nothing is encoded after
the worker returns. A timeout cannot release a running worker's permit. Abort handles may
cancel pending blocking jobs but cannot preempt running CPU work. Keep tests for overload, canceled
waiters, deadline expiry, and worker permit ownership.

Nor is a CPU slot held while a running job waits for work another job is
doing: `wait_without_slot` (#1155, a `CELLS` tile waiting for another
tile's cell-set build). The worker keeps its permit in a thread-local; the
call drops it, runs the wait, then takes a slot again behind the queued
renders, polling the semaphore on its own thread (no runtime needed;
`unconstrained`, or a query-runtime worker's task budget could hold the
permit back). Meanwhile the job holds a place in its admission queue, so
the queue still bounds admitted jobs and their blocking threads, and
`render_queue_depth` counts it; with the queue full it keeps its slot and
is never shed. Its memory reservation and deadline stay; a slot not retaken
by the deadline is `DeadlineExceeded`, counted once as `timed_out_render`
at the join. Call it only around a wait that uses no CPU. Pinned by
`a_job_waits_for_another_without_its_slot`,
`retaking_a_slot_ends_at_the_deadline` and
`a_job_keeps_its_slot_when_the_queue_is_full`.

Two ways to run admitted work, sharing one worker wrapper and one join:
`RenderJob::run`, a `spawn_blocking` thread of the caller's runtime, for a
render; and `RenderJob::run_query`, a task on the shared query runtime, for
a synchronous engine *query* such as WMS GetFeatureInfo's EDR position call
(#1064). An engine query may take `Handle::current()` and spawn: GRIB's
field jobs, the `DataStore` bridge. From `run`, that handle is the serving
runtime, so the blocking work lands on request workers (root CLAUDE.md
Critical Rule 7). From `run_query` it is the query runtime, where
`block_in_place` is valid. The slot, the deadline and the abort-before-start
semantics are the same for both.

The query runtime is ONE per process (#1103), and its API is stable:
`query_runtime()` returns its `&'static Handle`, `query_workers()` its worker
count (available CPUs, 2–8), and `QUERY_THREAD_NAME` (`mc-query`) names its
worker and blocking threads. api-edr runs every data query on it, under its
own admission (`query_workers()` running, 32 more waiting) and 30 s
deadline, a cross-section trajectory on its blocking pool; GetFeatureInfo
runs on it through `run_query`, under the render slots. Never build a
second query runtime for an API: share this one and keep the API's own
admission and deadline. Each API's admission bounds what it puts there; a
query waiting for a worker counts against its own deadline. Tests pin the
runtime by `Handle::id()` and the thread name:
`run_query_runs_engine_work_on_the_query_runtime` here, api-edr's
`queries_run_on_the_shared_query_runtime_under_the_edr_deadline`, and the
server's `edr_and_get_feature_info_share_one_query_runtime`.

`field_jobs::run_field_jobs` is an engine query's bounded field-read
scheduler (#1067: engine-grib's, shared with engine-netcdf): at most
`FIELD_CONCURRENCY` (4) jobs per query as tasks on the runtime the query
runs on (the query runtime), each inside `block_in_place` and re-entering
the caller's `ds_core::deadline`; on the first error or the deadline no
job starts, and the running ones drain before it returns. A caller outside
any runtime passes its fallback runtime (`field_jobs::block_on`); a caller
on a current-thread runtime, which cannot `block_in_place`, runs on the
fallback from a scoped thread carrying its deadline, so the future, the
field iterator and `consume` are `Send`. Pinned by `field_jobs::tests`;
keep it engine-neutral.

GetFeatureInfo admits with `RenderJob::acquire_feature_info`: the render
slots and queue, no memory (one point), under its own deadline,
`MC_FEATURE_INFO_TIMEOUT_MS` (10 000 by default, at most one day), not the
render deadline: a cold GRIB position query can need more than 3 s. As for
every job, the deadline starts at admission, so it covers the wait for a
slot as well as the query, and its expiries count in the same `queue` and
`render` stages as a render's. Pinned by
`feature_info_has_a_deadline_of_its_own` here and in api-wms's
`get_feature_info` tests.

Slot count: `render_concurrency()`, fixed by its first read — the server's
`[server] render_concurrency` via `set_render_concurrency` at boot, else
`default_render_concurrency()` (2× CPUs, min 8). Nothing may read the slots,
queue or metrics before the server sets it; tests of it need process isolation.
Raster default: MC_RENDER_TIMEOUT_MS=3000, once per layer for a layered
render. Shared 3D points/meshes use 30 s,
GetFeatureInfo MC_FEATURE_INFO_TIMEOUT_MS=10000.
MC_RENDER_QUEUE_CAPACITY defaults to 3× render concurrency; zero permits no
waiting. Global slots and queue survive reload. Metrics snapshot is exported
through server and its Grafana dashboard. `RenderTiming` is the response
extension raster handlers attach (registry collection id + fixed
`RenderOutcome`); server records it as `render_duration_seconds` (#466).
Its `RenderPhases` (fixed `RenderPhase`: queue, engine, assemble, encode)
become `render_phase_seconds` (#147); a phase a render skipped stays unset
and records nothing, so a hit carries none. Expired deadlines count by stage
(`timed_out_queue` waiting for memory or a slot, `timed_out_render` after
admission) and memory rejections by reason (`rejected_oversize` is an
immediate 503, `rejected_deadline` an expired memory wait): the counters
must keep immediate sheds and deadline expiries apart.

`budget::ByteBudget` / `BytePermit` is the engines' source-read admission,
not render admission: engine-geotiff's `decode_budget::BUDGET`
(`MC_GEOTIFF_DECODE_MEMORY_MB`) and engine-zarr's `read_budget::BUDGET`
(`MC_ZARR_READ_MEMORY_MB`) are instances, and the next engine's budget is
another one, never a copy (#1067). Outside the engines, api-edr's
`response_budget` (`MC_EDR_RESPONSE_MEMORY_MB`, #1041) admits response
bodies on the same type. Keep it apart from `RenderBudget`: it
never waits, because its callers hold an executor slot and may hold another
reservation. `reserve` fails at once with `ResourceExhausted` and counts a
rejection; `try_reserve`, a batch's probe for one more slot, and
`try_reserve_background`, at most half the capacity (#1004), never count
one. It checks no deadline and reads no environment: the engine owns its
static, env var, default, deadline checks and planning, and exports
`metrics()` (used, capacity, rejected) to `/metrics` unchanged.
