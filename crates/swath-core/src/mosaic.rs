//! Georeferenced sonar mosaic, painted onto the Web Mercator tile grid.
//!
//! Every ping is projected onto a raster aligned to the tile pyramid, so the
//! result drops straight onto the chart: zoom in and the actual seabed imagery
//! is in its real place rather than a track line standing in for it.
//!
//! The placement is only ever as good as the navigation. With a plain constant
//! layback and no reference grid to register against, expect 5-10 m. That is
//! not a reason to draw it badly -- it is a reason for the viewer to show the
//! uncertainty honestly, which is why `Mosaic` carries the nav config it was
//! built with.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::geo;
use crate::index::{Bounds, PingIndex};

use crate::nav::{Nav, NavConfig};
use crate::signal;
use crate::waterfall::Axis;

pub const MAGIC: &[u8; 8] = b"SWTMOS01";

/// The spelling this file used before the application was renamed.
///
/// Only ever read. Everything under `out/` is derived and could in principle
/// be rebuilt, but rebuilding it means re-reading every recording, so a rename
/// is not a good enough reason to invalidate a workspace. Written files carry
/// `SWTMOS01`; both are accepted on the way in.
pub const MAGIC_LEGACY: &[u8; 8] = b"WPAMOS01";
pub const TILE: usize = 256;

/// Which painter drew a mosaic, part of its cache key.
///
/// A derived product named after its settings is only as honest as the code
/// that reads them. Dropping the `water_column` setting put the settings JSON
/// back byte for byte to what it was before that setting existed, and a raster
/// painted before the nadir floor answered to the new name: same settings,
/// 5 282 872 cells against the current painter's 6 022 935, and nothing to say
/// so. Bump this on any change to what `build` puts on the ground.
pub const PAINTER: u32 = 8;

/// Lowest priority any sample is painted at, which is what keeps the nadir
/// band on the chart.
///
/// Leaving it out is the conventional choice -- the flat-seabed assumption is
/// at its worst directly under the fish and the return there is specular -- and
/// it was briefly a setting here. It should not have been. Whether the band is
/// drawn is a question about *imagery*; where a sample lands is a question
/// about *geometry*, and the two are independent in this file: `build_stroke`
/// walks ground range outward and reads the trace at `sqrt(g^2 + alt^2)`, so
/// the across-track position of a sample is decided before its priority is
/// even looked up. Suppressing the band therefore removed cells and moved
/// none -- measured on `070926_measures_b2`, every cell painted without it was
/// also painted with it at the same raster address, and the two rasters
/// cross-correlate at r = 0.998 with the peak at zero shift.
///
/// So the only thing the setting could buy was a hole two altitudes wide --
/// about seven metres here -- down the middle of every pass, in the one place
/// where the sonar has no second look to fall back on. Coverage one metre off
/// the track line goes from 48% to 100% by keeping it. The waterfall is where
/// that band can be looked at or ignored, and the Axis control already does
/// that: slant range shows the water column, ground range does not.
const NADIR_FLOOR: f64 = 0.04;

/// Priority as a function of apparent grazing angle, measured from vertical.
///
/// Modelled on MB-System's mbmosaic, whose manual notes that "the nadir region
/// of the sidescan swath is generally of little use because it is dominated by
/// specular reflection". Overlapping passes are not simply averaged: each cell
/// leans towards the look with the better geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PriorityTable {
    #[default]
    Outer,
    Nadir,
    Flat,
}

impl PriorityTable {
    fn table(&self) -> &'static [(f64, f64)] {
        match self {
            PriorityTable::Outer => &[
                (0.0, 0.0), (12.0, 0.0), (22.0, 0.30), (35.0, 0.80), (50.0, 1.0),
                (65.0, 1.0), (75.0, 0.85), (82.0, 0.55), (90.0, 0.30),
            ],
            PriorityTable::Nadir => &[
                (0.0, 1.0), (20.0, 1.0), (40.0, 0.70), (60.0, 0.35), (80.0, 0.10), (90.0, 0.05),
            ],
            PriorityTable::Flat => &[(0.0, 0.0), (12.0, 0.0), (20.0, 1.0), (90.0, 1.0)],
        }
    }
    pub fn priority(&self, theta_deg: f64) -> f64 {
        let t = self.table();
        if theta_deg <= t[0].0 {
            return t[0].1;
        }
        for w in t.windows(2) {
            if theta_deg <= w[1].0 {
                let f = (theta_deg - w[0].0) / (w[1].0 - w[0].0).max(1e-9);
                return w[0].1 + f * (w[1].1 - w[0].1);
            }
        }
        t[t.len() - 1].1
    }
}

