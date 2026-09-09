//! The waterfall: a block of pings rendered as an image, plus enough metadata
//! per row that any pixel in it can be turned back into a position on the
//! seabed.
//!
//! That last part is the point. Marking a contact from the waterfall and
//! marking it from the map have to put the pin in the same place, so the
//! forward transform used to draw the mosaic and the inverse used to resolve a
//! click are the same arithmetic, written once here and in `mosaic`.

use std::collections::BTreeMap;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::geo;
use crate::index::{PingIndex, PingRecord};
use crate::jsf::JsfFile;
use crate::nav::{Fix, Nav};
use crate::signal;

/// Across-track axis: raw slant range, or slant-corrected ground range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Slant,
    #[default]
    Ground,
}

impl Axis {
    /// A distance along the image's own across-track axis, as ground range.
    ///
    /// In ground mode the axis already *is* ground range. In slant mode a
    /// column at slant range `d` looks down on seabed `sqrt(d^2 - alt^2)` away,
    /// and inside the water column (`|d| < alt`) it looks at no seabed at all,
    /// which is why this saturates at nadir rather than going imaginary.
    pub fn to_ground(self, d: f64, alt: f64) -> f64 {
        match self {
            Axis::Ground => d,
            Axis::Slant => d.signum() * (d * d - alt * alt).max(0.0).sqrt(),
        }
    }

    /// The inverse: where a ground range lands on the image's own axis.
    pub fn from_ground(self, across: f64, alt: f64) -> f64 {
        match self {
            Axis::Ground => across,
            Axis::Slant => across.signum() * (across * across + alt * alt).sqrt(),
        }
    }
}

/// How the bottom is picked, shared with the mosaic so the two views cannot
/// disagree about where the seabed is.
///
/// The threshold is a fraction of the window's contrast. It used to be 0.25,
/// which is crossed by water-column returns several metres before the seabed:
/// on `070926_measures_star` that put the pick at 15.3 m where the return is at
/// 21.5 m and the sonar's own tracker said 20.0.
///
/// The window is asymmetric, and that is the point. Tightening the *lower*
/// bound is what stops an early pick. Tightening the upper bound as well looked
/// tidier and was wrong: `wpa20260906`'s recorded altitude runs about 20% short
/// of the return, so a ceiling of 1.18 could not reach the seabed at all and
/// doubled the darkness under the fish -- 9.4% of the inner swath to 19.2%.
/// Measured over five blocks of six hundred pings on three recordings, the
/// values below take the inner swath from 9.4/10.8/9.4% dark to 6.2/7.1/7.0%.
pub const BOTTOM_FRAC: f32 = 0.55;
pub const BOTTOM_LO: f32 = 0.90;
pub const BOTTOM_HI: f32 = 1.45;
/// Pings the prior is smoothed over. At 0.15 m a ping this is a metre of
/// seabed, which is flat compared with the five-metre jumps it removes.
pub const BOTTOM_MEDIAN: usize = 9;

fn default_agc() -> f32 {
    0.6
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaterfallRequest {
    pub subsystem: u8,
    /// First ping row, as a position within this channel's selection.
    pub start: usize,
    /// Number of pings to draw before decimation.
    pub count: usize,
    /// Output width in pixels, both channels together.
    pub width: usize,
    /// Draw one output row per `stride` pings.
    pub stride: usize,
    pub axis: Axis,
    /// Across-track flattening exponent; 0 disables it.
    pub tvg: f32,
    /// Along-track gain equalisation, 0 to 1. See `signal::row_gain`.
    #[serde(default = "default_agc")]
    pub agc: f32,
    pub gamma: f32,
    pub clip_lo: f64,
    pub clip_hi: f64,
    /// Clamp the across-track half-width, metres. None uses the full range.
    pub max_range_m: Option<f64>,
    /// Speed of sound in the water, m/s.
    ///
    /// The API overwrites whatever a client sends here with the recording's own
    /// setting, because the waterfall and the mosaic disagreeing about the
    /// speed of sound would put a contact marked in one somewhere else in the
    /// other -- which is the single thing this crate exists not to do.
    #[serde(default = "recorded_speed")]
    pub sound_speed_m_s: f64,
}

fn recorded_speed() -> f64 {
    crate::C_RECORDED
}

impl Default for WaterfallRequest {
    fn default() -> WaterfallRequest {
        WaterfallRequest {
            subsystem: 20,
            start: 0,
            count: 1024,
            width: 1024,
            stride: 1,
            axis: Axis::Ground,
            tvg: 0.7,
            agc: 0.6,
            gamma: 0.8,
            clip_lo: 1.0,
            clip_hi: 99.0,
            max_range_m: None,
            sound_speed_m_s: crate::C_RECORDED,
        }
    }
}

/// What one output row knows about itself.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct RowInfo {
    pub time: f64,
    pub fish_lat: f64,
    pub fish_lon: f64,
    pub boat_lat: f64,
    pub boat_lon: f64,
    pub bearing: f64,
    pub altitude: f64,
    pub depth: f64,
    pub roll: f64,
    pub clean: bool,
    /// Metres of across-track distance covered by the row, per side.
    pub half_width_m: f64,
    /// Along-track advance since the previous row, metres.
    pub advance_m: f64,
    /// Row position in the channel selection.
    pub ping_row: usize,
}

