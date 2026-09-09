//! Imported layers: a georeferenced raster resampled onto the chart's own grid.
//!
//! The sonar mosaic already paints into Web Mercator at a chosen zoom and cuts
//! 256 px tiles out of it; an imported grid does the same thing from a
//! different source, so it gets the same shape. What is different is that the
//! numbers are kept rather than the picture: a multibeam DTM is stored as
//! values, and the colour ramp is applied when a tile is cut. Changing the ramp
//! or the stretch then costs nothing, and the depth under the cursor is still a
//! depth.
//!
//! Values are quantised to 16 bits across the grid's own range, which for a
//! survey DTM spanning ten metres is a tenth of a millimetre -- far below what
//! a multibeam knows -- and `u16::MAX` is reserved to mean "no data".

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::crs;
use crate::geo;
use crate::index::Bounds;
use crate::tiff::GeoTiff;

pub const MAGIC: &[u8; 8] = b"SWTLYR01";

/// The spelling this file used before the application was renamed.
///
/// Only ever read. Everything under `out/` is derived and could in principle
/// be rebuilt, but rebuilding it means re-reading every recording, so a rename
/// is not a good enough reason to invalidate a workspace. Written files carry
/// `SWTLYR01`; both are accepted on the way in.
pub const MAGIC_LEGACY: &[u8; 8] = b"WPALYR01";
pub const TILE: usize = 256;
/// Reserved value meaning "nothing here".
pub const NODATA: u16 = u16::MAX;

/// Cells in the resampled raster before the import is refused.
///
/// At two bytes a cell this is a 512 MB file, which is already more than any
/// survey deliverable needs; beyond it the answer is to import a coarser grid,
/// not to spend a minute of the operator's time producing something unusable.
const MAX_CELLS: usize = 268_435_456;

// ---- colour ----------------------------------------------------------------

/// The ramps offered for a single-band layer.
///
/// `Depth` and `Terrain` are the two that matter for bathymetry and are shaped
/// for it: deep water dark blue through to shallow, and a land-style hypsometric
/// tint for anything above the waterline. The rest are the usual perceptual
/// ramps, which is what you want when the question is "where does this change",
/// not "how deep is it".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Ramp {
    Grey,
    #[default]
    Depth,
    Terrain,
    Viridis,
    Magma,
    Turbo,
    Ice,
    Amber,
}

