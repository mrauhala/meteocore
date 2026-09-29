//! Map renders: output pixels map to source pixels through a coarse
//! [`ProjectionGrid`] (Critical Rule 5), then each band samples its scan.
//!
//! A multi-band request (an RGB composite's channels) renders every band
//! from one scan time and builds the coordinate map once per distinct
//! source grid it samples: ABI bands of one resolution share a grid, so a
//! three-band composite projects once, not three times.

use chrono::{DateTime, SecondsFormat, Utc};
use ds_core::error::DataServerError;
use ds_core::geo::GeoTransform;
use ds_core::map_engine::{OutputCrs, RasterTile};
use ds_core::resample::ProjectionGrid;

use crate::frame::{Frame, OVERVIEW_FACTOR};
use crate::{PixelReader, SatelliteEngine};

/// The output→source coordinate maps of one request, one per distinct
/// source grid (full resolution or overview) a band samples.
pub(crate) struct CoordinateMaps<'a> {
    bbox: [f64; 4],
    width: u32,
    height: u32,
    output_crs: &'a OutputCrs,
    /// Each grid with its column period (a global grid's) and its map.
    built: Vec<(GeoTransform, Option<f64>, ProjectionGrid)>,
}

#[cfg(test)]
thread_local! {
    /// Coordinate maps built on this thread: renders run on the calling
    /// thread, so a test counts its own.
    pub(crate) static MAPS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl<'a> CoordinateMaps<'a> {
    pub(crate) fn new(bbox: [f64; 4], width: u32, height: u32, output_crs: &'a OutputCrs) -> Self {
        CoordinateMaps {
            bbox,
            width,
            height,
            output_crs,
            built: Vec::new(),
        }
    }

    /// The map onto `gt`'s pixels, built on first use. Points the satellite
    /// cannot see project to NaN; the grid refines cells on the limb, and no
    /// footprint guard is needed: there is no far side for a coarse cell to
    /// alias onto.
    ///
    /// A global grid (GMGSI) passes its column `period`: the map then
    /// interpolates across the grid's seam, and a projected output's own
    /// longitude cut, and `Frame::pixel` wraps the columns it samples.
    fn onto(&mut self, gt: &GeoTransform, period: Option<f64>) -> &ProjectionGrid {
        let index = match self
            .built
            .iter()
            .position(|(g, p, _)| same_grid(g, gt) && *p == period)
        {
            Some(index) => index,
            None => {
                #[cfg(test)]
                MAPS_BUILT.with(|n| n.set(n.get() + 1));
                let (bbox, output_crs) = (self.bbox, self.output_crs);
                let grid = ProjectionGrid::build_2d_periodic(
                    self.width,
                    self.height,
                    gt.width,
                    gt.height,
                    period,
                    |fx, fy| output_crs.project_node(bbox, fx, fy),
                    |lon, lat| gt.world_to_pixel_f64(lon, lat),
                );
                self.built.push((gt.clone(), period, grid));
                self.built.len() - 1
            }
        };
        &self.built[index].2
    }
}

/// Whether two grids place every pixel at the same coordinates, so one
/// coordinate map serves both.
fn same_grid(a: &GeoTransform, b: &GeoTransform) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.origin_x == b.origin_x
        && a.origin_y == b.origin_y
        && a.pixel_width == b.pixel_width
        && a.pixel_height == b.pixel_height
        && a.crs == b.crs
}

/// A tile of nodata.
fn empty_tile(width: u32, height: u32) -> RasterTile {
    RasterTile {
        width,
        height,
        values: vec![None; width as usize * height as usize].into(),
    }
}

impl SatelliteEngine {
    /// `get_raster_tile`: one product at the scan [`Self::select`] picks.
    pub(crate) fn render(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
    ) -> Result<RasterTile, DataServerError> {
        let index = self.product_index(parameter)?;
        let catalog = self.catalog.load();
        let Some(time) = Self::select(&catalog, index, time) else {
            return Ok(empty_tile(width, height));
        };
        let frame = self.frame(index, time, &catalog.frames[index][&time])?;
        let mut maps = CoordinateMaps::new(bbox, width, height, output_crs);
        self.render_scan(index, time, &frame, &mut maps)
    }