pub struct Waterfall {
    pub width: usize,
    pub height: usize,
    /// Row-major 8-bit grey.
    pub pixels: Vec<u8>,
    pub rows: Vec<RowInfo>,
    /// Across-track metres per pixel at the centre of the image.
    pub metres_per_px: f64,
    /// Which across-track axis the pixels were drawn on. Every inverse below
    /// needs it: a column offset is a *slant* range in slant mode, and reading
    /// it back as a ground range puts the seabed `sqrt(g^2+alt^2) - g` too far
    /// out -- the whole image sliding outward the moment the axis changes.
    pub axis: Axis,
}

impl Waterfall {
    /// Turn a pixel into a position on the seabed.
    ///
    /// `x` is measured from the left edge, so the port half is mirrored: the
    /// nadir sits at the centre and across-track distance grows outward.
    pub fn pixel_to_world(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let r = self.rows.get(y.floor().max(0.0) as usize)?;
        let half = self.width as f64 / 2.0;
        // signed distance along the image's axis: negative to port
        let d = (x - half) / half * r.half_width_m;
        // On the slant axis the band either side of nadir narrower than the
        // altitude is water. It has no position on the seabed, and answering
        // with the fish's own position -- which is what saturating at nadir
        // would do -- is a wrong answer dressed as a right one.
        if self.axis == Axis::Slant && d.abs() < r.altitude {
            return None;
        }
        let across = self.axis.to_ground(d, r.altitude);
        // starboard lies 90 degrees clockwise of the fish's axis
        let brg = r.bearing + 90.0;
        Some(geo::offset_m(r.fish_lat, r.fish_lon, brg, across))
    }

    /// The inverse: where a position falls in this image, if it falls in it at
    /// all. Used to put a map-drawn contact back on the waterfall.
    ///
    /// The row is the one that has the point *abeam*, not the one whose fish
    /// position is nearest. Those are not the same question: a target 40 m off
    /// the track is very nearly equidistant from every row for tens of metres
    /// either side, so nearest-position picks essentially at random -- and when
    /// the survey doubles back on itself, as a box or spiral does, it can pick
    /// a row from an entirely different pass. Minimising the along-track
    /// residual asks the question the geometry actually poses.
    pub fn world_to_pixel(&self, lat: f64, lon: f64) -> Option<(f64, f64)> {
        let mut best: Option<(f64, f64, usize)> = None; // |along|, across, row
        for (i, r) in self.rows.iter().enumerate() {
            let (m_lat, m_lon) = geo::local_scale(r.fish_lat);
            let dn = (lat - r.fish_lat) * m_lat;
            let de = (lon - r.fish_lon) * m_lon;
            let b = r.bearing.to_radians();
            // along the fish's axis, and 90 degrees clockwise of it
            let along = dn * b.cos() + de * b.sin();
            let across = -dn * b.sin() + de * b.cos();
            // Test the accept window on the image's own axis, not on the
            // ground: in slant mode the outer columns cover less ground than
            // their range suggests, so a ground-range test lets in points the
            // image does not actually show.
            let d = self.axis.from_ground(across, r.altitude);
            if d.abs() > r.half_width_m {
                continue;
            }
            let key = along.abs();
            if best.map_or(true, |(k, _, _)| key < k) {
                best = Some((key, d, i));
            }
        }
        let (_, d, row) = best?;
        let half = self.width as f64 / 2.0;
        let r = &self.rows[row];
        Some((half + d / r.half_width_m * half, row as f64 + 0.5))
    }
}