impl Ramp {
    pub const ALL: &'static [Ramp] = &[
        Ramp::Grey, Ramp::Depth, Ramp::Terrain, Ramp::Viridis,
        Ramp::Magma, Ramp::Turbo, Ramp::Ice, Ramp::Amber,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Ramp::Grey => "grey",
            Ramp::Depth => "depth",
            Ramp::Terrain => "terrain",
            Ramp::Viridis => "viridis",
            Ramp::Magma => "magma",
            Ramp::Turbo => "turbo",
            Ramp::Ice => "ice",
            Ramp::Amber => "amber",
        }
    }

    fn stops(&self) -> &'static [(f32, [u8; 3])] {
        match self {
            Ramp::Grey => &[(0.0, [16, 16, 18]), (1.0, [244, 246, 248])],
            Ramp::Depth => &[
                (0.00, [8, 20, 60]), (0.20, [12, 56, 120]), (0.40, [22, 104, 168]),
                (0.60, [64, 158, 190]), (0.80, [140, 205, 205]), (1.00, [222, 240, 226]),
            ],
            Ramp::Terrain => &[
                (0.00, [22, 60, 100]), (0.35, [70, 140, 170]), (0.50, [214, 214, 170]),
                (0.65, [120, 160, 90]), (0.85, [150, 120, 80]), (1.00, [250, 250, 250]),
            ],
            Ramp::Viridis => &[
                (0.00, [68, 1, 84]), (0.25, [59, 82, 139]), (0.50, [33, 145, 140]),
                (0.75, [94, 201, 98]), (1.00, [253, 231, 37]),
            ],
            Ramp::Magma => &[
                (0.00, [0, 0, 4]), (0.25, [81, 18, 124]), (0.50, [183, 55, 121]),
                (0.75, [252, 137, 97]), (1.00, [252, 253, 191]),
            ],
            Ramp::Turbo => &[
                (0.00, [48, 18, 59]), (0.20, [40, 130, 220]), (0.40, [50, 200, 160]),
                (0.60, [180, 222, 44]), (0.80, [250, 140, 40]), (1.00, [122, 4, 3]),
            ],
            Ramp::Ice => &[
                (0.00, [8, 12, 30]), (0.50, [58, 118, 158]), (1.00, [232, 246, 252]),
            ],
            Ramp::Amber => &[
                (0.00, [12, 8, 4]), (0.50, [168, 96, 26]), (1.00, [255, 236, 190]),
            ],
        }
    }

    /// Look up a colour at `t` in 0..1.
    pub fn at(&self, t: f32) -> [u8; 3] {
        let s = self.stops();
        let t = t.clamp(0.0, 1.0);
        if t <= s[0].0 {
            return s[0].1;
        }
        for w in s.windows(2) {
            if t <= w[1].0 {
                let f = ((t - w[0].0) / (w[1].0 - w[0].0).max(1e-9)).clamp(0.0, 1.0);
                let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f).round() as u8;
                return [
                    mix(w[0].1[0], w[1].1[0]),
                    mix(w[0].1[1], w[1].1[1]),
                    mix(w[0].1[2], w[1].1[2]),
                ];
            }
        }
        s[s.len() - 1].1
    }

    pub fn parse(s: &str) -> Option<Ramp> {
        Ramp::ALL.iter().copied().find(|r| r.name() == s)
    }
}

/// How a layer is drawn. Everything here is cheap to change: none of it is
/// baked into the resampled raster.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayerStyle {
    pub ramp: Ramp,
    #[serde(default)]
    pub reverse: bool,
    /// Value at the bottom of the ramp. `None` uses the grid's own 2nd
    /// percentile, which is what makes an unfamiliar file look sensible on the
    /// first draw.
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// Relief shading, 0 to 1. A bathymetric grid is mostly unreadable without
    /// some: colour alone hides anything smaller than the depth range.
    #[serde(default = "half")]
    pub shade: f64,
    #[serde(default = "az")]
    pub sun_azimuth_deg: f64,
    #[serde(default = "el")]
    pub sun_elevation_deg: f64,
    /// Vertical exaggeration for the shading only. Seabed relief is centimetres
    /// over metres of ground; lit truthfully it would be invisible.
    #[serde(default = "exag")]
    pub shade_exaggeration: f64,
}

fn half() -> f64 { 0.55 }
fn az() -> f64 { 315.0 }
fn el() -> f64 { 40.0 }
fn exag() -> f64 { 6.0 }

impl Default for LayerStyle {
    fn default() -> LayerStyle {
        LayerStyle {
            ramp: Ramp::default(),
            reverse: false,
            min: None,
            max: None,
            shade: half(),
            sun_azimuth_deg: az(),
            sun_elevation_deg: el(),
            shade_exaggeration: exag(),
        }
    }
}

// ---- the resampled raster --------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaneKind {
    /// One measured value per cell, quantised to 16 bits.
    Value,
    /// Straight colour, four bytes per cell.
    Rgba,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RasterHeader {
    pub version: u32,
    pub kind: PlaneKind,
    /// Origin in Web Mercator pixels at `base_zoom`.
    pub x0: i64,
    pub y0: i64,
    pub width: usize,
    pub height: usize,
    pub base_zoom: u32,
    pub bounds: Bounds,
    /// Range the 16-bit values span, in the source's units.
    pub vmin: f64,
    pub vmax: f64,
    /// A robust range for the default stretch, so one bad cell does not flatten
    /// the picture.
    pub p2: f64,
    pub p98: f64,
    pub source: String,
    pub source_size: [usize; 2],
    pub source_epsg: Option<u32>,
    pub units: String,
    /// Cells that got a value, for the "is this layer empty" question.
    pub filled: usize,
}