// Container-level default, so a caller that cares about one setting can send
// just that one and get the established values for the rest. Every field here
// is in the fingerprint the mosaic file is named by, so a partial request that
// silently zeroed the others would build a different mosaic under a name that
// claims otherwise.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MosaicConfig {
    pub subsystem: u8,
    /// Web Mercator zoom the raster is built at. 19 is ~0.18 m/px at 52 N.
    pub base_zoom: u32,
    /// Samples resampled across the swath, per side.
    pub across: usize,
    pub table: PriorityTable,
    /// How sharply priority selects between overlapping looks. High values
    /// approach "best look wins"; low values average.
    pub exponent: f64,
    /// Across-track axis the swath is laid out on, matching the waterfall's
    /// own Axis control.
    ///
    /// `Ground` is the slant-range correction: a sample is placed at
    /// `sqrt(slant^2 - alt^2)` from the track, which is where the seabed it
    /// came off actually is, and the water column vanishes because ground range
    /// zero *is* the first bottom return. This is the honest choice and the
    /// default.
    ///
    /// `Slant` places a sample at the range the sonar measured, uncorrected, so
    /// the water column is kept -- a band two altitudes wide down the middle of
    /// every pass -- and everything outside it sits `slant - ground` too far
    /// out, by 6.5 m at nadir+1 m and under 0.2 m past 20 m on a fish flying at
    /// 6 m. That displacement is the whole difference between the two, and it
    /// is why this cannot be the default: on `Slant` the mosaic is a picture of
    /// what the sonar measured rather than a map of where the seabed is, and a
    /// position read off it is wrong by that much.
    ///
    /// It is offered because the waterfall offers it, and an operator comparing
    /// the two views wants them drawn the same way. `nadir_blank_m` set to the
    /// flying height cuts the water column back out while leaving the rest on
    /// the slant axis.
    pub axis: Axis,
    /// Leave this many metres either side of nadir unpainted.
    ///
    /// Not the same decision as `NADIR_FLOOR`, and worth keeping apart. The
    /// floor is unconditional because painting the band adds coverage where
    /// there is none and moves nothing. This is the operator saying they would
    /// rather have a hole than that imagery -- which is a fair thing to want,
    /// because the sonar is looking at the seabed with the edge of its beam
    /// there and the return is specular.
    ///
    /// The 4125's array is depressed 33 degrees with a 50 degree vertical beam,
    /// so the main lobe starts about `0.62 * altitude` out. That is the value
    /// to reach for: it blanks exactly what the beam does not properly light.
    pub nadir_blank_m: f64,
    /// Clamp the trace to this much slant range. None uses all of it.
    ///
    /// The high-frequency channel is often set to a range it cannot reach --
    /// 176 224 pings of ss21 in `wpa20260906` are recorded out to 50 m against
    /// a 35 m specification -- and the tail of those traces is noise. Painting
    /// it is bad enough; painting it with a time-varied gain that lifts the far
    /// swath by thirty-odd decibels turns it into bright noise.
    pub max_range_m: Option<f64>,
    /// Speed of sound in the water, m/s. Measured, ideally.
    ///
    /// Every across-track distance is a travel time multiplied by this, so it
    /// scales the whole swath: the default 1500 against a measured 1524 draws
    /// everything 1.6% closer to the track line than it is, 0.75 m at the edge
    /// of a 47 m swath. It is *not* the number that reads Discover's own
    /// metre-valued fields back -- see `crate::C_RECORDED`.
    pub sound_speed_m_s: f64,
    /// Salinity, for the absorption model only. Nothing here measures it and
    /// the answer is insensitive to it.
    pub salinity_psu: f64,
    /// Centre frequency of this channel, Hz, for the absorption model. Zero
    /// means "ask the index", which is right for the low channel and wrong for
    /// the high one, whose recorded sweep field has wrapped -- so the API fills
    /// this in from the recovered value. See `PingIndex::recover_band_centres`.
    pub centre_freq_hz: f64,
    /// Along-track gain equalisation, 0 to 1.
    ///
    /// The time-varied gain corrects across the swath; nothing corrected along
    /// it, and the mosaic was the only view without it -- the waterfall has had
    /// `agc` since it was written, which is why one looked even and the other
    /// striped. Measured on `070926_measures_star`, the swath mean swings from
    /// 96 to 146 grey over tens of pings, and about a quarter of that variance
    /// is common to port and starboard (r = 0.25 at a nine-ping scale), which
    /// is the part that reads as a band crossing the whole swath.
    ///
    /// It is a trade, and the reason this is not 1.0: the shared part is a
    /// property of the ping, but the rest is the seabed, and normalising every
    /// ping to the same level flattens a genuine hard-to-soft transition along
    /// with the artefact.
    pub agc: f32,
    /// Strength of the time-varied gain, 0 to 1. See `signal::tvg_gain`.
    ///
    /// This was declared, defaulted and digested for months without being
    /// read, so every mosaic before `PAINTER` 3 was painted with no across-track
    /// correction at all: brightness peaked at 127 grey around 15 m and fell to
    /// 28 by 45 m, a bright band down the middle of every pass that is a
    /// property of the sonar and not of the seabed.
    pub tvg: f32,
    /// Empirical angle-varying gain, 0 to 1. See `signal::AngleGain`.
    ///
    /// The correction the mosaic was missing. `tvg` undoes spreading and
    /// absorption, which is what physics predicts; this undoes what is left --
    /// the beam pattern and the seabed's angular response -- by measuring it
    /// over the recording rather than modelling it. Measured after `tvg` at
    /// full strength, the residual across-track swing is 5x on
    /// `080929_demimines` and 2x on `070926_measures_star`, which is a bright
    /// core and dark edges on every pass and a patchwork wherever two passes
    /// cross.
    ///
    /// The Python mosaicker this was ported from did this and the port dropped
    /// it, which is why `docs/mosaicking.md` describes a correction that was
    /// not in the Rust. On by default because it is a correction rather than a
    /// preference: the waterfall has always had its equivalent, and leaving it
    /// out is what made the two views disagree about what the seabed looks
    /// like.
    pub angular_gain: f32,
    /// Adaptive speckle suppression, 0 to 1. See `signal::despeckle`.
    ///
    /// Measured on `070926_measures_star` over a fully covered 69 m square,
    /// 37% of the variance in the painted raster sits at the cell scale --
    /// which at 0.18 m is finer than the sonar's own along-track resolution at
    /// range, so it is speckle rather than seabed. Nothing else in this file
    /// suppresses it: `splat` deposits each sample into one cell, and the runs
    /// `bridge` fills between samples are interpolations of the same two
    /// numbers, so a cell's four to six `hits` are nothing like four to six
    /// independent looks.
    ///
    /// Zero by default because it changes every picture already painted, and
    /// because how hard to smooth a survey is the operator's call and not a
    /// constant: the filter leaves targets alone by construction, but it is
    /// still the difference between reading texture and reading returns.
    pub despeckle: f32,
    /// Take the logarithm before the contrast stretch.
    ///
    /// Backscatter spans orders of magnitude and the clip points are
    /// percentiles, so on a linear scale the specular nadir return sets the
    /// white point for the whole survey. Off by default only because it changes
    /// every existing picture.
    pub db: bool,
    pub gamma: f32,
    pub clip_lo: f64,
    pub clip_hi: f64,
    /// Skip pings the roll flag marked. Off by default: the mechanism linking
    /// roll to position error is not established, so dropping a fifth of the
    /// data on it would be a guess dressed as a filter.
    pub drop_flagged: bool,
    /// EXPERIMENT: paint only pings whose course is within this many degrees of
    /// `look_centre_deg`. None paints every ping.
    pub look_window_deg: Option<f64>,
    pub look_centre_deg: f64,
    /// EXPERIMENT: skip pings turning faster than this, deg/s. None keeps all.
    pub max_turn_rate: Option<f64>,
    pub nav: NavConfig,
}

