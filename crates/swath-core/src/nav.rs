//! Where the boat was, where the fish was, and which way the swath pointed.
//!
//! The layback model here is deliberately the plain one: a constant offset
//! astern of the tow point, along the smoothed course over ground. Six richer
//! models were tried against the multibeam -- tractrix, pure dead reckoning, a
//! complementary filter, a compass ODE, catenary-from-scope -- and a flat
//! offset captured essentially all of the available improvement, with three of
//! the others making at least one dataset worse. The cable length bounds the
//! offset magnitude to about a metre; what stays genuinely unknown is its
//! bearing during a turn, and none of the models recovered that.
//!
//! So: no cable physics. The number that matters is `layback_m`, and the honest
//! uncertainty is 5-10 m on a straight run without a reference grid.
//!
//! In a turn it is worse, and it is worth being precise about the direction of
//! the error rather than leaving it as "worse". Three placements are available
//! for a fish 48 m behind a vessel on an arc of radius R:
//!
//!   straight astern   on the vessel's own circle, radius R, but ahead of the
//!                     wake point -- the widest of the three
//!   the wake          where the vessel was 48 m ago, also radius R
//!   a tractrix        radius sqrt(R^2 - 48^2), well inside both
//!
//! A real towed body behaves like the third, and all three are selectable
//! through `LaybackModel`. Measured across these recordings, `Astern` sits
//! outside `Wake` on 83-98% of turning pings, and `Astern` differs from
//! `Tractrix` by 12-24 m at the median while turning and by up to 83 m in the
//! tightest turns.
//!
//! The part worth knowing is that the gap does not vanish on the straights.
//! It decays with distance sailed since the last turn -- 5.4 m within 25 m of
//! one, 1.8 m by 100 m, 0.7 m past 400 m -- because a towed body needs about
//! three cable lengths to settle. On a survey flown as short lines, 92% of
//! pings are inside that distance, so the steady state a constant offset
//! assumes is close to never reached.
//!
//! None of the three is known to be right for this rig. `Astern` is the
//! geometric convenience, `Tractrix` the taut-cable limit; real cable drag
//! makes the fish lag more than a pure pursuit curve, so the truth most likely
//! sits between `Wake` and `Tractrix`. The spread between them is the honest
//! uncertainty, and it is why the default is still `Astern`: changing it would
//! move every existing contact without any evidence that it moves them closer
//! to where they are.

use serde::{Deserialize, Serialize};

use crate::geo;
use crate::index::PingRecord;

/// Vessel positions, one row per distinct GNSS fix.
pub struct Track {
    pub time: Vec<f64>,
    pub lat: Vec<f64>,
    pub lon: Vec<f64>,
    /// Course over ground, unwrapped so interpolation does not cross 360.
    pub cog_unwrapped: Vec<f64>,
    /// Cumulative along-track distance in metres.
    pub dist: Vec<f64>,
    /// Speed over ground, smoothed, m/s.
    pub sog: Vec<f64>,
}

/// The shortest gap two GNSS epochs may be apart, in seconds.
///
/// The receiver updates at 1 Hz. Anything closer than this is the same epoch
/// reported twice: the two subsystems interleave their pings, so subsystem 21
/// can stamp a fix a millisecond after subsystem 20 stamped the *next* one.
/// Left in, those pairs put a real 2.6 m of travel over a 1 ms gap and the
/// speed comes out in the thousands of metres per second.
const MIN_FIX_GAP_S: f64 = 0.05;