/// Read the traces for one ping row across both channels.
///
/// Port and starboard are separate messages that share a ping number; they are
/// paired here so one output row is one physical ping.
fn read_pair(
    index: &PingIndex,
    files: &mut Vec<Option<JsfFile>>,
    port: Option<&PingRecord>,
    stbd: Option<&PingRecord>,
) -> (Option<Vec<f32>>, Option<Vec<f32>>) {
    let mut get = |r: Option<&PingRecord>| -> Option<Vec<f32>> {
        let r = r?;
        let fid = r.file_id as usize;
        if files.get(fid).map_or(true, |f| f.is_none()) {
            while files.len() <= fid {
                files.push(None);
            }
            files[fid] = index.file_of(r).ok();
        }
        let jf = files[fid].as_ref()?;
        jf.ping_at(r.offset).map(|p| p.data)
    };
    (get(port), get(stbd))
}

/// Pair up port and starboard records by ping number within one subsystem.
pub struct ChannelPairs {
    pub port: Vec<u32>,
    pub stbd: Vec<u32>,
}

/// One row of a recording, seen from one band.
///
/// A row is a *moment*, not a record. Every band is fired by the same fish at
/// the same instant -- that is what makes a pair of them a comparison rather
/// than two pictures of roughly the same place -- so the row carrying the low
/// band's ping has to be the row carrying the high band's, and a band with
/// nothing at that instant leaves the slot empty rather than shifting
/// everything after it up by one.
#[derive(Clone, Copy, Debug, Default)]
pub struct Row {
    /// This band's port and starboard records. Either or both may be missing.
    pub port: Option<u32>,
    pub stbd: Option<u32>,
    /// The record that stands for this instant, the same one in every band.
    /// Where the fish was, when, and how high: properties of the tow rather
    /// than of the band, and every row has one, because a row exists only
    /// because a ping happened.
    pub at: u32,
}

impl Row {
    /// This band's own record, if it caught this ping.
    pub fn own(&self) -> Option<u32> {
        self.port.or(self.stbd)
    }
}

/// Records within this fraction of a ping interval of each other are one ping.
///
/// A row is a transmit cycle, not an instant, because the bands are not always
/// fired together. Measured over these surveys: `070926_measures_b2`,
/// `070926_measures` and `080929_demimines` stamp the two bands within 2 ms of
/// each other, and `wpa20260906` interleaves them, firing the high band 42 ms
/// after the low one. Both are one cycle of one fish; 42 ms is six centimetres
/// of tow. So the window has to be wide enough for the stagger and narrower
/// than the 69 ms interval, and three quarters of an interval -- 52 ms -- sits
/// between the two with room on both sides.
///
/// The window is not what keeps two cycles apart, though. That is the rule in
/// `pair_bands` that a record closes the row when its slot is already filled,
/// which holds however the interval jitters -- and it does jitter, down to
/// 15 ms on `wpa20260906`.
const SAME_PING: f64 = 0.75;

/// The recording's row space: every band, on one shared set of rows.
///
/// Pairing per subsystem instead -- which is what this used to do, grouping by
/// the ping counter each band keeps for itself -- got both of the things a pane
/// beside another pane depends on wrong. The two bands of `080929_demimines`
/// came out one ping out of step for the whole recording, because that file
/// starts with a low ping and ends with a high one; and `070926_measures` came
/// out as two bands of *different lengths*, 30071 rows against 30012, the high
/// band having dropped 62 pings along the way. Two panes over one recording
/// then scrolled against each other, which is the one thing they must not do.
///
/// Here the rows come from the ping instants, and every band gets the same
/// number of them.
pub fn pair_bands(index: &PingIndex) -> BTreeMap<u8, Vec<Row>> {
    let subs = index.subsystems();
    let mut out: BTreeMap<u8, Vec<Row>> = subs.iter().map(|&s| (s, Vec::new())).collect();
    if index.records.is_empty() {
        return out;
    }
    let tol = ping_interval(index) * SAME_PING;

    let mut order: Vec<u32> = (0..index.records.len() as u32).collect();
    order.sort_by(|&a, &b| {
        index.records[a as usize].time.total_cmp(&index.records[b as usize].time)
    });

    // One pass, opening a row at the first record of an instant and closing it
    // when a record arrives that is either too late to belong or is a second
    // copy of a slot already filled -- the second being what keeps a tolerance
    // that is too generous from silently swallowing the next ping.
    let mut open: BTreeMap<u8, Row> = BTreeMap::new();
    let mut start = f64::NAN;
    let flush = |open: &mut BTreeMap<u8, Row>, out: &mut BTreeMap<u8, Vec<Row>>| {
        if open.is_empty() {
            return;
        }
        // Every band gets a row, whether or not it caught this ping, and every
        // band's row points at the same record for the fish. One instant, one
        // place, one answer -- whichever band is asked.
        let at = open.values().next().map(|r| r.at).unwrap_or(0);
        for (s, v) in out.iter_mut() {
            let mut row = open.remove(s).unwrap_or_default();
            row.at = at;
            v.push(row);
        }
    };

    for i in order {
        let r = &index.records[i as usize];
        let taken = open
            .get(&r.subsystem)
            .is_some_and(|x| if r.channel == 0 { x.port.is_some() } else { x.stbd.is_some() });
        if !start.is_nan() && (r.time - start > tol || taken) {
            flush(&mut open, &mut out);
            start = f64::NAN;
        }
        if start.is_nan() {
            start = r.time;
        }
        let slot = open.entry(r.subsystem).or_insert(Row { port: None, stbd: None, at: i });
        if r.channel == 0 {
            slot.port = Some(i);
        } else {
            slot.stbd = Some(i);
        }
    }
    flush(&mut open, &mut out);
    out
}