pub struct LayerRaster {
    pub header: RasterHeader,
    map: Mmap,
    off: usize,
}

impl LayerRaster {
    /// Raw 16-bit code at a cell.
    #[inline]
    fn code(&self, i: usize) -> u16 {
        let at = self.off + i * 2;
        match self.map.get(at..at + 2) {
            Some(s) => u16::from_le_bytes([s[0], s[1]]),
            None => NODATA,
        }
    }

    #[inline]
    fn rgba(&self, i: usize) -> [u8; 4] {
        let at = self.off + i * 4;
        match self.map.get(at..at + 4) {
            Some(s) => [s[0], s[1], s[2], s[3]],
            None => [0, 0, 0, 0],
        }
    }

    /// The real value at a cell, or `None` where nothing was resampled.
    #[inline]
    pub fn value(&self, i: usize) -> Option<f64> {
        let c = self.code(i);
        if c == NODATA {
            return None;
        }
        let h = &self.header;
        Some(h.vmin + c as f64 * (h.vmax - h.vmin) / (NODATA as f64 - 1.0))
    }

    /// Value under a position, for the readout.
    pub fn sample(&self, lat: f64, lon: f64) -> Option<f64> {
        if self.header.kind != PlaneKind::Value {
            return None;
        }
        let h = &self.header;
        let (x, y) = geo::lonlat_to_px(lon, lat, h.base_zoom as f64);
        let gx = x.floor() as i64 - h.x0;
        let gy = y.floor() as i64 - h.y0;
        if gx < 0 || gy < 0 || gx as usize >= h.width || gy as usize >= h.height {
            return None;
        }
        self.value(gy as usize * h.width + gx as usize)
    }

