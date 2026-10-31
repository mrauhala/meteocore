# MeteoCore Helm chart

Runs [MeteoCore](https://github.com/mrauhala/meteocore), a server for weather
data over OGC API - EDR, Features, Maps and Tiles, OGC WMS 1.3.0 and OGC 3D
Tiles, from its container image `ghcr.io/mrauhala/meteocore`.

The chart's version is the server's: chart X.Y.Z runs image X.Y.Z by
default. Each release publishes it on two channels.

```bash
# OCI, on ghcr.io beside the image
helm install meteocore oci://ghcr.io/mrauhala/charts/meteocore --version <version>

# The classic repository, an index on GitHub Pages pointing at the chart
# attached to each GitHub release
helm repo add meteocore https://mrauhala.github.io/meteocore
helm install meteocore meteocore/meteocore --version <version>

# From a checkout
helm install meteocore charts/meteocore
```

Then:

```bash
kubectl port-forward svc/meteocore 8000:8000
curl http://localhost:8000/health
```

## What the defaults run

- One pod of a Deployment, the server listening on port 8000 behind a
  ClusterIP Service.
- A sample collection, `sample-observations`: hourly observations from three
  Finnish weather stations on 1 January 2026, from the Finnish Meteorological
  Institute's open data (CC BY 4.0), served over EDR and Features. Turn it off
  with `sample.enabled=false` once you have collections of your own.
- uid and gid 999, the image's `dataserver` user, with a read-only root
  filesystem, no capabilities and the `RuntimeDefault` seccomp profile. The
  pod passes the `restricted` Pod Security Standard.
- No ServiceAccount token: the server never calls the Kubernetes API.

## Configuration files

The server reads `/data/config.toml`. Relative paths in it resolve against
`/data`, where the chart mounts:

| Path | From |
|---|---|
| `/data/config.toml` | `config.toml`, or the `existingConfigMapKey` of `config.existingConfigMap` |
| `/data/collections.d` | `collections.files`, the ConfigMaps in `collections.existingConfigMaps`, and the sample collection |
| `/data/colormaps.d` | `colormaps.files` and the ConfigMaps in `colormaps.existingConfigMaps` |
| `/data/sample` | the sample collection's CSV |
| `/var/lib/meteocore` | `state_dir`: an emptyDir, or a volume per pod (see [State](#state-and-replicas)) |
| `/tmp` | an emptyDir |

The server starts with `--config /data/config.toml --host 0.0.0.0 --port
<containerPort>`; the flags win over `[server] host` and `port`, which the
config format still requires. `extraArgs` adds more, such as
`["--collections", "radar,nwp"]`.

The default `config.toml`:

```toml
[server]
host = "0.0.0.0"
port = 8000
collections_dir = "collections.d"
# colormaps_dir = "colormaps.d"
state_dir = "/var/lib/meteocore"
```

A `config.toml` of your own replaces it whole, so keep the lines your
setup uses: `collections_dir` whenever there are collection files,
`colormaps_dir` with palette files, `state_dir` for push-fed collections. The
chart fails to render when collection or palette files would be mounted where
the config does not look, and when `collections.files` repeats the sample
collection's file name.

Top-level settings that the README's Configuration section says belong in
`config.toml`, such as `[[style_bundles]]`, `[[colormaps]]`,
`[[parameter_defaults]]`, `[mcp]` and `[discovery]`, go in this value too.

```yaml
config:
  toml: |
    [server]
    host = "0.0.0.0"
    port = 8000
    collections_dir = "collections.d"
    colormaps_dir = "colormaps.d"
    state_dir = "/var/lib/meteocore"
    base_url = "https://weather.example.com"

    [[style_bundles]]
    id = "radar_multi"
    [style_bundles.default]
    colormap = "radar_dbz"

collections:
  files:
    radar.toml: |
      id = "radar"
      title = "Radar reflectivity"
      description = "Radar reflectivity composites, every 5 minutes."
      engine_type = "geotiff"
      apis = ["edr", "wms", "maps", "tiles"]
      [geotiff]
      filename_template = "radar_%Y%m%dT%H%MZ.tif"
      parameter = "reflectivity"
      unit = "dBZ"
      endpoint = "https://s3.example.com"
      bucket = "radar-data"
      prefix_pattern = "%Y/%m/%d/"

sample:
  enabled: false
```

File names in `collections.files` and `colormaps.files` are ConfigMap keys:
letters, digits, `-`, `_` and `.`. A ConfigMap holds at most 1 MiB, so keep
large palette sets or many collections in ConfigMaps of their own and list
them in `existingConfigMaps`. Their file names must not repeat.

### Data

Object-store and HTTP sources need nothing from the chart. Local files come
in through `extraVolumes` and `extraVolumeMounts`, at the paths the collection
configs name:

```yaml
extraVolumes:
  - name: radar
    persistentVolumeClaim:
      claimName: radar-data
      readOnly: true
extraVolumeMounts:
  - name: radar
    mountPath: /meteo/data
    readOnly: true
```

Mount them read-only. Local GeoTIFFs are memory-mapped, so whoever writes
them must replace files by atomic rename, never overwrite them in place.

## Secrets

The chart renders no secret values. Reference existing Secrets in `extraEnv`
or `envFrom`:

```yaml
extraEnv:
  # Bearer token for POST /admin/collections/reload and GET /admin/cache-plan
  - name: ADMIN_TOKEN
    valueFrom:
      secretKeyRef:
        name: meteocore
        key: admin-token
  # The variable [mcp] token_env names
  - name: MCP_TOKEN
    valueFrom:
      secretKeyRef:
        name: meteocore
        key: mcp-token
  # The variable a PostGIS collection's dsn_env names
  - name: OBS_DATABASE_URL
    valueFrom:
      secretKeyRef:
        name: meteocore
        key: obs-dsn
```

An `s3://` data path reads the standard `AWS_*` variables, region, endpoint
and credentials, from the environment.

`env` holds plain variables, name to value: `LOG_FORMAT`, `RUST_LOG`, the
`MC_*` cache and budget sizes. Set a name to `null` to drop one of the
defaults.

## State and replicas

`state_dir` holds what push-fed collections, WIS2 CAP alert sets and WIS2 BUFR
report stores, have accumulated, so a restart does not start them empty.

- By default it is an emptyDir: it survives a container restart but not a
  new pod.
- With `stateDir.persistence.enabled` the chart runs a StatefulSet with one
  PersistentVolumeClaim per pod, `state-<statefulset>-<n>`. Two servers must never
  share a `state_dir`, which is why it is not a single shared volume.

Keep `replicaCount: 1` unless request load needs more. Every replica polls
every source and holds its own WIS2 subscriptions, so two replicas make
twice the upstream requests, and object stores answer that with 503 SlowDown
or 429. Each replica also fills its own caches, so the same data is fetched
and held once per replica.

On SIGTERM the server stops polling, finishes in-flight requests and writes
its state snapshots. `terminationGracePeriodSeconds`, 60 by default, bounds
that.

## Health and probes

`/health` answers 200 while any collection is ready or degraded, and 503 only
when every collection has failed.

- `startupProbe`: `/health`, up to 10 minutes. The server binds its port
  before it loads the collections but answers only after, and a large remote
  catalog can take minutes.
- `readinessProbe`: `/health`. A pod whose every source is down leaves the
  Service.
- `livenessProbe`: a TCP check. Restarting the pod would not bring upstream
  sources back, so `/health` is not a liveness signal.

Each probe value is a complete Kubernetes probe; set one to `null` to remove
it.

## Exposing the server

`ingress` creates an Ingress, and `httpRoute` a Gateway API HTTPRoute with
`parentRefs` to your Gateway. Behind either, set `[server] base_url`, or
`trust_proxy_headers = true` when the proxy sends `Forwarded` or
`X-Forwarded-Proto` and `X-Forwarded-Host`,
so that links in responses point at the public address.

Every path is routed, `/metrics` and `/admin/*` included. The admin endpoints
answer 403 without `ADMIN_TOKEN` and need its bearer token with it.
`/metrics` has no authentication: block it at the proxy if it should not be
public.

## Monitoring

- `serviceMonitor.enabled` creates a Prometheus Operator ServiceMonitor for
  `/metrics` on the Service port. Add the labels your Prometheus selects on
  in `serviceMonitor.labels`.
- `prometheusRule.enabled` creates a PrometheusRule with the repository's
  alert rules, `docker/prometheus/rules/meteocore.yml`: a stale, empty or
  failed collection, 5xx and WMS error-tile rates, shed requests, upstream
  throttling and memory. The repository README's Alerting section lists
  them and their thresholds. Their `job="meteocore"` selectors name the
  ServiceMonitor's job, the Service's name, or `prometheusRule.job` when
  something else scrapes the pods. Add the labels your Prometheus selects
  rules on in `prometheusRule.labels`. To change a threshold, leave this off
  and deploy an edited copy of the file. Routing the alerts is up to your
  Alertmanager.
- `grafanaDashboard.enabled` puts the repository's dashboard,
  `docker/grafana/dashboards/meteocore-overview.json`, in a ConfigMap
  labelled `grafana_dashboard: "1"` for the Grafana sidecar. It is a
  `dashboard.grafana.app/v2` resource and needs Grafana 13 or later. Its
  Prometheus and Loki datasources are variables.

## Memory

The server's memory is mostly caches and admission budgets, each sized by an
`MC_*` variable or a config key. `GET /admin/cache-plan` lists them all with
their sizes. The server does not size them from the container's memory
limit, so set the limit above the cache plan plus headroom for requests, or
lower the sizes. `MALLOC_ARENA_MAX=2` is set by default; it limits only C
libraries that allocate with glibc malloc, since the server itself uses
jemalloc.

## Configuration changes

The pod template carries a hash of the ConfigMaps the chart renders, so
`helm upgrade` with a changed `config.toml`, collection file, palette or
sample setting rolls the pods. ConfigMaps listed in `existingConfigMap` and
`existingConfigMaps` are not hashed: after changing one, run `kubectl rollout
restart`, or put a hash of it in `podAnnotations`.

`watch_collections_dir` stays off. Kubelet updates a ConfigMap volume by
swapping a `..data` symlink, and the server's watcher has not been tested
against that.

## Values

| Value | Default | Description |
|---|---|---|
| `image.repository` | `ghcr.io/mrauhala/meteocore` | Image repository |
| `image.tag` | `""` | Image tag; the chart's appVersion when empty |
| `image.digest` | `""` | Image digest, `sha256:…`; wins over the tag |
| `image.pullPolicy` | `IfNotPresent` | |
| `imagePullSecrets` | `[]` | |
| `nameOverride`, `fullnameOverride` | `""` | Resource names |
| `replicaCount` | `1` | See [State and replicas](#state-and-replicas) |
| `config.toml` | see above | The server's `config.toml` |
| `config.existingConfigMap` | `""` | A ConfigMap to take `config.toml` from instead |
| `config.existingConfigMapKey` | `config.toml` | Its key |
| `collections.files` | `{}` | File name → collection TOML, in `/data/collections.d` |
| `collections.existingConfigMaps` | `[]` | ConfigMaps mounted there too |
| `colormaps.files` | `{}` | File name → palette, in `/data/colormaps.d` |
| `colormaps.existingConfigMaps` | `[]` | ConfigMaps mounted there too |
| `sample.enabled` | `true` | The sample collection |
| `sample.id` | `sample-observations` | Its collection id |
| `env` | `LOG_FORMAT=json`, `RUST_LOG=info`, `MALLOC_ARENA_MAX=2` | Plain variables, name → value |
| `extraEnv` | `[]` | EnvVar objects, for Secrets |
| `envFrom` | `[]` | |
| `extraArgs` | `[]` | Server arguments after the chart's own |
| `containerPort` | `8000` | The port the server listens on |
| `stateDir.persistence.enabled` | `false` | A StatefulSet with a PersistentVolumeClaim per pod |
| `stateDir.persistence.storageClassName` | `""` | The cluster's default when empty |
| `stateDir.persistence.accessModes` | `[ReadWriteOnce]` | |
| `stateDir.persistence.size` | `1Gi` | A global WIS2 BUFR store snapshot is about 8 MB |
| `stateDir.persistence.annotations` | `{}` | |
| `serviceAccount.create` | `true` | |
| `serviceAccount.name` | `""` | The full name when empty |
| `serviceAccount.annotations` | `{}` | |
| `serviceAccount.automount` | `false` | Mount an API token |
| `podAnnotations`, `podLabels` | `{}` | |
| `podSecurityContext` | uid/gid/fsGroup 999, non-root, `RuntimeDefault` | |
| `securityContext` | read-only root, no privilege escalation, no capabilities | |
| `terminationGracePeriodSeconds` | `60` | Time for in-flight requests and the state flush |
| `service.type` | `ClusterIP` | |
| `service.port` | `8000` | |
| `service.annotations` | `{}` | |
| `startupProbe`, `readinessProbe`, `livenessProbe` | see [Health and probes](#health-and-probes) | |
| `resources` | `{}` | See [Memory](#memory) |
| `ingress.enabled` | `false` | |
| `ingress.className` | `""` | |
| `ingress.annotations` | `{}` | |
| `ingress.hosts` | `meteocore.example.com`, `/` | Hosts and paths |
| `ingress.tls` | `[]` | |
| `httpRoute.enabled` | `false` | A Gateway API HTTPRoute |
| `httpRoute.annotations` | `{}` | |
| `httpRoute.parentRefs` | `[]` | Gateways to attach to |
| `httpRoute.hostnames` | `[]` | |
| `httpRoute.rules` | `/`, prefix | `matches` and `filters` per rule; the backend is the Service |
| `serviceMonitor.enabled` | `false` | |
| `serviceMonitor.labels` | `{}` | |
| `serviceMonitor.interval` | `30s` | |
| `serviceMonitor.scrapeTimeout` | `""` | |
| `serviceMonitor.relabelings`, `serviceMonitor.metricRelabelings` | `[]` | |
| `prometheusRule.enabled` | `false` | The alert rules as a PrometheusRule |
| `prometheusRule.labels` | `{}` | |
| `prometheusRule.job` | `""` | The scrape job the rules match; the Service's name, the ServiceMonitor's job, when empty |
| `grafanaDashboard.enabled` | `false` | |
| `grafanaDashboard.namespace` | `""` | The release namespace when empty |
| `grafanaDashboard.labels` | `grafana_dashboard: "1"` | |
| `grafanaDashboard.annotations` | `{}` | |
| `podDisruptionBudget.enabled` | `false` | |
| `podDisruptionBudget.minAvailable` | `null` | Wins over `maxUnavailable` when set |
| `podDisruptionBudget.maxUnavailable` | `1` | |
| `extraVolumes`, `extraVolumeMounts` | `[]` | Data volumes |
| `priorityClassName` | `""` | |
| `nodeSelector`, `tolerations`, `affinity`, `topologySpreadConstraints` | empty | |

`values.schema.json` rejects unknown keys, so a misspelt value fails the
install instead of being ignored.
