"""
Saturation test: same task set as locustfile.py, but with no think time
and a stepped ramp to find the p95 cliff.

Run from the repo root so `locustfile.py` is importable:
  /tmp/locust-env/bin/locust \
    -f docs/performance/baseline-2026-04-data/saturation_locustfile.py \
    --host https://meteocore.app.meteo.fi --headless
"""

import sys
from pathlib import Path

# Resolve the repo root relative to this file so the import works regardless
# of the caller's cwd: this file lives at docs/performance/<dir>/<file>.py,
# so the repo root is three parents up.
_REPO_ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(_REPO_ROOT))

from locust import constant, LoadTestShape
import locustfile as _base

# Override wait_time on the existing User class so locust only discovers one.
_base.MeteoCoreUser.wait_time = constant(0)
MeteoCoreUser = _base.MeteoCoreUser


class StepLoadShape(LoadTestShape):
    stages = [
        (60,   50,  50),
        (120,  100, 50),
        (180,  200, 100),
        (240,  400, 200),
    ]

    def tick(self):
        t = self.get_run_time()
        for end, users, rate in self.stages:
            if t < end:
                return (users, rate)
        return None