    pub fn load(path: impl AsRef<Path>) -> io::Result<LayerRaster> {
        let mut f = File::open(path.as_ref())?;
        let mut magic = [0u8; 8];
        f.read_exact(&mut magic)?;
        if &magic != MAGIC && &magic != MAGIC_LEGACY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a swath layer raster"));
        }
        let mut n = [0u8; 4];
        f.read_exact(&mut n)?;
        let hlen = u32::from_le_bytes(n) as usize;
        let mut hdr = vec![0u8; hlen];
        f.read_exact(&mut hdr)?;
        let header: RasterHeader = serde_json::from_slice(&hdr)?;
        // Safety: the file is ours and read-only from here on. Mapping rather
        // than reading matters: a full-resolution DTM is hundreds of megabytes
        // and only the tiles being looked at are ever touched.
        let map = unsafe { Mmap::map(&f)? };
        Ok(LayerRaster { header, map, off: 12 + hlen })
    }

    /// Cut one 256 px tile, coloured through `style`.
    pub fn tile(&self, z: u32, tx: i64, ty: i64, style: &LayerStyle) -> Option<Vec<u8>> {
        match self.header.kind {
            PlaneKind::Rgba => self.tile_rgba(z, tx, ty),
            PlaneKind::Value => self.tile_value(z, tx, ty, style),
        }
    }

    #[inline]
    fn cell(&self, gx: i64, gy: i64) -> Option<usize> {
        let h = &self.header;
        if gx < 0 || gy < 0 || gx as usize >= h.width || gy as usize >= h.height {
            return None;
        }
        Some(gy as usize * h.width + gx as usize)
    }

    /// How a tile pixel maps onto base cells: the top-left cell, then how many
    /// to walk and how far apart.
    ///
    /// Zoomed out, one tile pixel stands for many cells, and taking whichever
    /// one happens to fall under it turns a smooth grid into a moire -- the
    /// vertical striping that shows up on a raster whose cells are about two
    /// output pixels wide. So the window is averaged, and capped at 4x4 so a
    /// very low zoom does not walk a million cells for one pixel.
    ///
    /// `px` and `py` are signed because the relief shading asks for a one-pixel
    /// margin outside the tile.
    #[inline]
    fn window(&self, z: u32, tx: i64, ty: i64, px: i64, py: i64) -> (i64, i64, i64, i64) {
        let h = &self.header;
        if z <= h.base_zoom {
            let step = 1i64 << (h.base_zoom - z);
            let n = step.min(4);
            (
                (tx * TILE as i64 + px) * step - h.x0,
                (ty * TILE as i64 + py) * step - h.y0,
                n,
                (step / n).max(1),
            )
        } else {
            let mag = 1i64 << (z - h.base_zoom);
            (
                (tx * TILE as i64 + px).div_euclid(mag) - h.x0,
                (ty * TILE as i64 + py).div_euclid(mag) - h.y0,
                1,
                1,
            )
        }
    }

    /// The mean of the cells under one tile pixel, or None where none of them
    /// carry data.
    #[inline]
    fn mean_at(&self, z: u32, tx: i64, ty: i64, px: i64, py: i64) -> Option<f64> {
        let (gx, gy, n, sub) = self.window(z, tx, ty, px, py);
        let (mut acc, mut count) = (0.0f64, 0u32);
        for dy in 0..n {
            for dx in 0..n {
                let Some(i) = self.cell(gx + dx * sub, gy + dy * sub) else { continue };
                if let Some(v) = self.value(i) {
                    acc += v;
                    count += 1;
                }
            }
        }
        (count > 0).then(|| acc / count as f64)
    }

    fn tile_rgba(&self, z: u32, tx: i64, ty: i64) -> Option<Vec<u8>> {
        let mut out = vec![0u8; TILE * TILE * 4];
        let mut any = false;
        for py in 0..TILE {
            for px in 0..TILE {
                let (gx, gy, n, sub) = self.window(z, tx, ty, px as i64, py as i64);
                let (mut acc, mut count) = ([0u32; 4], 0u32);
                for dy in 0..n {
                    for dx in 0..n {
                        let Some(i) = self.cell(gx + dx * sub, gy + dy * sub) else { continue };
                        let c = self.rgba(i);
                        if c[3] == 0 {
                            continue;
                        }
                        for k in 0..4 {
                            acc[k] += c[k] as u32;
                        }
                        count += 1;
                    }
                }
                if count == 0 {
                    continue;
                }
                let o = (py * TILE + px) * 4;
                for k in 0..4 {
                    out[o + k] = (acc[k] / count) as u8;
                }
                any = true;
            }
        }
        any.then_some(out)
    }

    fn tile_value(&self, z: u32, tx: i64, ty: i64, style: &LayerStyle) -> Option<Vec<u8>> {
        let h = &self.header;
        let lo = style.min.unwrap_or(h.p2);
        let hi = style.max.unwrap_or(h.p98);
        let span = if (hi - lo).abs() < 1e-12 { 1.0 } else { hi - lo };

        // One row of margin either side, so the relief shading has neighbours
        // at the tile edges. Without it every tile boundary gets a seam where
        // the slope is computed from a missing neighbour.
        let w = TILE + 2;
        let mut vals = vec![f64::NAN; w * w];
        let mut any = false;
        for py in 0..w {
            for px in 0..w {
                let Some(v) = self.mean_at(z, tx, ty, px as i64 - 1, py as i64 - 1) else {
                    continue;
                };
                vals[py * w + px] = v;
                if px >= 1 && px <= TILE && py >= 1 && py <= TILE {
                    any = true;
                }
            }
        }
        if !any {
            return None;
        }

        // Ground distance one tile pixel covers, for the slope.
        let mpp = geo::mercator_scale(
            geo::px_to_lonlat(0.0, (h.y0 + h.height as i64 / 2) as f64, h.base_zoom as f64).1,
            z as f64,
        )
        .max(1e-6);
        let sun_az = (90.0 - style.sun_azimuth_deg).to_radians();
        let sun_el = style.sun_elevation_deg.to_radians();
        let (sx, sy) = (sun_az.cos() * sun_el.cos(), sun_az.sin() * sun_el.cos());
        let sz = sun_el.sin();
        let exag = style.shade_exaggeration.max(0.0);

        let mut out = vec![0u8; TILE * TILE * 4];
        for py in 0..TILE {
            for px in 0..TILE {
                let c = (py + 1) * w + (px + 1);
                let v = vals[c];
                if !v.is_finite() {
                    continue;
                }
                let mut t = ((v - lo) / span) as f32;
                if style.reverse {
                    t = 1.0 - t;
                }
                let mut rgb = style.ramp.at(t);

                if style.shade > 0.0 {
                    let l = vals[c - 1];
                    let r = vals[c + 1];
                    let u = vals[c - w];
                    let d = vals[c + w];
                    let dzdx = if l.is_finite() && r.is_finite() {
                        (r - l) / (2.0 * mpp)
                    } else {
                        0.0
                    };
                    // north is up, so a positive dz going down the image is a
                    // negative dz going north
                    let dzdy = if u.is_finite() && d.is_finite() {
                        (u - d) / (2.0 * mpp)
                    } else {
                        0.0
                    };
                    let (nx, ny, nz) = (-dzdx * exag, -dzdy * exag, 1.0);
                    let len = (nx * nx + ny * ny + nz * nz).sqrt();
                    let lambert = ((nx * sx + ny * sy + nz * sz) / len).clamp(0.0, 1.0);
                    // Blend towards the lit value rather than multiplying, so
                    // full shading still shows the colour it is shading.
                    let f = (1.0 - style.shade + style.shade * lambert * 1.6).clamp(0.0, 1.6);
                    rgb = [
                        (rgb[0] as f64 * f).clamp(0.0, 255.0) as u8,
                        (rgb[1] as f64 * f).clamp(0.0, 255.0) as u8,
                        (rgb[2] as f64 * f).clamp(0.0, 255.0) as u8,
                    ];
                }
                let o = (py * TILE + px) * 4;
                out[o] = rgb[0];
                out[o + 1] = rgb[1];
                out[o + 2] = rgb[2];
                out[o + 3] = 255;
            }
        }
        Some(out)
    }
}