/// One fix per position change, not per timestamp.
///
/// The sonar pings at ~14 Hz and the position updates at 1 Hz, so about
/// fourteen consecutive pings carry one fix verbatim. Deduplicating on the
/// timestamp keeps all fourteen -- now that the milliseconds are real, the
/// stamps differ -- and the along-track distance built from that stands still
/// for thirteen samples and then jumps. Keying on the position changing is what
/// is actually wanted, and it lets a ping's own sub-second time interpolate to
/// a place *between* two fixes.
pub fn fix_track(records: &[PingRecord]) -> Vec<(f64, f64, f64)> {
    let mut rows: Vec<(f64, f64, f64)> = records
        .iter()
        .filter(|r| r.lat.is_finite() && r.lon.is_finite() && (r.lat != 0.0 || r.lon != 0.0))
        .map(|r| (r.time, r.lat, r.lon))
        .collect();
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64, f64)> = Vec::with_capacity(rows.len() / 8);
    for r in rows {
        match out.last() {
            Some(&(t, la, lo)) => {
                if (la != r.1 || lo != r.2) && r.0 - t >= MIN_FIX_GAP_S {
                    out.push(r);
                }
            }
            None => out.push(r),
        }
    }
    out
}

/// Course in degrees from the track itself, over a +/-`baseline` sample span.
///
/// Far more stable than the fish's compass, which yaws with the swell, and it
/// is what actually defines a run line.
pub fn course_over_ground(lat: &[f64], lon: &[f64], baseline: usize) -> Vec<f64> {
    let n = lat.len();
    if n == 0 {
        return Vec::new();
    }
    let lat0 = lat.iter().sum::<f64>() / n as f64;
    let (m_lat, m_lon) = geo::local_scale(lat0);
    let lon0 = lon.iter().sum::<f64>() / n as f64;
    let x: Vec<f64> = lon.iter().map(|&l| (l - lon0) * m_lon).collect();
    let y: Vec<f64> = lat.iter().map(|&l| (l - lat0) * m_lat).collect();
    (0..n)
        .map(|i| {
            let i0 = i.saturating_sub(baseline);
            let i1 = (i + baseline).min(n - 1);
            (x[i1] - x[i0]).atan2(y[i1] - y[i0]).to_degrees().rem_euclid(360.0)
        })
        .collect()
}

/// Unwrap a degree series so it is continuous, ready for interpolation.
pub fn unwrap_deg(v: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(v.len());
    let mut prev = 0.0;
    for (i, &a) in v.iter().enumerate() {
        if i == 0 {
            out.push(a);
            prev = a;
            continue;
        }
        let mut d = a - prev;
        while d > 180.0 {
            d -= 360.0;
        }
        while d < -180.0 {
            d += 360.0;
        }
        prev += d;
        out.push(prev);
    }
    out
}

/// Centred moving average of odd width `n`, edges held.
pub fn smooth(v: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(1) | 1;
    if n == 1 || v.len() < 2 {
        return v.to_vec();
    }
    let h = n / 2;
    (0..v.len())
        .map(|i| {
            let a = i.saturating_sub(h);
            let b = (i + h + 1).min(v.len());
            v[a..b].iter().sum::<f64>() / (b - a) as f64
        })
        .collect()
}

/// Linear interpolation into a series whose x is sorted ascending. Values
/// outside the span clamp to the ends, as `np.interp` does.
pub fn interp(xs: &[f64], ys: &[f64], x: f64) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    if x <= xs[0] {
        return ys[0];
    }
    if x >= xs[xs.len() - 1] {
        return ys[ys.len() - 1];
    }
    let i = match xs.binary_search_by(|p| p.total_cmp(&x)) {
        Ok(i) => return ys[i],
        Err(i) => i,
    };
    let (x0, x1) = (xs[i - 1], xs[i]);
    let (y0, y1) = (ys[i - 1], ys[i]);
    if (x1 - x0).abs() < 1e-12 {
        return y0;
    }
    y0 + (y1 - y0) * (x - x0) / (x1 - x0)
}

impl Track {
    /// Build a track from an index selection. `cog_baseline_s` is the half-span
    /// in seconds the course is measured over.
    pub fn from_records(records: &[PingRecord], cog_baseline_s: f64) -> Track {
        let fixes = fix_track(records);
        let time: Vec<f64> = fixes.iter().map(|f| f.0).collect();
        let lat: Vec<f64> = fixes.iter().map(|f| f.1).collect();
        let lon: Vec<f64> = fixes.iter().map(|f| f.2).collect();
        Track::new(time, lat, lon, cog_baseline_s)
    }