/// Seconds between pings, from the busiest channel in the recording.
///
/// One channel of one band, because that is a clean sequence: taking gaps
/// across the whole index would measure the interleaving of the bands instead.
fn ping_interval(index: &PingIndex) -> f64 {
    let mut best: Vec<f64> = Vec::new();
    for (s, c) in index.channels() {
        let t: Vec<f64> = index
            .records
            .iter()
            .filter(|r| r.subsystem == s && r.channel == c)
            .map(|r| r.time)
            .collect();
        if t.len() > best.len() {
            best = t;
        }
    }
    let mut gaps: Vec<f64> =
        best.windows(2).map(|w| w[1] - w[0]).filter(|g| g.is_finite() && *g > 0.0).collect();
    if gaps.is_empty() {
        return 0.1;
    }
    gaps.sort_by(f64::total_cmp);
    gaps[gaps.len() / 2]
}

/// One band's rows. The rows are the recording's; see `pair_bands`.
pub fn pair_channels(index: &PingIndex, subsystem: u8) -> Vec<Row> {
    pair_bands(index).remove(&subsystem).unwrap_or_default()
}

/// Pings a range group's gain model is measured over.
///
/// Spread evenly across the whole group rather than taken from the front of
/// it, so the model sees the recording and not its first minute. Twelve
/// hundred is about half a block's worth of decode, paid once for a recording
/// instead of once for every block of it.
const GAIN_SAMPLE_PINGS: usize = 1200;

/// Range settings closer together than this are the same setting, metres.
///
/// The recorded range is `nsamples * interval`, both of which the operator set,
/// so it does not drift. This only has to merge the 49.92 m and 49.99 m that
/// `wpa20260906` writes for what is plainly one 50 m range.
const RANGE_BUCKET_M: f64 = 0.5;

/// Which range setting a ping was recorded at.
fn range_bucket(r: &PingRecord, c: f64) -> u32 {
    (r.slant_range_m(c) / RANGE_BUCKET_M).round().max(0.0) as u32
}

/// One ping's traces and the record they came from.
struct Raw {
    port: Option<Vec<f32>>,
    stbd: Option<Vec<f32>>,
    rec: PingRecord,
}

/// The frame and the gain model for one range setting.
#[derive(Clone, Debug)]
struct RangeGain {
    bucket: u32,
    pings: usize,
    /// Half-width of the image on the axis it was built for, metres.
    half_width_m: f64,
    /// Across-track flattening, per side.
    port: Vec<f32>,
    stbd: Vec<f32>,
    /// The level every row is pulled towards.
    reference: f32,
    stretch: signal::Stretch,
}

/// Everything a waterfall row needs that is a property of the *recording*
/// rather than of the block it happens to fall in.
///
/// Four separate quantities used to be measured from whatever 2048-ping block
/// the viewer asked for: the frame width, the across-track profile, the level
/// the along-track equalisation pulls towards, and the contrast stretch. All
/// four are estimates of the same thing at different block boundaries, so all
/// four stepped every 2048 pings, and the picture was a function of where the
/// buffer happened to start. Measured on `080929_demimines`, the mean grey
/// stepped a median of 4.06 levels at a seam against 1.34 between two ordinary
/// neighbouring pings, and rendering the same pings inside two differently
/// aligned blocks agreed on 3.4% of their pixels.
///
/// The model is per *range setting*, not per recording, because the operator
/// changing the range is a real event that has to change the frame -- 23% of
/// `wpa20260906` is recorded at 30 m against 50 m for the rest, and drawing
/// both on one axis would either clip the long pings or leave the short ones
/// in a black surround. Within one setting the picture is now seamless; where
/// the setting changes there is a seam, and it is the truth.
#[derive(Clone, Debug, Default)]
pub struct GainModel {
    /// Sorted by ping count, descending, so `groups[0]` is the setting the
    /// recording was mostly flown at.
    groups: Vec<RangeGain>,
}

