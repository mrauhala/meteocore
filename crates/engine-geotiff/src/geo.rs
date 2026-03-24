/// Affine transform mapping pixel coordinates to world coordinates (WGS84).
///
/// For a GeoTIFF with ModelTiepointTag and ModelPixelScaleTag:
///   world_x = origin_x + (col - tiepoint_col) * pixel_width
///   world_y = origin_y - (row - tiepoint_row) * pixel_height
#[derive(Debug, Clone)]
pub struct GeoTransform {
    pub origin_x: f64,
    pub origin_y: f64,
    pub pixel_width: f64,
    pub pixel_height: f64,
    pub width: u32,
    pub height: u32,
}

impl GeoTransform {
    /// Convert world coordinate (lon, lat) to pixel coordinate (col, row).
    /// Returns None if the coordinate is outside the raster bounds.
    pub fn world_to_pixel(&self, lon: f64, lat: f64) -> Option<(u32, u32)> {
        let col = ((lon - self.origin_x) / self.pixel_width) as i64;
        let row = ((self.origin_y - lat) / self.pixel_height) as i64;

        if col >= 0 && col < self.width as i64 && row >= 0 && row < self.height as i64 {
            Some((col as u32, row as u32))
        } else {
            None
        }
    }

    /// Compute the bounding box in world coordinates [west, south, east, north].
    pub fn bbox(&self) -> [f64; 4] {
        let west = self.origin_x;
        let east = self.origin_x + self.width as f64 * self.pixel_width;
        let north = self.origin_y;
        let south = self.origin_y - self.height as f64 * self.pixel_height;
        [west, south, east, north]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_transform() -> GeoTransform {
        GeoTransform {
            origin_x: 0.419,
            origin_y: 74.810,
            pixel_width: 0.01,
            pixel_height: 0.01,
            width: 3249,
            height: 1750,
        }
    }

    #[test]
    fn world_to_pixel_inside() {
        let gt = sample_transform();
        let (col, row) = gt.world_to_pixel(10.0, 65.0).unwrap();
        assert_eq!(col, ((10.0 - 0.419) / 0.01) as u32);
        assert_eq!(row, ((74.810 - 65.0) / 0.01) as u32);
    }

    #[test]
    fn world_to_pixel_outside() {
        let gt = sample_transform();
        assert!(gt.world_to_pixel(-10.0, 65.0).is_none());
        assert!(gt.world_to_pixel(10.0, 80.0).is_none());
    }

    #[test]
    fn bbox_correct() {
        let gt = sample_transform();
        let bbox = gt.bbox();
        assert!((bbox[0] - 0.419).abs() < 1e-6);
        assert!((bbox[2] - (0.419 + 3249.0 * 0.01)).abs() < 1e-6);
        assert!((bbox[3] - 74.810).abs() < 1e-6);
    }
}