    pub fn new(time: Vec<f64>, lat: Vec<f64>, lon: Vec<f64>, cog_baseline_s: f64) -> Track {
        let n = time.len();
        if n == 0 {
            return Track {
                time,
                lat,
                lon,
                cog_unwrapped: Vec::new(),
                dist: Vec::new(),
                sog: Vec::new(),
            };
        }
        let dtm = if n > 1 {
            let mut d: Vec<f64> = time.windows(2).map(|w| w[1] - w[0]).collect();
            d.sort_by(f64::total_cmp);
            d[d.len() / 2].max(1e-3)
        } else {
            1.0
        };
        let baseline = ((cog_baseline_s / dtm).round() as usize).max(2);
        let cog = course_over_ground(&lat, &lon, baseline);
        let cog_unwrapped = unwrap_deg(&cog);

        let lat0 = lat.iter().sum::<f64>() / n as f64;
        let (m_lat, m_lon) = geo::local_scale(lat0);
        let mut dist = Vec::with_capacity(n);
        dist.push(0.0);
        for i in 1..n {
            let dx = (lon[i] - lon[i - 1]) * m_lon;
            let dy = (lat[i] - lat[i - 1]) * m_lat;
            dist.push(dist[i - 1] + dx.hypot(dy));
        }
        // Speed over the same baseline the course uses, rather than between
        // adjacent fixes. One straggling epoch would otherwise divide two and a
        // half metres by a millisecond, and no amount of smoothing afterwards
        // brings a three-thousand-metres-per-second sample back to sanity.
        let sog: Vec<f64> = (0..n)
            .map(|i| {
                let a = i.saturating_sub(baseline);
                let b = (i + baseline).min(n - 1);
                let dt = time[b] - time[a];
                if dt <= 1e-6 {
                    0.0
                } else {
                    (dist[b] - dist[a]) / dt
                }
            })
            .collect();

        Track { time, lat, lon, cog_unwrapped, dist, sog }
    }

    pub fn len(&self) -> usize {
        self.time.len()
    }
    pub fn is_empty(&self) -> bool {
        self.time.is_empty()
    }

    /// Vessel position at an instant.
    pub fn position(&self, t: f64) -> (f64, f64) {
        (interp(&self.time, &self.lat, t), interp(&self.time, &self.lon, t))
    }

    /// Course over ground at an instant, degrees in [0, 360).
    pub fn course(&self, t: f64) -> f64 {
        interp(&self.time, &self.cog_unwrapped, t).rem_euclid(360.0)
    }

    pub fn speed(&self, t: f64) -> f64 {
        interp(&self.time, &self.sog, t)
    }

    /// Along-track distance at an instant.
    pub fn distance(&self, t: f64) -> f64 {
        interp(&self.time, &self.dist, t)
    }

    /// The position `back` metres behind `t` along the path actually sailed,
    /// rather than along a straight bearing. The two differ wherever the vessel
    /// was turning -- which, on these surveys, is most of the time.
    pub fn position_back(&self, t: f64, back: f64) -> (f64, f64) {
        let s = self.distance(t) - back;
        (interp(&self.dist, &self.lat, s), interp(&self.dist, &self.lon, s))
    }
}

/// How the fish is placed relative to the tow point.
///
/// For a fish `L` behind a vessel on an arc of radius `R`, these are three
/// different places, and in a turn they are far apart:
///
/// | model      | radius            |
/// |------------|-------------------|
/// | `Astern`   | `R`, ahead of the wake |
/// | `Wake`     | `R`               |
/// | `Tractrix` | `sqrt(R^2 - L^2)` |
///
/// A towed body on a taut cable does the last one. None of the three is known
/// to be right for this rig -- see the module comment -- so the viewer offers
/// all three and the difference between them is the honest uncertainty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LaybackModel {
    /// A constant offset astern along the smoothed course over ground.
    /// Cheap, steady, and the widest of the three in a turn.
    #[default]
    Astern,
    /// Where the tow point was `layback_m` of sailed distance ago. The fish
    /// follows exactly in the vessel's wake.
    Wake,
    /// A taut inextensible cable: the fish stays `layback_m` from the tow point
    /// and always moves straight towards it. This is the pursuit curve -- the
    /// classical tractrix when the tow point runs in a straight line.
    Tractrix,
}