impl GainModel {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The model for a range setting, falling back to the dominant one.
    ///
    /// The fallback is for a bucket that held too few pings to be sampled at
    /// all. Its frame will be wrong for those pings, which is worth knowing,
    /// but it is a handful of pings against a stable picture for the rest.
    fn for_bucket(&self, b: u32) -> Option<&RangeGain> {
        self.groups.iter().find(|g| g.bucket == b).or_else(|| self.groups.first())
    }
}

/// Measure the gain model for a recording, one range setting at a time.
///
/// `req` supplies everything about *how* the waterfall is drawn -- axis, width,
/// the strength of both gains, the clip points -- and nothing about which pings
/// are on screen. `start`, `count` and `stride` are ignored.
pub fn build_gain_model(
    index: &PingIndex,
    pairs: &[Row],
    req: &WaterfallRequest,
) -> GainModel {
    let width = req.width.max(16) & !1;
    let half = width / 2;
    let c = req.sound_speed_m_s;

    // Which pings share a range setting.
    let mut buckets: Vec<(u32, Vec<usize>)> = Vec::new();
    for (i, row) in pairs.iter().enumerate() {
        let Some(rec) = row.own().map(|j| index.records[j as usize]) else { continue };
        let b = range_bucket(&rec, c);
        match buckets.iter_mut().find(|(k, _)| *k == b) {
            Some((_, v)) => v.push(i),
            None => buckets.push((b, vec![i])),
        }
    }

    let mut groups: Vec<RangeGain> = Vec::new();
    for (bucket, members) in buckets {
        // Evenly across the group, so the sample is of the whole recording.
        let step = (members.len() / GAIN_SAMPLE_PINGS).max(1);
        let picks: Vec<usize> = members.iter().copied().step_by(step).collect();
        if picks.is_empty() {
            continue;
        }
        let raw = decode(index, pairs, &picks);
        let alts = altitudes(&raw, c);

        // The frame is this setting's own range, corrected to ground against
        // the altitude the fish held over the group. A row flown higher than
        // that loses its outermost pixels; the alternative -- a frame that
        // follows each row's own altitude -- makes a straight pipeline look
        // wavy as the fish rises and falls, which is worse.
        let slant = signal::median_f64(
            &raw.iter()
                .map(|r| {
                    let n = r.port.as_ref().or(r.stbd.as_ref()).map_or(0, |v| v.len());
                    n as f64 * r.rec.resolution_m(c)
                })
                .filter(|v| *v > 0.0)
                .collect::<Vec<_>>(),
        );
        let alt = signal::median_f64(
            &alts.iter().map(|a| a.2).filter(|v| *v > 0.0).collect::<Vec<_>>(),
        );
        let half_width_m = frame_half_width(slant, alt, req.axis, req.max_range_m);

        let side: Vec<(Vec<f32>, Vec<f32>)> = raw
            .par_iter()
            .zip(alts.par_iter())
            .map(|(r, &(alt_s, res, _))| resample(r, alt_s, res, half, half_width_m, req.axis))
            .collect();

        let (port, stbd) = profiles(&side, half, req.tvg);
        let levels: Vec<f32> = side
            .iter()
            .map(|(p, s)| signal::percentile(&[&p[..], &s[..]].concat(), 60.0))
            .collect();
        let reference = signal::median(&levels);

        // The stretch has to be measured on rows that have already been
        // flattened and equalised, because that is what will be handed to it.
        let mut all: Vec<f32> = Vec::with_capacity(side.len() * width);
        for (p, s) in &side {
            let g = signal::row_gain(&[&p[..], &s[..]].concat(), req.agc, reference).max(1e-6);
            for i in 0..half {
                all.push(p[i] / port[i].max(1e-9) / g);
                all.push(s[i] / stbd[i].max(1e-9) / g);
            }
        }
        let stretch = signal::Stretch::from_data(&all, req.clip_lo, req.clip_hi, req.gamma);

        groups.push(RangeGain {
            bucket,
            pings: members.len(),
            half_width_m,
            port,
            stbd,
            reference,
            stretch,
        });
    }
    groups.sort_by(|a, b| b.pings.cmp(&a.pings));
    GainModel { groups }
}

