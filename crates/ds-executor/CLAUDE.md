# ds-executor — Claude instructions

Shared Tokio render admission/execution, with no axum or engine dependencies.
Do not put Tokio in ds-core or ds-render; core only carries the absolute
thread-local deadline. Capture it explicitly when fanning out onto Rayon.

The process-wide queue counts memory and CPU admission waiters; cache hits bypass it.
Raster admission reserves memory before CPU, sharing one absolute deadline.
No CPU slot is held while waiting for memory. CPU permits belong to workers,
never HTTP futures awaiting a running worker. Memory reservations are shared
between worker and handler through fallback encoding and survive cancellation. A timeout cannot
release a running worker's permit. Abort handles may cancel pending blocking
jobs but cannot preempt running CPU work. Keep tests for overload, canceled
waiters, deadline expiry, and worker permit ownership.

Raster default: MC_RENDER_TIMEOUT_MS=3000. Shared 3D points/meshes use 30 s.
MC_RENDER_QUEUE_CAPACITY defaults to 3× render concurrency; zero permits no
waiting. Global slots and queue survive reload. Metrics snapshot is exported
through server and its Grafana dashboard.