/// Which bearing aims the swath.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bearing {
    /// Course over ground from the vessel track. Steady, but it is the boat's
    /// track, and differs from the fish's own axis by the crab angle.
    Cog,
    /// The fish's own compass. Absolute, not an integrated rate -- checked by
    /// its bias drifting only +0.17 deg/hour over a survey and by every
    /// accelerometer and gyro-rate slot in message 2020 reading zero. Noisy
    /// enough at ping rate that it must be smoothed before use.
    Compass,
}

/// How the fish is placed relative to the vessel.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct NavConfig {
    /// Antenna to tow point, metres, positive aft. The GPS is at the bow.
    pub gps_to_towpoint_m: f64,
    /// Horizontal offset of the fish astern of the tow point, metres.
    ///
    /// The default 44 m is a *ceiling*, not a best estimate: with 45 m of cable
    /// out and the fish 5-19 m down, a perfectly taut straight cable gives
    /// 43.9-44.7 m. Any sag at all makes the true layback smaller, and nothing
    /// in the recording says how much.
    pub layback_m: f64,
    /// How the fish is placed. See `LaybackModel`.
    ///
    /// Old project files carry `walk_track: bool` instead; serde ignores it and
    /// they read back as `Astern`, which is what `walk_track: false` meant.
    #[serde(default)]
    pub model: LaybackModel,
    pub bearing: Bearing,
    /// Seconds of smoothing on the compass before it aims a swath. The fish
    /// cannot yaw as fast as the raw series says; the sensor is what moves.
    pub compass_smooth_s: f64,
    /// Constant added to the swath bearing, for a mounting or deviation offset.
    pub heading_offset_deg: f64,
    /// Half-span in seconds the course over ground is measured over.
    pub cog_baseline_s: f64,
    /// Roll beyond this is flagged; about 21% of samples on these surveys.
    /// Whether roll actually corrupts the geometry is *not* established -- the
    /// port/starboard bottom ranges show no correlation with it (r = -0.023)
    /// and a cos(roll) correction made the fit worse. It is carried as a
    /// quality flag, not as a correction.
    pub roll_flag_deg: f64,
}

impl Default for NavConfig {
    fn default() -> NavConfig {
        NavConfig {
            gps_to_towpoint_m: 4.0,
            layback_m: 44.0,
            model: LaybackModel::Astern,
            bearing: Bearing::Cog,
            compass_smooth_s: 2.0,
            heading_offset_deg: 0.0,
            cog_baseline_s: 12.0,
            roll_flag_deg: 11.0,
        }
    }
}