/// Read the traces for a set of rows, in parallel.
fn decode(index: &PingIndex, pairs: &[Row], picks: &[usize]) -> Vec<Raw> {
    picks
        .par_iter()
        .map(|&pi| {
            let row = pairs[pi];
            let mut files: Vec<Option<JsfFile>> = Vec::new();
            let pr = row.port.map(|i| index.records[i as usize]);
            let sr = row.stbd.map(|i| index.records[i as usize]);
            let (pd, sd) = read_pair(index, &mut files, pr.as_ref(), sr.as_ref());
            // A row this band missed still knows where the fish was and when,
            // from a band that did not miss it. Only the picture is absent.
            Raw {
                port: pd,
                stbd: sd,
                rec: pr.or(sr).unwrap_or(index.records[row.at as usize]),
            }
        })
        .collect()
}

/// Altitude per row: `(samples, metres per sample, metres)`.
///
/// Resolved before anything is resampled, because the ground axis needs to know
/// how far the traces actually reach. The recorded altitude is the prior; the
/// trace says where the first return really is. See `signal::refine_bottom`.
///
/// The prior is the sonar's own bottom track, as a sample index converted back
/// at the speed *it* wrote the metres with, and smoothed along track before it
/// is used -- see `signal::steady_prior`. The refinement is then held close to
/// it, because a bottom pick that wanders is not a better answer than a steady
/// one, it is a stripe across the picture.
fn altitudes(raw: &[Raw], c: f64) -> Vec<(f32, f64, f64)> {
    let priors: Vec<f32> = raw.iter().map(|r| r.rec.altitude_samples().unwrap_or(0.0)).collect();
    let priors = signal::steady_prior(&priors, BOTTOM_MEDIAN);
    raw.par_iter()
        .zip(priors.par_iter())
        .map(|(r, &p)| {
            let res = r.rec.resolution_m(c).max(1e-6);
            let t0 = r.port.as_ref().or(r.stbd.as_ref());
            let prior = if p > 0.0 {
                p
            } else {
                t0.map_or(1.0, |t| signal::pick_bottom_trace(t, 8, BOTTOM_FRAC, 9))
            };
            let alt_s = t0
                .map(|t| signal::refine_bottom_within(t, prior, BOTTOM_FRAC, BOTTOM_LO, BOTTOM_HI))
                .unwrap_or(prior)
                .max(1.0);
            (alt_s, res, alt_s as f64 * res)
        })
        .collect()
}

/// The extent of the image's own across-track axis, metres per side.
///
/// In ground mode it has to be a ground range: a trace reaching `slant` of
/// slant only reaches `sqrt(slant^2 - alt^2)` of ground, and asking for more
/// samples runs past the end of the trace and comes back black -- 23 px a side
/// on a 50 m swath at 15 m altitude -- while `world_to_pixel` would accept
/// positions out in a band the image never shows.
fn frame_half_width(slant_m: f64, alt_m: f64, axis: Axis, max_range_m: Option<f64>) -> f64 {
    let slant = if slant_m > 0.0 { slant_m } else { 1.0 };
    let mut hw = match axis {
        Axis::Slant => slant,
        // A fish flying higher than its own swath is a broken recording, not a
        // reason to collapse the image to nothing.
        Axis::Ground => {
            let sq = slant * slant;
            (sq - alt_m * alt_m).max(sq * 0.25).sqrt()
        }
    };
    if let Some(m) = max_range_m {
        hw = hw.min(m);
    }
    hw
}

/// Resample one ping's two sides onto the image's across-track axis.
fn resample(
    r: &Raw,
    alt_s: f32,
    res: f64,
    half: usize,
    half_width_m: f64,
    axis: Axis,
) -> (Vec<f32>, Vec<f32>) {
    let one = |trace: &Option<Vec<f32>>| -> Vec<f32> {
        let Some(t) = trace else { return vec![0.0; half] };
        (0..half)
            .map(|i| {
                // outward from nadir, along the image's own axis
                let d = half_width_m * (i as f64 + 0.5) / half as f64;
                let s = match axis {
                    Axis::Slant => d / res,
                    // ground -> slant, on a flat seabed one altitude below
                    Axis::Ground => {
                        let g = d / res;
                        (g * g + (alt_s as f64) * (alt_s as f64)).sqrt()
                    }
                };
                signal::sample_at(t, s as f32)
            })
            .collect()
    };
    (one(&r.port), one(&r.stbd))
}