impl Default for MosaicConfig {
    fn default() -> MosaicConfig {
        MosaicConfig {
            subsystem: 20,
            base_zoom: 19,
            across: 1024,
            table: PriorityTable::Outer,
            exponent: 8.0,
            axis: Axis::Ground,
            nadir_blank_m: 0.0,
            max_range_m: None,
            sound_speed_m_s: crate::C_RECORDED,
            salinity_psu: 35.0,
            centre_freq_hz: 0.0,
            agc: 0.5,
            tvg: 0.7,
            angular_gain: 1.0,
            despeckle: 0.0,
            db: false,
            gamma: 0.8,
            clip_lo: 1.0,
            clip_hi: 99.0,
            drop_flagged: false,
            look_window_deg: None,
            look_centre_deg: 0.0,
            max_turn_rate: None,
            nav: NavConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MosaicHeader {
    pub version: u32,
    /// Raster origin in Web Mercator pixels at `base_zoom`.
    pub x0: i64,
    pub y0: i64,
    pub width: usize,
    pub height: usize,
    pub base_zoom: u32,
    pub bounds: Bounds,
    pub pings: usize,
    pub config: MosaicConfig,
}

impl MosaicHeader {
    /// Whether a mosaic already painted is the one these settings ask for.
    /// Both halves matter: the settings, and the painter that read them.
    pub fn matches(&self, cfg: &MosaicConfig) -> bool {
        self.version == PAINTER && crate::fingerprint(&self.config) == crate::fingerprint(cfg)
    }
}

/// The cache key for a set of settings: the file name, and the `v=` on every
/// tile URL the viewer asks for.
pub fn key(cfg: &MosaicConfig) -> String {
    crate::fingerprint(&(PAINTER, cfg))
}

pub struct Mosaic {
    pub header: MosaicHeader,
    /// Row-major 8-bit backscatter.
    pub value: Vec<u8>,
    /// Row-major coverage: 0 where nothing was painted.
    pub cover: Vec<u8>,
}

/// Accumulators used while painting, before the stretch is applied.
struct Grid {
    sum: Vec<f32>,
    wgt: Vec<f32>,
    hits: Vec<u16>,
}

impl Mosaic {
    pub fn build(index: &PingIndex, cfg: &MosaicConfig) -> io::Result<Mosaic> {
        let sel: Vec<usize> = index
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.subsystem == cfg.subsystem)
            .map(|(i, _)| i)
            .collect();
        if sel.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no pings for subsystem {}", cfg.subsystem),
            ));
        }
        let recs: Vec<_> = sel.iter().map(|&i| index.records[i]).collect();
        let nav = Nav::build(&recs, cfg.nav);

