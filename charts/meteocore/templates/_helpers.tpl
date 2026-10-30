{{/* Chart name, overridable. */}}
{{- define "meteocore.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Fully qualified app name. The release name alone when it already contains
the chart name.
*/}}
{{- define "meteocore.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "meteocore.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "meteocore.selectorLabels" -}}
app.kubernetes.io/name: {{ include "meteocore.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "meteocore.labels" -}}
helm.sh/chart: {{ include "meteocore.chart" . }}
{{ include "meteocore.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "meteocore.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "meteocore.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* The image reference: a digest wins over the tag, the tag defaults to appVersion. */}}
{{- define "meteocore.image" -}}
{{- if .Values.image.digest }}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest }}
{{- else }}
{{- printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion | toString) }}
{{- end }}
{{- end }}

{{/* The StatefulSet with per-pod state volumes, or the Deployment. */}}
{{- define "meteocore.statefulSet" -}}
{{- if .Values.stateDir.persistence.enabled }}true{{ end }}
{{- end }}

{{/* The sample collection's file in collections.d. */}}
{{- define "meteocore.sampleFile" -}}
{{- printf "%s.toml" .Values.sample.id }}
{{- end }}

{{/*
Config checks that would otherwise fail only at server start, or never:
directories the config does not point at.
*/}}
{{- define "meteocore.validate" -}}
{{- if and .Values.sample.enabled (hasKey .Values.collections.files (include "meteocore.sampleFile" .)) }}
{{- fail (printf "collections.files has %s, the sample collection's file: rename it, or set sample.enabled=false" (include "meteocore.sampleFile" .)) }}
{{- end }}
{{- if not .Values.config.existingConfigMap }}
{{- $toml := .Values.config.toml | default "" }}
{{- $collections := or .Values.collections.files .Values.collections.existingConfigMaps .Values.sample.enabled }}
{{- if and $collections (not (regexMatch "(?m)^\\s*collections_dir\\s*=\\s*\"(/data/)?collections\\.d/?\"" $toml)) }}
{{- fail "config.toml must set [server] collections_dir = \"collections.d\": collections.files, collections.existingConfigMaps and the sample collection are mounted in /data/collections.d" }}
{{- end }}
{{- if and (or .Values.colormaps.files .Values.colormaps.existingConfigMaps) (not (regexMatch "(?m)^\\s*colormaps_dir\\s*=\\s*\"(/data/)?colormaps\\.d/?\"" $toml)) }}
{{- fail "config.toml must set [server] colormaps_dir = \"colormaps.d\": colormaps.files and colormaps.existingConfigMaps are mounted in /data/colormaps.d" }}
{{- end }}
{{- end }}
{{- end }}