/// Integrate the pursuit curve along a tow-point path.
///
/// The update is the taut-string one: having moved the tow point to `p`, put
/// the fish back at distance `L` from it, along the line from where the fish
/// just was. In the limit of small steps that is exactly the condition that
/// defines the curve -- the fish is always `L` away and always moving straight
/// at the tow point -- and it is unconditionally stable, which an explicit
/// integration of the differential form is not.
///
/// Steps are subdivided to `MAX_STEP_M` of tow-point travel, and that matters
/// more than it looks: the scheme is only *first* order. Measured against the
/// closed form for a steady turn, the settled radius comes out low by about
/// 0.24 m per metre of step at R = 90 m, and halving the step halves the
/// error rather than quartering it. At the 2.6 m a vessel covers between GNSS
/// fixes that would be a 60 cm bias, systematically inside the true curve.
/// A tenth of a metre puts it under 3.5 cm at R = 90 m and under 1 cm at
/// R = 300 m, for a few tens of thousands of iterations over a whole survey --
/// which is nothing, so there is no case for a cleverer integrator here.
fn integrate_pursuit(
    time: &[f64],
    x: &[f64],
    y: &[f64],
    l: f64,
    start_heading_rad: f64,
) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    const MAX_STEP_M: f64 = 0.1;
    let n = time.len();
    if n == 0 || !(l > 0.0) {
        return (time.to_vec(), x.to_vec(), y.to_vec());
    }
    // Start the fish straight astern. A pursuit curve forgets its initial
    // condition over a few multiples of L, so on a survey that begins with a
    // run-in this is settled long before the first line.
    let mut fx = x[0] - l * start_heading_rad.sin();
    let mut fy = y[0] - l * start_heading_rad.cos();

    let (mut ot, mut ox, mut oy) = (
        Vec::with_capacity(n * 4),
        Vec::with_capacity(n * 4),
        Vec::with_capacity(n * 4),
    );
    ot.push(time[0]);
    ox.push(fx);
    oy.push(fy);

    for i in 1..n {
        let (dx, dy) = (x[i] - x[i - 1], y[i] - y[i - 1]);
        let seg = dx.hypot(dy);
        let steps = ((seg / MAX_STEP_M).ceil() as usize).clamp(1, 4096);
        for k in 1..=steps {
            let f = k as f64 / steps as f64;
            let (px, py) = (x[i - 1] + dx * f, y[i - 1] + dy * f);
            let (ux, uy) = (px - fx, py - fy);
            let d = ux.hypot(uy);
            // A stationary vessel leaves the fish where it is; a cable cannot
            // push, only pull.
            if d > 1e-9 {
                fx = px - l * ux / d;
                fy = py - l * uy / d;
            }
            // one output sample per input fix, at the fix's own time
            if k == steps {
                ot.push(time[i]);
                ox.push(fx);
                oy.push(fy);
            }
        }
    }
    (ot, ox, oy)
}

/// Where one ping was fired from and which way it looked.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Fix {
    pub time: f64,
    /// Vessel antenna.
    pub boat_lat: f64,
    pub boat_lon: f64,
    /// Towfish.
    pub lat: f64,
    pub lon: f64,
    /// Bearing the fish's fore-and-aft axis points, degrees true.
    pub bearing: f64,
    /// Course over ground at this instant.
    pub cog: f64,
    pub speed: f64,
    pub roll: f64,
    pub pitch: f64,
    pub depth: f64,
    pub altitude: f64,
    /// False when the roll exceeded the flag threshold.
    pub clean: bool,
}

/// The navigation solution for one channel of one dataset.
pub struct Nav {
    pub track: Track,
    pub cfg: NavConfig,
    /// Ping times, ascending, and the smoothed compass at each.
    ping_time: Vec<f64>,
    compass_unwrapped: Vec<f64>,
    /// The pursuit curve, solved once over the whole track and then read by
    /// interpolation. It cannot be evaluated per ping: every position depends
    /// on the whole history before it.
    pursuit: Option<(Vec<f64>, Vec<f64>, Vec<f64>)>,
}

impl Nav {
    pub fn build(records: &[PingRecord], cfg: NavConfig) -> Nav {
        let track = Track::from_records(records, cfg.cog_baseline_s);

        // The compass at ping rate, smoothed over `compass_smooth_s`. Between
        // two pings 70 ms apart the raw heading moves 0.3 deg typically and up
        // to 5 deg, which at 30 m of ground range throws the swath edge metres
        // sideways: consecutive pings fan out instead of overlapping and the
        // mosaic fills with wedge-shaped holes.
        let mut rows: Vec<(f64, f64)> =
            records.iter().map(|r| (r.time, r.heading as f64)).collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        rows.dedup_by(|a, b| a.0 == b.0);
        let ping_time: Vec<f64> = rows.iter().map(|r| r.0).collect();
        let raw: Vec<f64> = rows.iter().map(|r| r.1).collect();
        let unwrapped = unwrap_deg(&raw);
        let width = if ping_time.len() > 2 {
            let dt = (ping_time[ping_time.len() - 1] - ping_time[0])
                / (ping_time.len() - 1) as f64;
            ((cfg.compass_smooth_s / dt.max(1e-3)).round() as usize).max(1)
        } else {
            1
        };
        let compass_unwrapped = smooth(&unwrapped, width);

        let pursuit = (cfg.model == LaybackModel::Tractrix && track.len() > 1).then(|| {
            // Integrate in a local tangent plane about the survey, then hand
            // back degrees. A few kilometres of track is well inside where that
            // is exact to the centimetre.
            let lat0 = track.lat.iter().sum::<f64>() / track.len() as f64;
            let lon0 = track.lon.iter().sum::<f64>() / track.len() as f64;
            let (m_lat, m_lon) = geo::local_scale(lat0);
            // The cable leaves the tow point, not the antenna.
            let (tx, ty): (Vec<f64>, Vec<f64>) = (0..track.len())
                .map(|i| {
                    let b = (track.cog_unwrapped[i] + 180.0).to_radians();
                    (
                        (track.lon[i] - lon0) * m_lon + cfg.gps_to_towpoint_m * b.sin(),
                        (track.lat[i] - lat0) * m_lat + cfg.gps_to_towpoint_m * b.cos(),
                    )
                })
                .unzip();
            let (t, fx, fy) = integrate_pursuit(
                &track.time,
                &tx,
                &ty,
                cfg.layback_m,
                track.cog_unwrapped[0].to_radians(),
            );
            (
                t,
                fy.iter().map(|v| lat0 + v / m_lat).collect(),
                fx.iter().map(|v| lon0 + v / m_lon).collect(),
            )
        });

        Nav { track, cfg, ping_time, compass_unwrapped, pursuit }
    }