    /// `get_raster_tiles`: every band from the one scan `time` names, or
    /// for `None` the latest scan they all have ([`Self::select_common`]).
    /// A band without a scan at `time` fails the request: a composite never
    /// mixes scans. With no shared scan for `None`, every tile is empty,
    /// as a single band renders before its first scan.
    pub(crate) fn render_bands(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameters: &[&str],
    ) -> Result<Vec<RasterTile>, DataServerError> {
        let indices = self.product_indices(parameters)?;
        let catalog = self.catalog.load();
        let Some(time) = time.or_else(|| Self::select_common(&catalog, &indices, None)) else {
            return Ok(indices.iter().map(|_| empty_tile(width, height)).collect());
        };
        let scans = indices
            .iter()
            .map(|&index| {
                catalog.frames[index].get(&time).ok_or_else(|| {
                    DataServerError::InvalidParameter(format!(
                        "parameter '{}' has no scan at {} in '{}'",
                        self.products[index].parameter,
                        time.to_rfc3339_opts(SecondsFormat::Secs, true),
                        self.collection_id
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut maps = CoordinateMaps::new(bbox, width, height, output_crs);
        indices
            .iter()
            .zip(scans)
            .map(|(&index, scan)| {
                ds_core::deadline::check()?;
                let frame = self.frame(index, time, scan)?;
                self.render_scan(index, time, &frame, &mut maps)
            })
            .collect()
    }

    /// Sample one product's scan onto the request's output grid.
    fn render_scan(
        &self,
        index: usize,
        time: DateTime<Utc>,
        frame: &Frame,
        maps: &mut CoordinateMaps,
    ) -> Result<RasterTile, DataServerError> {
        let (width, height) = (maps.width, maps.height);
        // The part of the grid the request sees decides the level: sample
        // the overview when a full-resolution read would take several
        // source pixels per output pixel. On a global grid a request across
        // its seam sees two windows.
        let windows = frame.windows(maps.bbox);
        if windows.is_empty() {
            return Ok(empty_tile(width, height));
        }
        let cols: u32 = windows.iter().map(|[c0, _, c1, _]| c1 - c0).sum();
        let rows = windows
            .iter()
            .map(|[_, r0, _, r1]| r1 - r0)
            .max()
            .unwrap_or(0);
        let density = f64::max(cols as f64 / width as f64, rows as f64 / height as f64);
        let overview = density >= OVERVIEW_FACTOR as f64;
        let gt = if overview {
            &frame.overview.gt
        } else {
            &frame.gt
        };
        let grid = maps.onto(gt, frame.period(overview));

        let mut reader = PixelReader::new(self.frame_key(index, time), frame);
        let mut values = Vec::with_capacity(width as usize * height as usize);
        for oy in 0..height {
            for ox in 0..width {
                let (c, r) = grid.sample(ox, oy);
                let Some((col, row)) = frame.pixel(overview, c, r) else {
                    values.push(None);
                    continue;
                };
                let raw = if overview {
                    frame.overview.raw[row as usize * gt.width as usize + col as usize]
                } else {
                    reader.raw(row, col)?
                };
                values.push(frame.packing.decode(raw));
            }
        }
        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
    use ds_core::map_engine::{MapEngine, OutputCrs};

    use super::MAPS_BUILT;
    use crate::SatelliteEngine;

    const C13: &str =
        "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";
    const ACHT: &str = "OR_ABI-L2-ACHTF-M6_G19_s20262681900199_e20262681909507_c20262681912337.nc";

    /// Coordinate maps a render built on this thread.
    fn maps_built(render: impl FnOnce()) -> usize {
        let before = MAPS_BUILT.with(|n| n.get());
        render();
        MAPS_BUILT.with(|n| n.get()) - before
    }

    /// Bands on one grid share one coordinate map; a band on another grid
    /// (the cloud top crop lies elsewhere on the disk) adds one.
    #[test]
    fn a_coordinate_map_is_built_once_per_source_grid() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/goes19-abi");
        for name in [C13, ACHT] {
            std::fs::copy(fixtures.join(name), dir.path().join(name)).unwrap();
        }
        let product =
            |parameter: &str, product: &str, band, variable: &str| SatelliteProductConfig {
                parameter: parameter.into(),
                title: parameter.into(),
                unit: "K".into(),
                product: product.into(),
                band,
                variable: variable.into(),
            };
        let config = SatelliteConfig {
            provider: "goes-r".into(),
            data_path: Some(dir.path().to_string_lossy().into_owned()),
            endpoint: None,
            bucket: None,
            time_window: None,
            poll_interval_secs: 60,
            // Two parameters read the same C13 scan: two bands of one grid.
            products: vec![
                product("ir_a", "ABI-L2-CMIPF", Some(13), "CMI"),
                product("ir_b", "ABI-L2-CMIPF", Some(13), "CMI"),
                product("cloud", "ABI-L2-ACHTF", None, "TEMP"),
            ],
        };
        let engine = SatelliteEngine::new("goes19-render-maps", &config).unwrap();
        engine.poll_once();
        let extent = engine.raster_info().spatial_extent.unwrap();
        for (size, overview) in [(256, false), (48, true)] {
            let render = |parameters: &[&str]| {
                let tiles = engine
                    .get_raster_tiles(
                        extent,
                        size,
                        size,
                        None,
                        &OutputCrs::Wgs84,
                        parameters,
                        None,
                        None,
                    )
                    .unwrap();
                assert!(tiles.iter().all(|t| !t.is_empty()), "overview {overview}");
                tiles
            };
            assert_eq!(maps_built(|| drop(render(&["ir_a", "ir_b", "ir_a"]))), 1);
            assert_eq!(maps_built(|| drop(render(&["ir_a", "cloud", "ir_b"]))), 2);
            let single = || {
                engine
                    .get_raster_tile(
                        extent,
                        size,
                        size,
                        None,
                        &OutputCrs::Wgs84,
                        Some("ir_b"),
                        None,
                        None,
                    )
                    .unwrap();
            };
            assert_eq!(maps_built(single), 1);
            // The shared map samples each band as its own would.
            let tiles = render(&["ir_a", "ir_b"]);
            assert!(tiles[0]
                .values
                .iter_values()
                .eq(tiles[1].values.iter_values()));
        }
    }
}