        // Raster extent: survey bounds plus one swath of margin, so the outer
        // edge of the widest ping still lands inside the grid.
        let mut b = Bounds::EMPTY;
        for r in &recs {
            if r.lat.is_finite() && r.lon.is_finite() && (r.lat != 0.0 || r.lon != 0.0) {
                b.extend(r.lat, r.lon);
            }
        }
        if b.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "no positions"));
        }
        let max_range = recs
            .iter()
            .map(|r| {
                let full = r.slant_range_m(cfg.sound_speed_m_s);
                cfg.max_range_m.map_or(full, |m| full.min(m))
            })
            .fold(0.0, f64::max);
        let margin = (max_range + cfg.nav.layback_m + cfg.nav.gps_to_towpoint_m) * 1.3;
        let padded = b.pad_m(margin);

        let z = cfg.base_zoom as f64;
        let (x0f, y1f) = geo::lonlat_to_px(padded.min_lon, padded.min_lat, z);
        let (x1f, y0f) = geo::lonlat_to_px(padded.max_lon, padded.max_lat, z);
        let x0 = x0f.floor() as i64;
        let y0 = y0f.floor() as i64;
        let width = (x1f.ceil() as i64 - x0 + 1) as usize;
        let height = (y1f.ceil() as i64 - y0 + 1) as usize;
        if width * height > 400_000_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("raster would be {width}x{height}; lower base_zoom"),
            ));
        }

        let mut grid = Grid {
            sum: vec![0.0; width * height],
            wgt: vec![0.0; width * height],
            hits: vec![0; width * height],
        };

        // Absorption, for the time-varied gain. It needs a frequency and a
        // temperature; the frequency comes from the channel and the
        // temperature from the sound speed, which was measured and is a good
        // enough thermometer for a coefficient this insensitive to it.
        let f_hz = if cfg.centre_freq_hz > 0.0 {
            cfg.centre_freq_hz
        } else {
            index
                .bands()
                .iter()
                .find(|b| b.subsystem == cfg.subsystem)
                .map_or(0.0, |b| b.centre_hz as f64)
        };
        let depth = signal::median_f64(
            &recs.iter().map(|r| r.depth as f64).filter(|d| *d > 0.0).collect::<Vec<_>>(),
        );
        let depth = if depth.is_finite() { depth } else { 10.0 };
        let temp = signal::temperature_from_sound_speed(cfg.sound_speed_m_s, cfg.salinity_psu, depth);
        let alpha_db_per_m = if f_hz > 0.0 && cfg.tvg > 0.0 {
            signal::absorption_db_per_km(f_hz / 1000.0, temp, cfg.salinity_psu, depth) / 1000.0
        } else {
            0.0
        };

        // Pair the two sides so one pass paints a whole ping.
        let pairs = crate::waterfall::pair_channels(index, cfg.subsystem);

        // Decode in parallel chunks, paint serially into the shared grid.
        // Painting is scattered writes into one raster; a lock per cell would
        // cost more than the decode it protects, so the decode -- which is the
        // expensive half -- is what gets the threads.
        // The bottom prior, smoothed along track before a single trace is
        // decoded -- it is in the index. The waterfall does exactly this, with
        // the same constants, because a mosaic and a waterfall that disagree
        // about where the seabed is put a contact in two places.
        let priors: Vec<f32> = pairs
            .iter()
            .map(|row| {
                row.own()
                    .and_then(|i| index.records[i as usize].altitude_samples())
                    .unwrap_or(0.0)
            })
            .collect();
        let priors = signal::steady_prior(&priors, crate::waterfall::BOTTOM_MEDIAN);

        // What the swath still looks like after the physics has been undone,
        // measured over the whole recording so that dividing it out re-shades
        // the ping without following the seabed under any one of them.
        let angle = measure_angle_gain(index, cfg, &pairs, &priors, alpha_db_per_m);

        const CHUNK: usize = 512;
        let mut painted = 0usize;
        let mut prev_line = Line::default();
        // Levels carried across the chunk boundary, so a ping at the seam is
        // equalised against the same neighbourhood as the one before it.
        let mut tails: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
        for (ci, chunk) in pairs.chunks(CHUNK).enumerate() {
            let base = ci * CHUNK;
            let mut strokes: Vec<Option<Stroke>> = chunk
                .par_iter()
                .enumerate()
                .map(|(k, row)| {
                    build_stroke(
                        index, &nav, cfg, alpha_db_per_m, &angle, priors[base + k],
                        row.port, row.stbd,
                    )
                })
                .collect();
            equalise(&mut strokes, &mut tails[0], cfg.agc, 0);
            equalise(&mut strokes, &mut tails[1], cfg.agc, 1);
            for st in strokes.into_iter().flatten() {
                if let Some(w) = cfg.look_window_deg {
                    let d = ((st.bearing - cfg.look_centre_deg + 180.0).rem_euclid(360.0)
                        - 180.0)
                        .abs();
                    if d > w {
                        prev_line = Line::default();
                        continue;
                    }
                }
                if let Some(m) = cfg.max_turn_rate {
                    if st.turn_rate.abs() > m {
                        prev_line = Line::default();
                        continue;
                    }
                }
                paint(&mut grid, &st, &mut prev_line, x0, y0, width, height, z);
                painted += 1;
            }
        }

        // The mean of everything painted into each cell, and `NaN` where
        // nothing was. Written back over the accumulator rather than into a
        // second plane: this raster runs to forty million cells on a survey
        // this size, and the weights have no reader past this point.
        let Grid { mut sum, wgt, hits } = grid;
        for i in 0..sum.len() {
            sum[i] = if wgt[i] > 0.0 { sum[i] / wgt[i] } else { f32::NAN };
        }
        drop(wgt);
        signal::despeckle(&mut sum, width, height, signal::DESPECKLE_RADIUS, cfg.despeckle);

        // Stretch on the painted cells only; an empty grid would otherwise drag
        // the low clip point to zero and wash the image out. The filter runs
        // before this, so the clip points are set by seabed rather than by
        // whichever speckle in the survey happened to be brightest.
        let scale = |v: f32| -> f32 {
            if cfg.db { 20.0 * v.max(1e-6).log10() } else { v }
        };
        let vals: Vec<f32> =
            sum.iter().filter(|v| v.is_finite()).map(|&v| scale(v)).collect();
        let st = signal::Stretch::from_data(&vals, cfg.clip_lo, cfg.clip_hi, cfg.gamma);

        let mut value = vec![0u8; width * height];
        let mut cover = vec![0u8; width * height];
        for i in 0..width * height {
            if sum[i].is_finite() {
                value[i] = st.apply(scale(sum[i]));
                cover[i] = hits[i].min(255) as u8;
            }
        }

        Ok(Mosaic {
            header: MosaicHeader {
                version: PAINTER,
                x0,
                y0,
                width,
                height,
                base_zoom: cfg.base_zoom,
                bounds: padded,
                pings: painted,
                config: cfg.clone(),
            },
            value,
            cover,
        })
    }

    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let mut f = BufWriter::new(File::create(path)?);
        let hdr = serde_json::to_vec(&self.header)?;
        f.write_all(MAGIC)?;
        f.write_all(&(hdr.len() as u32).to_le_bytes())?;
        f.write_all(&hdr)?;
        f.write_all(&self.value)?;
        f.write_all(&self.cover)?;
        f.flush()
    }

    pub fn load(path: impl AsRef<Path>) -> io::Result<Mosaic> {
        let mut f = File::open(path)?;
        let mut magic = [0u8; 8];
        f.read_exact(&mut magic)?;
        if &magic != MAGIC && &magic != MAGIC_LEGACY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a swath mosaic"));
        }
        let mut n = [0u8; 4];
        f.read_exact(&mut n)?;
        let mut hdr = vec![0u8; u32::from_le_bytes(n) as usize];
        f.read_exact(&mut hdr)?;
        let header: MosaicHeader = serde_json::from_slice(&hdr)?;
        let cells = header.width * header.height;
        let mut value = vec![0u8; cells];
        let mut cover = vec![0u8; cells];
        f.read_exact(&mut value)?;
        f.read_exact(&mut cover)?;
        Ok(Mosaic { header, value, cover })
    }

    /// Cut one 256 px tile, averaging down or replicating up from the base
    /// raster. Returns RGBA, transparent where nothing was painted.
    ///
    /// The value plane is 8-bit backscatter, already stretched when the mosaic
    /// was painted, so a colour scheme is a lookup on the way out and costs
    /// nothing to change. Grey is the default because it is the honest one:
    /// a ramp makes small differences in return strength easier to see and
    /// equally easy to over-read.
    pub fn tile_styled(&self, z: u32, tx: i64, ty: i64, style: &MosaicStyle) -> Option<Vec<u8>> {
        let mut rgba = self.tile(z, tx, ty)?;
        style.apply(&mut rgba);
        Some(rgba)
    }

    pub fn tile(&self, z: u32, tx: i64, ty: i64) -> Option<Vec<u8>> {
        let h = &self.header;
        let mut out = vec![0u8; TILE * TILE * 4];
        let mut any = false;

        if z <= h.base_zoom {
            let step = 1i64 << (h.base_zoom - z); // base pixels per tile pixel
            let bx = tx * TILE as i64 * step - h.x0;
            let by = ty * TILE as i64 * step - h.y0;
            for py in 0..TILE {
                for px in 0..TILE {
                    let sx = bx + px as i64 * step;
                    let sy = by + py as i64 * step;
                    let (mut acc, mut n) = (0u32, 0u32);
                    // averaging window; capped so a very low zoom does not walk
                    // a million source pixels per output pixel
                    let win = step.min(16);
                    let sub = (step / win).max(1);
                    let mut yy = 0;
                    while yy < win {
                        let mut xx = 0;
                        while xx < win {
                            let gx = sx + xx * sub;
                            let gy = sy + yy * sub;
                            if gx >= 0 && gy >= 0 && (gx as usize) < h.width && (gy as usize) < h.height {
                                let i = gy as usize * h.width + gx as usize;
                                if self.cover[i] > 0 {
                                    acc += self.value[i] as u32;
                                    n += 1;
                                }
                            }
                            xx += 1;
                        }
                        yy += 1;
                    }
                    if n > 0 {
                        let v = (acc / n) as u8;
                        let o = (py * TILE + px) * 4;
                        out[o] = v;
                        out[o + 1] = v;
                        out[o + 2] = v;
                        out[o + 3] = 255;
                        any = true;
                    }
                }
            }
        } else {
            let mag = 1i64 << (z - h.base_zoom); // tile pixels per base pixel
            for py in 0..TILE {
                for px in 0..TILE {
                    let gx = (tx * TILE as i64 + px as i64).div_euclid(mag) - h.x0;
                    let gy = (ty * TILE as i64 + py as i64).div_euclid(mag) - h.y0;
                    if gx >= 0 && gy >= 0 && (gx as usize) < h.width && (gy as usize) < h.height {
                        let i = gy as usize * h.width + gx as usize;
                        if self.cover[i] > 0 {
                            let v = self.value[i];
                            let o = (py * TILE + px) * 4;
                            out[o] = v;
                            out[o + 1] = v;
                            out[o + 2] = v;
                            out[o + 3] = 255;
                            any = true;
                        }
                    }
                }
            }
        }
        if any {
            Some(out)
        } else {
            None
        }
    }

    /// Sample the mosaic at a position, for a readout under the cursor.
    pub fn sample(&self, lat: f64, lon: f64) -> Option<u8> {
        let h = &self.header;
        let (x, y) = geo::lonlat_to_px(lon, lat, h.base_zoom as f64);
        let gx = x.floor() as i64 - h.x0;
        let gy = y.floor() as i64 - h.y0;
        if gx < 0 || gy < 0 || gx as usize >= h.width || gy as usize >= h.height {
            return None;
        }
        let i = gy as usize * h.width + gx as usize;
        if self.cover[i] == 0 {
            None
        } else {
            Some(self.value[i])
        }
    }
}