    /// Smoothed compass heading at an instant, degrees in [0, 360).
    pub fn compass(&self, t: f64) -> f64 {
        if self.compass_unwrapped.is_empty() {
            return f64::NAN;
        }
        interp(&self.ping_time, &self.compass_unwrapped, t).rem_euclid(360.0)
    }

    /// Solve one ping.
    pub fn fix(&self, r: &PingRecord) -> Fix {
        let t = r.time;
        let (blat, blon) = self.track.position(t);
        let cog = self.track.course(t);
        let back = self.cfg.gps_to_towpoint_m + self.cfg.layback_m;
        let (lat, lon) = match self.cfg.model {
            // astern along the course made good
            LaybackModel::Astern => geo::offset_m(blat, blon, cog + 180.0, back),
            LaybackModel::Wake => self.track.position_back(t, back),
            LaybackModel::Tractrix => match &self.pursuit {
                Some((pt, plat, plon)) => (interp(pt, plat, t), interp(pt, plon, t)),
                None => geo::offset_m(blat, blon, cog + 180.0, back),
            },
        };
        let bearing = match self.cfg.bearing {
            Bearing::Cog => cog,
            Bearing::Compass => {
                let c = self.compass(t);
                if c.is_finite() {
                    c
                } else {
                    cog
                }
            }
        };
        Fix {
            time: t,
            boat_lat: blat,
            boat_lon: blon,
            lat,
            lon,
            bearing: (bearing + self.cfg.heading_offset_deg).rem_euclid(360.0),
            cog,
            speed: self.track.speed(t),
            roll: r.roll as f64,
            pitch: r.pitch as f64,
            depth: r.depth as f64,
            altitude: r.altitude as f64,
            clean: (r.roll as f64).abs() <= self.cfg.roll_flag_deg,
        }
    }

    /// Fix every record, in order.
    /// EXPERIMENT: rate of change of course over ground at `t`, deg/s,
    /// measured over a six-second baseline.
    pub fn turn_rate_at(&self, t: f64) -> f64 {
        let a = interp(&self.track.time, &self.track.cog_unwrapped, t - 3.0);
        let b = interp(&self.track.time, &self.track.cog_unwrapped, t + 3.0);
        if a.is_finite() && b.is_finite() { (b - a) / 6.0 } else { 0.0 }
    }

    pub fn fixes(&self, records: &[PingRecord]) -> Vec<Fix> {
        records.iter().map(|r| self.fix(r)).collect()
    }
}

/// A straight run line or the turn between two of them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Segment {
    pub kind: SegmentKind,
    pub t0: f64,
    pub t1: f64,
    pub index0: usize,
    pub index1: usize,
    /// Mean course over the segment, degrees.
    pub course: f64,
    pub length_m: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentKind {
    Line,
    Turn,
}