// ---- import ----------------------------------------------------------------

/// Progress callback: (rows done, rows total).
pub type Progress<'a> = &'a mut dyn FnMut(usize, usize);

/// Resample a GeoTIFF onto the Web Mercator grid and write it to `out`.
///
/// The base zoom is chosen so one output cell is no coarser than the finest
/// axis of the source, then lowered if that would make an unreasonable raster.
/// Each source pixel is painted over the rectangle it actually covers, which is
/// what stops a grid that is finer in longitude than in latitude coming out
/// striped.
pub fn import_tiff(
    src: &Path,
    out: &Path,
    zoom_override: Option<u32>,
    progress: Option<Progress>,
) -> io::Result<RasterHeader> {
    let t = GeoTiff::open(src)?;
    let epsg = t.epsg.unwrap_or(4326);
    let sys = crs::get(epsg).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "EPSG:{epsg} is not in the registry; reproject the file to WGS84 \
                 or a UTM zone first"
            ),
        )
    })?;

    // Corners, to size the raster and to pick a zoom.
    let to_ll = |c: f64, r: f64| {
        let (x, y) = t.pixel_to_model(c, r);
        sys.to_wgs84(x, y)
    };
    let mut b = Bounds::EMPTY;
    for (c, r) in [
        (0.0, 0.0),
        (t.width as f64 - 1.0, 0.0),
        (0.0, t.height as f64 - 1.0),
        (t.width as f64 - 1.0, t.height as f64 - 1.0),
    ] {
        let (lat, lon) = to_ll(c, r);
        if !lat.is_finite() || !lon.is_finite() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the georeferencing does not resolve to a position on the earth",
            ));
        }
        b.extend(lat, lon);
    }

    // Ground size of one source pixel, measured rather than assumed: it is the
    // projection that decides, not the transform's numbers.
    let (lat0, lon0) = to_ll(0.0, 0.0);
    let (lat1, lon1) = to_ll(1.0, 0.0);
    let (lat2, lon2) = to_ll(0.0, 1.0);
    let px_x = geo::distance_m(lat0, lon0, lat1, lon1);
    let px_y = geo::distance_m(lat0, lon0, lat2, lon2);
    let finest = px_x.min(px_y).max(1e-6);
    let mid_lat = (b.min_lat + b.max_lat) / 2.0;

    let mut zoom = zoom_override.unwrap_or_else(|| {
        let s = 156_543.033_928_040_97 * mid_lat.to_radians().cos();
        ((s / finest).log2().ceil() as i64).clamp(1, 22) as u32
    });

    // Drop a zoom at a time until the raster is a size worth writing. Better
    // to give the operator a slightly softer layer than to refuse the import
    // of a file they clearly meant to look at.
    let (x0, y0, width, height) = loop {
        let z = zoom as f64;
        let (x0f, y1f) = geo::lonlat_to_px(b.min_lon, b.min_lat, z);
        let (x1f, y0f) = geo::lonlat_to_px(b.max_lon, b.max_lat, z);
        let x0 = x0f.floor() as i64;
        let y0 = y0f.floor() as i64;
        let width = (x1f.ceil() as i64 - x0 + 1) as usize;
        let height = (y1f.ceil() as i64 - y0 + 1) as usize;
        if width.saturating_mul(height) <= MAX_CELLS || zoom <= 1 {
            break (x0, y0, width, height);
        }
        zoom -= 1;
    };
    if width == 0 || height == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the image has no extent"));
    }

    let cells = width * height;
    let kind = if t.is_single_band() { PlaneKind::Value } else { PlaneKind::Rgba };

    let mut sum = vec![0.0f32; if kind == PlaneKind::Value { cells } else { 0 }];
    let mut cnt = vec![0u16; if kind == PlaneKind::Value { cells } else { 0 }];
    let mut rgba = vec![0u8; if kind == PlaneKind::Rgba { cells * 4 } else { 0 }];

    let z = zoom as f64;
    let mut row_px: Vec<(f64, f64)> = vec![(0.0, 0.0); t.width];
    let mut next_px: Vec<(f64, f64)> = vec![(0.0, 0.0); t.width];
    let mut have_next = false;

    let project_row = |row: usize, dst: &mut Vec<(f64, f64)>| {
        for c in 0..t.width {
            let (x, y) = t.pixel_to_model(c as f64, row as f64);
            let (lat, lon) = sys.to_wgs84(x, y);
            dst[c] = geo::lonlat_to_px(lon, lat, z);
        }
    };

    let mut prog = progress;
    for row in 0..t.height {
        if have_next {
            std::mem::swap(&mut row_px, &mut next_px);
        } else {
            project_row(row, &mut row_px);
        }
        if row + 1 < t.height {
            project_row(row + 1, &mut next_px);
            have_next = true;
        } else {
            have_next = false;
        }

        for c in 0..t.width {
            let (px, py) = row_px[c];
            // Footprint of this source pixel in output cells, from its
            // neighbours rather than from the transform: after projection the
            // spacing is not constant across the image.
            let wx = if c + 1 < t.width { (row_px[c + 1].0 - px).abs() } else { 1.0 };
            let hy = if row + 1 < t.height { (next_px[c].1 - py).abs() } else { 1.0 };
            let nx = (wx.ceil() as i64).clamp(1, 64);
            let ny = (hy.ceil() as i64).clamp(1, 64);

            match kind {
                PlaneKind::Value => {
                    let Some(v) = t.value(c, row, 0) else { continue };
                    if !v.is_finite() {
                        continue;
                    }
                    if let Some(nd) = t.nodata {
                        if (v - nd).abs() <= nd.abs() * 1e-9 {
                            continue;
                        }
                    }
                    let vf = v as f32;
                    for dy in 0..ny {
                        for dx in 0..nx {
                            let gx = px.floor() as i64 + dx - x0;
                            let gy = py.floor() as i64 + dy - y0;
                            if gx < 0 || gy < 0 || gx as usize >= width || gy as usize >= height {
                                continue;
                            }
                            let i = gy as usize * width + gx as usize;
                            sum[i] += vf;
                            cnt[i] = cnt[i].saturating_add(1);
                        }
                    }
                }
                PlaneKind::Rgba => {
                    let Some(c3) = t.rgb(c, row) else { continue };
                    let a = t.alpha(c, row);
                    if a == 0 {
                        continue;
                    }
                    for dy in 0..ny {
                        for dx in 0..nx {
                            let gx = px.floor() as i64 + dx - x0;
                            let gy = py.floor() as i64 + dy - y0;
                            if gx < 0 || gy < 0 || gx as usize >= width || gy as usize >= height {
                                continue;
                            }
                            let i = (gy as usize * width + gx as usize) * 4;
                            rgba[i] = c3[0];
                            rgba[i + 1] = c3[1];
                            rgba[i + 2] = c3[2];
                            rgba[i + 3] = a;
                        }
                    }
                }
            }
        }
        if let Some(p) = prog.as_deref_mut() {
            if row % 256 == 0 {
                p(row, t.height);
            }
        }
    }

    // ---- quantise and write ----
    std::fs::create_dir_all(out.parent().unwrap_or(Path::new(".")))?;
    let mut header = RasterHeader {
        version: 1,
        kind,
        x0,
        y0,
        width,
        height,
        base_zoom: zoom,
        bounds: b,
        vmin: 0.0,
        vmax: 1.0,
        p2: 0.0,
        p98: 1.0,
        source: src.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        source_size: [t.width, t.height],
        source_epsg: Some(epsg),
        units: if t.is_single_band() { "value".into() } else { "colour".into() },
        filled: 0,
    };

    let mut plane: Vec<u8>;
    match kind {
        PlaneKind::Rgba => {
            header.filled = rgba.chunks_exact(4).filter(|c| c[3] > 0).count();
            plane = rgba;
        }
        PlaneKind::Value => {
            let mut vals: Vec<f32> = Vec::new();
            let mut lo = f64::MAX;
            let mut hi = f64::MIN;
            for i in 0..cells {
                if cnt[i] > 0 {
                    let v = (sum[i] / cnt[i] as f32) as f64;
                    lo = lo.min(v);
                    hi = hi.max(v);
                    vals.push(v as f32);
                }
            }
            if vals.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "nothing landed on the grid: check the file's coordinate system",
                ));
            }
            header.filled = vals.len();
            header.vmin = lo;
            header.vmax = if hi > lo { hi } else { lo + 1.0 };
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let pick = |p: f64| -> f64 {
                let i = ((vals.len() - 1) as f64 * p / 100.0).round() as usize;
                vals[i.min(vals.len() - 1)] as f64
            };
            header.p2 = pick(2.0);
            header.p98 = pick(98.0);

            let span = header.vmax - header.vmin;
            plane = vec![0u8; cells * 2];
            for i in 0..cells {
                let code = if cnt[i] == 0 {
                    NODATA
                } else {
                    let v = (sum[i] / cnt[i] as f32) as f64;
                    let q = ((v - header.vmin) / span * (NODATA as f64 - 1.0)).round();
                    q.clamp(0.0, NODATA as f64 - 1.0) as u16
                };
                plane[i * 2..i * 2 + 2].copy_from_slice(&code.to_le_bytes());
            }
        }
    }

    let hdr = serde_json::to_vec(&header)?;
    let mut f = BufWriter::new(File::create(out)?);
    f.write_all(MAGIC)?;
    f.write_all(&(hdr.len() as u32).to_le_bytes())?;
    f.write_all(&hdr)?;
    f.write_all(&plane)?;
    f.flush()?;
    if let Some(p) = prog.as_deref_mut() {
        p(t.height, t.height);
    }
    Ok(header)
}