/// One ping resampled onto the across-track axis, with everything the painter
/// needs and nothing it does not.
struct Stroke {
    lat: f64,
    lon: f64,
    bearing: f64,
    /// Signed across-track distance in metres, ascending, port negative.
    across: Vec<f64>,
    amp: Vec<f32>,
    prio: Vec<f32>,
    /// This ping's own robust level, port then starboard, for the along-track
    /// equalisation. Measured outside the nadir band, so the specular return
    /// under the fish -- which is the brightest and least representative thing
    /// in the swath -- does not decide how the rest of the ping is scaled.
    ///
    /// One per side rather than one per ping, because the banding is not the
    /// ping getting brighter: it is one side brightening while the other dims,
    /// as the fish rolls and tips its fan down on one side and up on the other.
    /// Measured over 8000 pings the two sides correlate at only r = 0.50, the
    /// imbalance tracks roll at r = -0.75, and it costs 3.3% of brightness per
    /// degree. A single per-ping gain multiplies both sides by the same number
    /// and so cannot touch that by construction -- which is why bands used to
    /// cross part of the swath and stop.
    levels: [f32; 2],
    /// EXPERIMENT: rate of change of course at this ping, deg/s.
    turn_rate: f64,
}

fn build_stroke(
    index: &PingIndex,
    nav: &Nav,
    cfg: &MosaicConfig,
    alpha_db_per_m: f64,
    angle: &(signal::AngleGain, signal::AngleGain),
    prior_samples: f32,
    port: Option<u32>,
    stbd: Option<u32>,
) -> Option<Stroke> {
    let pr = port.map(|i| index.records[i as usize]);
    let sr = stbd.map(|i| index.records[i as usize]);
    let rec = pr.or(sr)?;
    let fix = nav.fix(&rec);
    if cfg.drop_flagged && !fix.clean {
        return None;
    }
    if !fix.lat.is_finite() || !fix.lon.is_finite() {
        return None;
    }

    let open = |r: &Option<crate::index::PingRecord>| -> Option<Vec<f32>> {
        let r = (*r)?;
        let jf = index.file_of(&r).ok()?;
        jf.ping_at(r.offset).map(|p| p.data)
    };
    let pd = open(&pr);
    let sd = open(&sr);
    if pd.is_none() && sd.is_none() {
        return None;
    }

    let res = rec.resolution_m(cfg.sound_speed_m_s).max(1e-6);
    // Same bottom refinement the waterfall uses: the recorded altitude as a
    // prior, the trace for where the return actually is. The two views have to
    // agree about this or a contact marked in one lands somewhere else in the
    // other.
    //
    // The prior is a *sample index*, and the recorded altitude is metres the
    // sonar computed at its own sound speed, so it converts back at that speed
    // and not at ours. The refinement then works in samples and only becomes
    // metres again at `res`, which is why changing the sound speed scales the
    // altitude correctly without touching the detector.
    let t0 = pd.as_ref().or(sd.as_ref())?;
    let prior = if prior_samples > 0.0 {
        prior_samples
    } else {
        signal::pick_bottom_trace(t0, 8, crate::waterfall::BOTTOM_FRAC, 9)
    };
    let alt_m = (signal::refine_bottom_within(
        t0, prior, crate::waterfall::BOTTOM_FRAC,
        crate::waterfall::BOTTOM_LO, crate::waterfall::BOTTOM_HI,
    ) as f64 * res).max(0.5);

    let n = pd.as_ref().or(sd.as_ref()).map(|v| v.len()).unwrap_or(0);
    if n < 16 {
        return None;
    }
    let mut half_m = n as f64 * res;
    if let Some(m) = cfg.max_range_m {
        half_m = half_m.min(m);
    }
    let per_side = cfg.across / 2;
    // Reference range for the gain: nadir, where the correction is 1 and the
    // level is what it always was. Anything further out is amplified relative
    // to it rather than the whole image being scaled.
    let r_ref = alt_m.max(1.0);

    let mut across = Vec::with_capacity(cfg.across);
    let mut amp = Vec::with_capacity(cfg.across);
    let mut prio = Vec::with_capacity(cfg.across);

    // Port first, most-negative outward, so `across` comes out ascending and
    // the painter can walk it as a polyline.
    for side in [-1.0f64, 1.0] {
        let trace = if side < 0.0 { pd.as_ref() } else { sd.as_ref() };
        for k in 0..per_side {
            let i = if side < 0.0 { per_side - 1 - k } else { k };
            // Outward along the image's own axis, which is a ground range in
            // `Ground` and the raw measured range in `Slant`.
            let d = half_m * (i as f64 + 0.5) / per_side as f64;
            if d < cfg.nadir_blank_m {
                continue;
            }
            let slant = match cfg.axis {
                // ground -> slant on a flat seabed one altitude below the fish
                Axis::Ground => (d * d + alt_m * alt_m).sqrt(),
                Axis::Slant => d,
            };
            if slant / res >= n as f64 - 1.0 {
                continue;
            }
            // The angle is a question about the seabed, not about the axis, so
            // it comes off the ground range either way. Inside the water column
            // `to_ground` saturates at nadir, which is the right answer: that
            // sample looks at no seabed at all, so it scores the floor and
            // fills only where nothing else reaches.
            let theta = cfg.axis.to_ground(d, alt_m).atan2(alt_m).to_degrees();
            // Spreading and absorption first, because they are a property of
            // the path; then the measured curve, which is what the beam and
            // the seabed do with what is left. Per side: the fish flies with a
            // list, so the two fans do not see the same angles.
            let shade = if side < 0.0 { angle.0.at(theta) } else { angle.1.at(theta) };
            let v = trace.map_or(0.0, |t| signal::sample_at(t, (slant / res) as f32))
                * signal::tvg_gain(slant, r_ref, alpha_db_per_m, cfg.tvg as f64)
                / shade.max(1e-6);
            // Nadir scores zero in every table, which blanks a strip two
            // altitudes wide down every pass. It gets a floor instead: raised
            // to the exponent below it is ~1e-11 against an outer look's 1, so
            // it fills only where nothing else reaches and never dilutes a
            // look that does. See the note on `NADIR_FLOOR`.
            let p = cfg.table.priority(theta).max(NADIR_FLOOR);
            across.push(side * d);
            amp.push(v);
            // `exponent` was declared, documented and never applied, so
            // overlapping looks were being averaged in proportion to priority
            // rather than selected between. Eight is close to best-look-wins.
            prio.push(shape_priority(p, cfg.exponent) as f32);
        }
    }
    if across.len() < 8 {
        return None;
    }

    let side_level = |want_port: bool| -> f32 {
        let outer: Vec<f32> = across
            .iter()
            .zip(amp.iter())
            .filter(|(a, _)| a.abs() > alt_m && (**a < 0.0) == want_port)
            .map(|(_, v)| *v)
            .collect();
        if outer.len() > 16 { signal::percentile(&outer, 60.0) } else { 0.0 }
    };
    let levels = [side_level(true), side_level(false)];

    let turn_rate = nav.turn_rate_at(fix.time);
    Some(Stroke {
        lat: fix.lat, lon: fix.lon, bearing: fix.bearing, across, amp, prio, levels, turn_rate,
    })
}