/// The across-track flattening for a set of rows, per side.
fn profiles(side: &[(Vec<f32>, Vec<f32>)], half: usize, tvg: f32) -> (Vec<f32>, Vec<f32>) {
    if tvg <= 0.0 {
        return (vec![1.0; half], vec![1.0; half]);
    }
    let pv: Vec<Vec<f32>> = side.iter().map(|s| s.0.clone()).collect();
    let sv: Vec<Vec<f32>> = side.iter().map(|s| s.1.clone()).collect();
    (signal::tvg_profile(&pv, half, tvg), signal::tvg_profile(&sv, half, tvg))
}

/// Render a waterfall.
///
/// `gains` is the recording's own gain model. Passing `None` measures
/// everything from this block alone, which is what the function used to do
/// always, and which makes the picture depend on where the block boundaries
/// fall -- see `GainModel`.
pub fn render(
    index: &PingIndex,
    nav: &Nav,
    pairs: &[Row],
    req: &WaterfallRequest,
    gains: Option<&GainModel>,
) -> Waterfall {
    let stride = req.stride.max(1);
    let width = req.width.max(16) & !1; // even, so the two halves are equal
    let half = width / 2;
    let end = (req.start + req.count).min(pairs.len());
    let picks: Vec<usize> = (req.start..end).step_by(stride).collect();
    let height = picks.len();

    if height == 0 {
        return Waterfall {
            width,
            height: 0,
            pixels: Vec::new(),
            rows: Vec::new(),
            metres_per_px: f64::NAN,
            axis: req.axis,
        };
    }

    let raw = decode(index, pairs, &picks);
    let alts = altitudes(&raw, req.sound_speed_m_s);

    // The frame this block would set for itself: the shortest full-range ping
    // in it, so every row fills the image rather than the widest one setting a
    // scale the rest cannot reach. Used where there is no model, and as the
    // fallback for a range setting the model never saw.
    let mut halves: Vec<f64> = raw
        .iter()
        .map(|r| {
            let n = r.port.as_ref().or(r.stbd.as_ref()).map_or(0, |v| v.len());
            n as f64 * r.rec.resolution_m(req.sound_speed_m_s)
        })
        .filter(|v| *v > 0.0)
        .collect();
    halves.sort_by(f64::total_cmp);
    let block_slant = if halves.is_empty() { 1.0 } else { halves[halves.len() / 2] };
    let mut ms: Vec<f64> = alts.iter().map(|a| a.2).filter(|v| *v > 0.0).collect();
    ms.sort_by(f64::total_cmp);
    let block_alt = if ms.is_empty() { 0.0 } else { ms[ms.len() / 2] };
    let block_frame = frame_half_width(block_slant, block_alt, req.axis, req.max_range_m);

    // One frame per row, so a range change lands on the ping it happened at
    // rather than on the next block boundary.
    let buckets: Vec<u32> =
        raw.iter().map(|r| range_bucket(&r.rec, req.sound_speed_m_s)).collect();
    let frames: Vec<f64> = match gains {
        Some(g) => buckets
            .iter()
            .map(|&b| g.for_bucket(b).map_or(block_frame, |x| x.half_width_m))
            .collect(),
        None => vec![block_frame; height],
    };

    let side: Vec<(Vec<f32>, Vec<f32>)> = raw
        .par_iter()
        .zip(alts.par_iter())
        .zip(frames.par_iter())
        .map(|((r, &(alt_s, res, _)), &hw)| resample(r, alt_s, res, half, hw, req.axis))
        .collect();

    // The flattening, the level to equalise towards and the stretch: from the
    // recording where there is a model for this row's range setting, and from
    // this block alone where there is not.
    struct Flat {
        port: Vec<f32>,
        stbd: Vec<f32>,
        reference: f32,
        stretch: Option<signal::Stretch>,
    }
    let (flats, which): (Vec<Flat>, Vec<usize>) = match gains {
        Some(g) => {
            let mut keys: Vec<u32> = Vec::new();
            let mut which = Vec::with_capacity(height);
            for &b in &buckets {
                let k = match keys.iter().position(|&x| x == b) {
                    Some(i) => i,
                    None => {
                        keys.push(b);
                        keys.len() - 1
                    }
                };
                which.push(k);
            }
            let flats = keys
                .iter()
                .map(|&b| match g.for_bucket(b) {
                    Some(rg) => Flat {
                        port: rg.port.clone(),
                        stbd: rg.stbd.clone(),
                        reference: rg.reference,
                        stretch: Some(rg.stretch),
                    },
                    None => Flat {
                        port: vec![1.0; half],
                        stbd: vec![1.0; half],
                        reference: 0.0,
                        stretch: None,
                    },
                })
                .collect();
            (flats, which)
        }
        None => {
            // Computed on the block so the image is internally consistent even
            // when the gain changed mid-line, and against the median row level
            // rather than the first row, so one bad ping at the start cannot
            // set the scale for the block.
            let (port, stbd) = profiles(&side, half, req.tvg);
            let levels: Vec<f32> = side
                .iter()
                .map(|(p, s)| signal::percentile(&[&p[..], &s[..]].concat(), 60.0))
                .collect();
            let reference = signal::median(&levels);
            (vec![Flat { port, stbd, reference, stretch: None }], vec![0; height])
        }
    };

    let mut flat: Vec<Vec<f32>> = Vec::with_capacity(height);
    for (k, (p, s)) in side.iter().enumerate() {
        let f = &flats[which[k]];
        let g = signal::row_gain(&[&p[..], &s[..]].concat(), req.agc, f.reference).max(1e-6);
        let mut row = vec![0.0f32; width];
        // port is mirrored so nadir sits at the centre and the image reads as
        // a normal swath: port left, nadir middle, starboard right
        for i in 0..half {
            row[half - 1 - i] = p[i] / f.port[i].max(1e-9) / g;
            row[half + i] = s[i] / f.stbd[i].max(1e-9) / g;
        }
        flat.push(row);
    }

    // Where no row has a stretch of its own, the block sets one for all of
    // them -- which is the old behaviour, and the reason a block boundary used
    // to be visible.
    let block_stretch = flats.iter().all(|f| f.stretch.is_none()).then(|| {
        let all: Vec<f32> = flat.iter().flat_map(|r| r.iter().copied()).collect();
        signal::Stretch::from_data(&all, req.clip_lo, req.clip_hi, req.gamma)
    });
    let mut pixels = vec![0u8; width * height];
    pixels
        .par_chunks_mut(width)
        .zip(flat.par_iter())
        .zip(which.par_iter())
        .for_each(|((dst, src), &k)| {
            let st = flats[k].stretch.or(block_stretch).unwrap_or(signal::Stretch {
                lo: 0.0,
                hi: 1.0,
                gamma: req.gamma,
            });
            for (d, &v) in dst.iter_mut().zip(src.iter()) {
                *d = st.apply(v);
            }
        });

    // Row metadata, so a click resolves to a place.
    let mut rows = Vec::with_capacity(height);
    let mut prev: Option<Fix> = None;
    for (k, r) in raw.iter().enumerate() {
        let fix = nav.fix(&r.rec);
        let advance = prev
            .map(|p| geo::distance_m(p.lat, p.lon, fix.lat, fix.lon))
            .unwrap_or(0.0);
        rows.push(RowInfo {
            time: fix.time,
            fish_lat: fix.lat,
            fish_lon: fix.lon,
            boat_lat: fix.boat_lat,
            boat_lon: fix.boat_lon,
            bearing: fix.bearing,
            altitude: alts[k].2,
            depth: fix.depth,
            roll: fix.roll,
            clean: fix.clean,
            half_width_m: frames[k],
            advance_m: advance,
            ping_row: picks[k],
        });
        prev = Some(fix);
    }

    let mut mid = frames.clone();
    mid.sort_by(f64::total_cmp);
    Waterfall {
        width,
        height,
        pixels,
        rows,
        metres_per_px: mid[mid.len() / 2] / half as f64,
        axis: req.axis,
    }
}

