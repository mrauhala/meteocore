"""
MeteoCore load test with Locust.

Run:
  /tmp/locust-env/bin/locust -f locustfile.py --host https://meteocore.app.meteo.fi

Then open http://localhost:8089 to configure users and start the test.

Headless mode (50 users, 5/s spawn rate, 2 minutes):
  /tmp/locust-env/bin/locust -f locustfile.py --host https://meteocore.app.meteo.fi \
    --users 50 --spawn-rate 5 --run-time 2m --headless
"""

import random
from locust import HttpUser, task, between

# Collections available on the server
LAYERS = [
    "fmi-radar-composite-dbz",
    "smhi-radar-composite-dbz",
    "dmi-radar-composite-dbz",
    "met-radar-composite-dbz",
    "dwd-radar-composite-dbz",
    "opera-reflectivity",
    "opera-precipitation",
]

# Tile matrix sets
TILE_LAYERS = [
    "fmi-radar-composite-dbz",
    "smhi-radar-composite-dbz",
    "dmi-radar-composite-dbz",
    "met-radar-composite-dbz",
    "dwd-radar-composite-dbz",
]

# Bboxes in EPSG:3857 (Web Mercator) covering Nordic/European radar areas
BBOXES_3857 = [
    # Finland
    (2200000, 8200000, 3600000, 10200000),
    # Sweden
    (1000000, 7500000, 2600000, 10000000),
    # Denmark
    (700000, 7200000, 1800000, 8200000),
    # Norway
    (400000, 7800000, 2000000, 10800000),
    # Nordic overview
    (400000, 7200000, 3800000, 11200000),
    # Central Europe
    (-200000, 5800000, 2400000, 7600000),
    # Europe wide (OPERA)
    (-2000000, 4000000, 4500000, 11500000),
]

# Bboxes in CRS:84 (lon/lat)
BBOXES_CRS84 = [
    (19.0, 59.0, 32.0, 70.5),   # Finland
    (10.0, 55.0, 24.0, 69.0),   # Sweden
    (7.0, 54.0, 16.0, 58.0),    # Denmark
    (4.0, 57.0, 18.0, 72.0),    # Norway
    (-5.0, 47.0, 35.0, 72.0),   # Europe wide
]

# WebMercatorQuad tile coordinates (zoom, row, col) covering Nordic area
TILES = [
    # Zoom 4 (overview)
    (4, 4, 8), (4, 4, 9), (4, 3, 8), (4, 3, 9),
    # Zoom 6 (country level)
    (6, 17, 35), (6, 17, 36), (6, 18, 35), (6, 18, 36),
    (6, 16, 33), (6, 16, 34),
    # Zoom 8 (regional)
    (8, 70, 143), (8, 70, 144), (8, 71, 143), (8, 71, 144),
    (8, 68, 140), (8, 69, 140),
    # Zoom 10 (city level)
    (10, 282, 574), (10, 282, 575), (10, 283, 574), (10, 283, 575),
]

# Only use "default" — not all layers have all styles configured
STYLES = ["default"]
WIDTHS = [256, 512, 1024, 2048]
HEIGHTS = [256, 512, 1024, 1400]


class MeteoCoreUser(HttpUser):
    wait_time = between(0.5, 2)

    # --- WMS ---

    @task(10)
    def wms_getmap_3857(self):
        """WMS GetMap in EPSG:3857 — the most common real-world request."""
        layer = random.choice(LAYERS)
        bbox = random.choice(BBOXES_3857)
        w = random.choice(WIDTHS)
        h = random.choice(HEIGHTS)
        style = random.choice(STYLES)
        self.client.get(
            f"/wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap"
            f"&LAYERS={layer}&CRS=EPSG:3857&STYLES={style}"
            f"&BBOX={bbox[0]},{bbox[1]},{bbox[2]},{bbox[3]}"
            f"&WIDTH={w}&HEIGHT={h}&FORMAT=image/png&TRANSPARENT=TRUE",
            name="/wms GetMap EPSG:3857",
        )

    @task(3)
    def wms_getmap_crs84(self):
        """WMS GetMap in CRS:84."""
        layer = random.choice(LAYERS)
        bbox = random.choice(BBOXES_CRS84)
        w = random.choice([256, 512])
        h = random.choice([256, 512])
        self.client.get(
            f"/wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap"
            f"&LAYERS={layer}&CRS=CRS:84&STYLES=default"
            f"&BBOX={bbox[0]},{bbox[1]},{bbox[2]},{bbox[3]}"
            f"&WIDTH={w}&HEIGHT={h}&FORMAT=image/png&TRANSPARENT=TRUE",
            name="/wms GetMap CRS:84",
        )

    @task(2)
    def wms_getmap_4326(self):
        """WMS GetMap in EPSG:4326 (swapped axis order)."""
        layer = random.choice(LAYERS)
        bbox = random.choice(BBOXES_CRS84)
        self.client.get(
            f"/wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap"
            f"&LAYERS={layer}&CRS=EPSG:4326&STYLES=default"
            f"&BBOX={bbox[1]},{bbox[0]},{bbox[3]},{bbox[2]}"
            f"&WIDTH=512&HEIGHT=512&FORMAT=image/png&TRANSPARENT=TRUE",
            name="/wms GetMap EPSG:4326",
        )

    @task(1)
    def wms_getcapabilities(self):
        """WMS GetCapabilities — lightweight metadata request."""
        self.client.get(
            "/wms?SERVICE=WMS&REQUEST=GetCapabilities",
            name="/wms GetCapabilities",
        )

    @task(1)
    def wms_getlegend(self):
        """WMS GetLegendGraphic."""
        layer = random.choice(LAYERS)
        style = random.choice(STYLES)
        self.client.get(
            f"/wms?SERVICE=WMS&REQUEST=GetLegendGraphic"
            f"&LAYER={layer}&STYLE={style}&WIDTH=40&HEIGHT=200",
            name="/wms GetLegendGraphic",
        )

    # --- Tiles ---

    @task(8)
    def tiles_get(self):
        """OGC API Tiles — WebMercatorQuad tile request."""
        layer = random.choice(TILE_LAYERS)
        z, row, col = random.choice(TILES)
        self.client.get(
            f"/tiles/collections/{layer}/tiles/WebMercatorQuad/{z}/{row}/{col}?f=image/png",
            name=f"/tiles z={z}",
        )

    @task(2)
    def tiles_styled(self):
        """OGC API Tiles — styled tile request."""
        layer = random.choice(TILE_LAYERS)
        z, row, col = random.choice(TILES)
        style = random.choice(STYLES)
        self.client.get(
            f"/tiles/collections/{layer}/styles/{style}/tiles/WebMercatorQuad/{z}/{row}/{col}?f=image/png",
            name=f"/tiles styled z={z}",
        )

    # --- Maps ---

    @task(3)
    def maps_get(self):
        """OGC API Maps — map request."""
        layer = random.choice(TILE_LAYERS)
        bbox = random.choice(BBOXES_CRS84)
        self.client.get(
            f"/maps/collections/{layer}/map"
            f"?bbox={bbox[0]},{bbox[1]},{bbox[2]},{bbox[3]}"
            f"&width=512&height=512&f=image/png",
            name="/maps GetMap",
        )

    # --- Metadata ---

    @task(1)
    def tiles_collections(self):
        """Tiles collection listing."""
        self.client.get("/tiles/collections", name="/tiles/collections")

    @task(1)
    def health(self):
        """Health check."""
        self.client.get("/health", name="/health")

    @task(1)
    def metrics(self):
        """Prometheus metrics."""
        self.client.get("/metrics", name="/metrics")