/// Pings this side of a ping are averaged to give it something to be level
/// with. About five metres of track at survey speed -- long against the nine to
/// seventeen pings the banding lives at, short against a real change of seabed.
///
/// `docs/banding.md` recommends lengthening this to 101 alongside the move to a
/// per-side gain, on the grounds that a per-side gain over a short window
/// erases genuine port-to-starboard differences in the seabed. Tried, and the
/// measurement does not support it here: on `070926_measures_b2` the median
/// imbalance goes 10.4 -> 10.2 points at 41 and 10.4 -> 10.3 at 101, so the
/// longer window erases more seabed for slightly less correction. Left at 41.
const AGC_WINDOW: usize = 41;

/// Bring every ping to the level of its neighbours.
///
/// Each ping is divided by (its own level / the local median level) raised to
/// `strength`, so at 0 nothing moves and at 1 every ping is pulled onto the
/// running median. The median rather than the mean because a ping over a wreck
/// should not drag its neighbours, and a ping that is simply wrong should not
/// be averaged into them.
///
/// `tail` carries the previous chunk's last levels in and this chunk's out, so
/// that a ping at a chunk boundary is equalised against a full window rather
/// than half of one -- otherwise the seams between chunks become their own
/// banding, at 512 pings instead of nine.
///
/// Run once per `side` -- 0 for port, 1 for starboard -- because half of the
/// banding is a see-saw between the two and a gain that scales the whole ping
/// cannot see it. Each side is levelled against its own neighbours and only
/// its own samples are scaled.
fn equalise(
    strokes: &mut [Option<Stroke>],
    tail: &mut Vec<f32>,
    strength: f32,
    side: usize,
) {
    if strength <= 0.0 {
        return;
    }
    let half = AGC_WINDOW / 2;
    let levels: Vec<f32> = strokes
        .iter()
        .map(|s| s.as_ref().map_or(0.0, |s| s.levels[side]))
        .collect();
    // The window for ping i spans [i-half, i+half], taken from the tail for the
    // part that falls before this chunk.
    let mut window: Vec<f32> = Vec::with_capacity(AGC_WINDOW);
    let mut buf: Vec<f32> = Vec::with_capacity(AGC_WINDOW);
    for (i, st) in strokes.iter_mut().enumerate() {
        let Some(st) = st.as_mut() else { continue };
        if !(st.levels[side] > 0.0) {
            continue;
        }
        window.clear();
        let want_before = half.min(i + tail.len());
        for k in 0..want_before {
            let at = i as isize - 1 - k as isize;
            let v = if at >= 0 {
                levels[at as usize]
            } else {
                let t = tail.len() as isize + at;
                if t < 0 { continue } else { tail[t as usize] }
            };
            if v > 0.0 {
                window.push(v);
            }
        }
        for v in levels.iter().skip(i).take(half + 1) {
            if *v > 0.0 {
                window.push(*v);
            }
        }
        if window.len() < 5 {
            continue;
        }
        buf.clear();
        buf.extend_from_slice(&window);
        buf.sort_by(f32::total_cmp);
        let reference = buf[buf.len() / 2];
        if !(reference > 0.0) {
            continue;
        }
        let g = (reference / st.levels[side]).powf(strength);
        // A ping that wants more than a factor of four is not a gain step, it
        // is a ping with nothing in it; leaving it alone is better than
        // amplifying its noise to match its neighbours.
        if !(0.25..=4.0).contains(&g) {
            continue;
        }
        // Port is the negative half of `across`, starboard the positive.
        let want_port = side == 0;
        for (a, v) in st.across.iter().zip(st.amp.iter_mut()) {
            if (*a < 0.0) == want_port {
                *v *= g;
            }
        }
    }
    *tail = levels;
    if tail.len() > AGC_WINDOW {
        tail.drain(..tail.len() - AGC_WINDOW);
    }
}

/// Paint one ping into the raster.
///
/// The swath is a line on the ground perpendicular to the fish's axis, so each
/// resampled sample is placed by walking that line out from the fish and the
/// gaps between consecutive samples are filled by stepping along it in
/// pixel-sized increments. Without the fill, a ping at 30 m range spreads its
/// outer samples further apart than a pixel and the mosaic comes out striped.
/// One ping's across-track line, already projected onto the raster.
///
/// Kept from ping to ping so the next one can fill the strip between them.
#[derive(Default)]
struct Line {
    /// Across-track metres, ascending: port most-negative first.
    across: Vec<f64>,
    px: Vec<(f64, f64)>,
    amp: Vec<f32>,
    prio: Vec<f32>,
}

impl Line {
    /// This line interpolated at an across-track distance, or None when that
    /// distance falls outside it or inside a gap in it.
    fn at(&self, a: f64) -> Option<((f64, f64), f32, f32)> {
        let i = self.across.partition_point(|&x| x < a);
        if i == 0 || i >= self.across.len() {
            return None;
        }
        let (a0, a1) = (self.across[i - 1], self.across[i]);
        // A metre is a hundred times the sample spacing, so this only trips
        // across a genuine gap -- the nadir blank, or samples dropped past the
        // end of the trace. Bridging one would invent seabed.
        if a1 - a0 > 1.0 {
            return None;
        }
        let f = (a - a0) / (a1 - a0).max(1e-9);
        let (p0, p1) = (self.px[i - 1], self.px[i]);
        Some((
            (p0.0 + (p1.0 - p0.0) * f, p0.1 + (p1.1 - p0.1) * f),
            self.amp[i - 1] + (self.amp[i] - self.amp[i - 1]) * f as f32,
            self.prio[i - 1] + (self.prio[i] - self.prio[i - 1]) * f as f32,
        ))
    }
}