/// The row in a channel's pairing recorded nearest an instant, and how far off
/// it is in seconds.
///
/// Asking by time rather than by position is what makes a pair of band crops a
/// comparison. A contact sits up to a swath off the track, so every row for
/// tens of metres either side is very nearly the same distance from it: a
/// search on position picks among them essentially at random, and on a
/// recording that loops over itself it can land on a different pass entirely,
/// which is the right seabed seen from the wrong look. `world_to_pixel` says
/// the same thing from the other end, which is why it asks which row has the
/// point *abeam* rather than which fish is nearest.
///
/// Every band shares the recording's rows, so one instant resolves to the same
/// row number in all of them -- which is the whole reason two bands can be set
/// side by side and argued from. See `pair_bands`.
pub fn row_at_time(index: &PingIndex, pairs: &[Row], t: f64) -> Option<(usize, f64)> {
    let mut best: Option<(usize, f64)> = None;
    for (i, row) in pairs.iter().enumerate() {
        // `at` rather than this band's own record: a row this band missed is
        // still a moment the recording passed through, and refusing to resolve
        // to it would make the answer depend on which band was asked.
        let dt = (index.records[row.at as usize].time - t).abs();
        if best.is_none_or(|(_, b)| dt < b) {
            best = Some((i, dt));
        }
    }
    best
}