/// Split a track into run lines and turns, by rate of turn.
///
/// A line is a stretch where the course is changing slower than `turn_rate`
/// deg/s for at least `min_line_s`. Everything else is a turn.
/// How far apart the three layback models put the fish, in metres: the median
/// and the 95th percentile over the recording.
///
/// This is not an error bar -- nothing in the recording observes where the fish
/// actually was, so nothing here can measure how wrong we are. It is a *floor*
/// under one. Three defensible models of the same 44 m of cable, given the same
/// vessel track, disagree by this much; the truth is at best this uncertain and
/// probably worse, because all three share the assumption that the fish trails
/// the tow point and none of them observes the cable's bearing.
///
/// Reported rather than hidden because a mosaic without a stated uncertainty
/// reads as a map, and this one is a picture in roughly the right place.
pub fn model_spread_m(records: &[PingRecord], cfg: NavConfig) -> (f64, f64) {
    let navs: Vec<Nav> = [LaybackModel::Astern, LaybackModel::Wake, LaybackModel::Tractrix]
        .iter()
        .map(|&model| Nav::build(records, NavConfig { model, ..cfg }))
        .collect();
    // Every hundredth ping: the answer is a distribution, not a per-ping fact,
    // and the fixes only move at 1 Hz anyway.
    let mut d: Vec<f64> = Vec::new();
    for r in records.iter().step_by(101) {
        let f: Vec<Fix> = navs.iter().map(|n| n.fix(r)).collect();
        let mut worst = 0.0f64;
        for i in 0..f.len() {
            for j in i + 1..f.len() {
                if f[i].lat.is_finite() && f[j].lat.is_finite() {
                    worst = worst.max(crate::geo::distance_m(f[i].lat, f[i].lon, f[j].lat, f[j].lon));
                }
            }
        }
        if worst.is_finite() {
            d.push(worst);
        }
    }
    if d.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    d.sort_by(f64::total_cmp);
    (d[d.len() / 2], d[(d.len() * 95 / 100).min(d.len() - 1)])
}

pub fn detect_lines(track: &Track, turn_rate: f64, min_line_s: f64, smooth_s: f64) -> Vec<Segment> {
    let n = track.len();
    if n < 3 {
        return Vec::new();
    }
    let dt = ((track.time[n - 1] - track.time[0]) / (n - 1) as f64).max(1e-3);
    let w = ((smooth_s / dt).round() as usize).max(1);
    let cog = smooth(&track.cog_unwrapped, w);
    let mut rate = Vec::with_capacity(n);
    for i in 0..n {
        let a = i.saturating_sub(1);
        let b = (i + 1).min(n - 1);
        let d = (track.time[b] - track.time[a]).max(1e-6);
        rate.push(((cog[b] - cog[a]) / d).abs());
    }
    let rate = smooth(&rate, w);

    let straight: Vec<bool> = rate.iter().map(|&r| r < turn_rate).collect();
    let mut segs: Vec<Segment> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let s = straight[i];
        let mut j = i;
        while j + 1 < n && straight[j + 1] == s {
            j += 1;
        }
        let dur = track.time[j] - track.time[i];
        let kind = if s && dur >= min_line_s { SegmentKind::Line } else { SegmentKind::Turn };
        let course = {
            let a = cog[i];
            let b = cog[j];
            ((a + b) / 2.0).rem_euclid(360.0)
        };
        segs.push(Segment {
            kind,
            t0: track.time[i],
            t1: track.time[j],
            index0: i,
            index1: j,
            course,
            length_m: track.dist[j] - track.dist[i],
        });
        i = j + 1;
    }

    // merge adjacent turns left behind by short straight stretches
    let mut merged: Vec<Segment> = Vec::with_capacity(segs.len());
    for s in segs {
        match merged.last_mut() {
            Some(p) if p.kind == s.kind && p.kind == SegmentKind::Turn => {
                p.t1 = s.t1;
                p.index1 = s.index1;
                p.length_m += s.length_m;
            }
            _ => merged.push(s),
        }
    }
    merged
}