/// Draw a run of samples between two projected points.
fn bridge(
    g: &mut Grid, a: ((f64, f64), f32, f32), b: ((f64, f64), f32, f32),
    x0: i64, y0: i64, w: usize, h: usize, cap: usize,
) {
    let d = ((b.0 .0 - a.0 .0).powi(2) + (b.0 .1 - a.0 .1).powi(2)).sqrt();
    let steps = (d.ceil() as usize).clamp(1, cap);
    for s in 0..steps {
        let f = (s as f64 + 0.5) / steps as f64;
        splat(
            g,
            a.0 .0 + (b.0 .0 - a.0 .0) * f,
            a.0 .1 + (b.0 .1 - a.0 .1) * f,
            a.1 + (b.1 - a.1) * f as f32,
            a.2 + (b.2 - a.2) * f as f32,
            x0, y0, w, h,
        );
    }
}

/// Paint one ping, and the strip between it and the one before.
///
/// Painting only the across-track line leaves the along-track gaps: the fish
/// advances about 0.15 m a ping and a cell at zoom 19 is 0.18 m, so
/// consecutive lines land a pixel or so apart and rounding drops whichever
/// cells fall between them. That is 18% of the painted area on this survey,
/// almost all of it single pixels -- speckle through the middle of otherwise
/// good imagery. Two adjacent pings looked at the same seabed a tenth of a
/// second apart, so filling between them is interpolation, not invention.
fn paint(
    g: &mut Grid, st: &Stroke, prev: &mut Line,
    x0: i64, y0: i64, w: usize, h: usize, z: f64,
) {
    let brg = (st.bearing + 90.0).to_radians(); // starboard
    let (m_lat, m_lon) = geo::local_scale(st.lat);
    let (sb, cb) = (brg.sin(), brg.cos());

    let mut cur = Line {
        across: st.across.clone(),
        px: Vec::with_capacity(st.across.len()),
        amp: st.amp.clone(),
        prio: st.prio.clone(),
    };
    for &a in &st.across {
        let lat = st.lat + a * cb / m_lat;
        let lon = st.lon + a * sb / m_lon;
        cur.px.push(geo::lonlat_to_px(lon, lat, z));
    }

    // Across the swath.
    for i in 0..cur.px.len() {
        let here = (cur.px[i], cur.amp[i], cur.prio[i]);
        if i == 0 {
            splat(g, here.0 .0, here.0 .1, here.1, here.2, x0, y0, w, h);
        } else {
            bridge(g, (cur.px[i - 1], cur.amp[i - 1], cur.prio[i - 1]), here,
                   x0, y0, w, h, 64);
        }
    }

    // ...and along the track, to the same across-track distance on the
    // previous ping. Skipped where the fish jumped: a turn, a data gap, or the
    // seam between two files is not something to interpolate over.
    if !prev.px.is_empty() {
        for i in 0..cur.px.len() {
            let Some(back) = prev.at(cur.across[i]) else { continue };
            let d = ((cur.px[i].0 - back.0 .0).powi(2)
                   + (cur.px[i].1 - back.0 .1).powi(2)).sqrt();
            if d > 12.0 {
                continue;
            }
            bridge(g, back, (cur.px[i], cur.amp[i], cur.prio[i]), x0, y0, w, h, 16);
        }
    }

    *prev = cur;
}

#[inline]
fn splat(g: &mut Grid, px: f64, py: f64, amp: f32, pri: f32, x0: i64, y0: i64, w: usize, h: usize) {
    if pri <= 0.0 || !amp.is_finite() {
        return;
    }
    let ix = px.floor() as i64 - x0;
    let iy = py.floor() as i64 - y0;
    if ix < 0 || iy < 0 || ix as usize >= w || iy as usize >= h {
        return;
    }
    let i = iy as usize * w + ix as usize;
    g.sum[i] += amp * pri;
    g.wgt[i] += pri;
    g.hits[i] = g.hits[i].saturating_add(1);
}

/// Raise priority to `exponent` so overlapping looks are selected between
/// rather than averaged. Applied when the stroke is built.
pub fn shape_priority(p: f64, exponent: f64) -> f64 {
    p.max(0.0).powf(exponent)
}

/// How a mosaic is coloured on the way out of the raster.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MosaicStyle {
    pub ramp: crate::layer::Ramp,
    #[serde(default)]
    pub reverse: bool,
    /// Ends of the 0-255 value range mapped across the ramp. Narrowing them is
    /// a contrast stretch that does not need a repaint.
    #[serde(default)]
    pub lo: u8,
    #[serde(default = "full")]
    pub hi: u8,
}

fn full() -> u8 {
    255
}

impl Default for MosaicStyle {
    fn default() -> MosaicStyle {
        MosaicStyle { ramp: crate::layer::Ramp::Grey, reverse: false, lo: 0, hi: 255 }
    }
}

impl MosaicStyle {
    fn is_plain(&self) -> bool {
        self.ramp == crate::layer::Ramp::Grey && !self.reverse && self.lo == 0 && self.hi == 255
    }

    /// Recolour a grey RGBA tile in place, leaving transparent pixels alone.
    pub fn apply(&self, rgba: &mut [u8]) {
        // The grey ramp with the full range is what the raster already holds,
        // so the common case does no work at all.
        if self.is_plain() {
            return;
        }
        let lo = self.lo as f32;
        let span = (self.hi as f32 - lo).abs().max(1.0);
        // 256 entries, built once per tile rather than per pixel.
        let mut lut = [[0u8; 3]; 256];
        for (v, out) in lut.iter_mut().enumerate() {
            let mut t = ((v as f32 - lo) / span).clamp(0.0, 1.0);
            if self.reverse {
                t = 1.0 - t;
            }
            *out = self.ramp.at(t);
        }
        for px in rgba.chunks_exact_mut(4) {
            if px[3] == 0 {
                continue;
            }
            let c = lut[px[0] as usize];
            px[0] = c[0];
            px[1] = c[1];
            px[2] = c[2];
        }
    }

    /// Colour a greyscale image, for the waterfall.
    ///
    /// The waterfall and the mosaic are two views of the same 0-255 numbers, so
    /// a channel drawn in one should be able to wear the colours it was given
    /// in the other -- otherwise the operator is asked to hold two colour
    /// vocabularies at once for the same seabed.
    pub fn colour_grey(&self, grey: &[u8]) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(grey.len() * 4);
        for &g in grey {
            rgba.extend_from_slice(&[g, g, g, 255]);
        }
        self.apply(&mut rgba);
        rgba
    }

    pub fn is_grey(&self) -> bool {
        self.is_plain()
    }
}

/// How hard the tile encoder tries.
///
/// Fast, because the channel narrowing below already took the bytes that were
/// there to take, and what is left is a poor trade: measured over a screen at
/// z17, the default level saved 1.1 MB of a 7.6 MB screen and cost 106 ms of
/// the server's time to do it -- time spent holding a connection the local
/// imagery wants.
const TILE_COMPRESSION: png::Compression = png::Compression::Fast;

/// Encode an RGBA tile as the narrowest PNG that holds it exactly.
///
/// A sidescan mosaic is grey: the three colour channels carry the same number,
/// and shipping all four cost twice the bytes to say it. A shaded relief tile
/// is not grey but is usually fully opaque, which saves a channel just the
/// same. Nothing here is lossy -- the narrowing only happens when the wider
/// form was carrying duplicates -- so a ramped tile still goes out as RGBA.
///
/// Worth the scan: a screen of imagery was fourteen megabytes of PNG, all of
/// which the browser had to decode on the thread that draws.
pub fn encode_png_rgba(rgba: &[u8], w: usize, h: usize) -> io::Result<Vec<u8>> {
    let grey = rgba.chunks_exact(4).all(|p| p[0] == p[1] && p[1] == p[2]);
    let opaque = rgba.chunks_exact(4).all(|p| p[3] == 255);
    let (colour, packed) = match (grey, opaque) {
        (true, true) => (
            png::ColorType::Grayscale,
            rgba.chunks_exact(4).map(|p| p[0]).collect::<Vec<u8>>(),
        ),
        (true, false) => (
            png::ColorType::GrayscaleAlpha,
            rgba.chunks_exact(4).flat_map(|p| [p[0], p[3]]).collect(),
        ),
        (false, true) => (
            png::ColorType::Rgb,
            rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
        ),
        (false, false) => (png::ColorType::Rgba, rgba.to_vec()),
    };
    encode(&packed, w, h, colour)
}

fn encode(data: &[u8], w: usize, h: usize, colour: png::ColorType) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w as u32, h as u32);
        enc.set_color(colour);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(TILE_COMPRESSION);
        let mut wr = enc.write_header().map_err(to_io)?;
        wr.write_image_data(data).map_err(to_io)?;
    }
    Ok(buf)
}

/// Encode an 8-bit grey image as PNG.
pub fn encode_png_grey(grey: &[u8], w: usize, h: usize) -> io::Result<Vec<u8>> {
    encode(grey, w, h, png::ColorType::Grayscale)
}

fn to_io(e: png::EncodingError) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// Pings the angle-varying gain is measured over.
///
/// Spread evenly across the recording, so the curve is the sonar's signature
/// and not the seabed under one part of the line. Six hundred pings at 512
/// samples a side is three hundred thousand samples per bin-full, which is far
/// more than the median needs and still under a second to read.
const ANGLE_SAMPLE_PINGS: usize = 600;

/// One bin per degree from vertical.
const ANGLE_BINS: usize = 90;

/// Measure the across-track shading this recording actually has, per side.
///
/// The same ground mapping `build_stroke` uses, because a curve measured
/// against one set of angles and applied against another would shade the swath
/// with a shape that belongs to neither.
fn measure_angle_gain(
    index: &PingIndex,
    cfg: &MosaicConfig,
    pairs: &[crate::waterfall::Row],
    priors: &[f32],
    alpha_db_per_m: f64,
) -> (signal::AngleGain, signal::AngleGain) {
    if cfg.angular_gain <= 0.0 || pairs.is_empty() {
        return (signal::AngleGain::flat(), signal::AngleGain::flat());
    }
    let step = (pairs.len() / ANGLE_SAMPLE_PINGS).max(1);
    let picks: Vec<usize> = (0..pairs.len()).step_by(step).collect();

    let binned: Vec<(Vec<Vec<f32>>, Vec<Vec<f32>>)> = picks
        .par_iter()
        .map(|&k| {
            let mut port: Vec<Vec<f32>> = vec![Vec::new(); ANGLE_BINS];
            let mut stbd: Vec<Vec<f32>> = vec![Vec::new(); ANGLE_BINS];
            let (p, s) = (pairs[k].port, pairs[k].stbd);
            let Some(rec) = pairs[k].own().map(|i| index.records[i as usize]) else {
                return (port, stbd);
            };
            let open = |i: Option<u32>| -> Option<Vec<f32>> {
                let r = index.records[i? as usize];
                let jf = index.file_of(&r).ok()?;
                jf.ping_at(r.offset).map(|q| q.data)
            };
            let (pd, sd) = (open(p), open(s));
            let Some(t0) = pd.as_ref().or(sd.as_ref()).cloned() else {
                return (port, stbd);
            };
            let res = rec.resolution_m(cfg.sound_speed_m_s).max(1e-6);
            let prior = if priors[k] > 0.0 {
                priors[k]
            } else {
                signal::pick_bottom_trace(&t0, 8, crate::waterfall::BOTTOM_FRAC, 9)
            };
            let alt_m = (signal::refine_bottom_within(
                &t0, prior, crate::waterfall::BOTTOM_FRAC,
                crate::waterfall::BOTTOM_LO, crate::waterfall::BOTTOM_HI,
            ) as f64 * res).max(0.5);
            let n = t0.len();
            let mut half_m = n as f64 * res;
            if let Some(m) = cfg.max_range_m {
                half_m = half_m.min(m);
            }
            let per_side = cfg.across / 2;
            let r_ref = alt_m.max(1.0);
            for (trace, out) in [(&pd, &mut port), (&sd, &mut stbd)] {
                let Some(t) = trace else { continue };
                for i in 0..per_side {
                    let g = half_m * (i as f64 + 0.5) / per_side as f64;
                    if g < cfg.nadir_blank_m {
                        continue;
                    }
                    let slant = (g * g + alt_m * alt_m).sqrt();
                    if slant / res >= n as f64 - 1.0 {
                        continue;
                    }
                    let v = signal::sample_at(t, (slant / res) as f32)
                        * signal::tvg_gain(slant, r_ref, alpha_db_per_m, cfg.tvg as f64);
                    if v > 0.0 {
                        let b = (g.atan2(alt_m).to_degrees() as usize).min(ANGLE_BINS - 1);
                        out[b].push(v);
                    }
                }
            }
            (port, stbd)
        })
        .collect();

    let mut port: Vec<Vec<f32>> = vec![Vec::new(); ANGLE_BINS];
    let mut stbd: Vec<Vec<f32>> = vec![Vec::new(); ANGLE_BINS];
    for (p, s) in binned {
        for b in 0..ANGLE_BINS {
            port[b].extend_from_slice(&p[b]);
            stbd[b].extend_from_slice(&s[b]);
        }
    }
    (
        signal::AngleGain::measure(&mut port, cfg.angular_gain),
        signal::AngleGain::measure(&mut stbd, cfg.angular_gain),
    )
}
