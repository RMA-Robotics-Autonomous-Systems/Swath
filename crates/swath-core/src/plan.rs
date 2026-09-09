//! Planning a search: where to run the lines, and what that covers.
//!
//! The job is one decision -- which way to steer -- and a pile of arithmetic
//! that follows from it. The arithmetic is here, with no I/O and no notion of a
//! request, for the same reason `nav` has none: the command line, the server
//! and the tests all want the same answers.
//!
//! Three things in it are easy to get wrong, so they are stated once here.
//!
//! **The waypoints steer the antenna, not the fish.** The GPS is at the bow and
//! the fish is `gps_to_towpoint_m + layback_m` astern of it -- 54 m on the
//! standard rig. A line drawn from one edge of the box to the other therefore
//! puts the *boat* over the box and the fish short of it at both ends.
//!
//! The run-in and the run-out are the legs either side of the box, drawn and
//! steered exactly as asked for: 50 m of run-in is 50 m of line before the box
//! edge. What the layback does to them is not symmetrical, and this is the one
//! thing worth knowing about the whole arrangement:
//!
//! ```text
//!   the near edge is free      recording starts when the fish reaches it,
//!                              which is `offset` into the leg -- the boat
//!                              is already inside the box by then
//!   the far edge is not        the fish is `offset` behind, so the leg has
//!                              to carry on `offset` past the box or the
//!                              fish never gets there
//! ```
//!
//! So a run-in shorter than the offset costs settling, and a run-out shorter
//! than the offset costs *coverage*: the fish is `offset - run_out` short of
//! the far edge when the wheel goes over, and that strip of the box is not
//! surveyed by that line. The plan reports it as `fish_short_m` and draws it,
//! rather than quietly claiming the box.
//!
//! **The run-in is not padding.** A towed body needs about three cable lengths
//! to settle behind a turn, and until it has, the constant-offset placement in
//! `nav` is wrong by an amount that decays with distance sailed -- 5.4 m within
//! 25 m of a turn, 1.8 m by 100 m, 0.7 m past 400 m on these recordings. The
//! run-in buys that settling, which is why recording starts partway along the
//! line rather than at its start.
//!
//! **Spacing is a choice between three regimes, not an overlap percentage.**
//! With a usable half swath of `range - nadir` either side, three thresholds
//! fall out of the geometry and each one means something:
//!
//! ```text
//!   spacing <= 2*(range - nadir)   every patch of seabed seen once,
//!                                  and a hole under every line
//!   spacing <= range               neighbours cover each other's nadir gap,
//!                                  so nothing is unseen
//!   spacing <= range - nadir       every point seen twice, from opposite
//!                                  sides -- the search standard
//! ```
//!
//! The last reduces to the textbook `spacing = range` when the nadir gap is
//! zero, which is the check that the derivation is the usual one and not a new
//! one.

use serde::{Deserialize, Serialize};

use crate::geo;
use crate::gpx::{Gpx, Line, Point, Waypoint};
use crate::index::Bounds;

const KN: f64 = 0.514_444;

/// How close together the lines are, said as what it buys rather than as a
/// percentage of overlap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Regime {
    /// One pass over each patch of seabed. Fast, and it leaves the nadir open.
    Recon,
    /// Neighbours cover each other's nadir gap. Nothing on the seabed is unseen.
    Full,
    /// Every point twice, from opposite sides.
    #[default]
    Double,
    /// Straight from `spacing_m`.
    Custom,
}

/// Lines run one after the next, or every k-th with the gaps filled in after.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    /// 1, 2, 3, turning 180 degrees each time. What a boat actually does.
    #[default]
    Sequential,
    /// Enough room between consecutive runs for a clean semicircle, then back
    /// for the ones that were skipped.
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Mow the lawn: the heading swaps on every line.
    #[default]
    Alternate,
    /// Every line run the same way, so every line meets the sea the same way.
    /// It costs a full transit back after each one.
    OneWay,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Sonar {
    /// Ground range each side, metres.
    pub range_m: f64,
    /// Height the fish is to be flown at, metres.
    pub altitude_m: f64,
    /// The strip under the fish that is not imaged, as a multiple of altitude.
    pub nadir_factor: f64,
    pub regime: Regime,
    /// Used only when `regime` is `Custom`.
    pub spacing_m: f64,
}

impl Default for Sonar {
    fn default() -> Sonar {
        Sonar { range_m: 75.0, altitude_m: 8.0, nadir_factor: 1.0, regime: Regime::Double, spacing_m: 65.0 }
    }
}

impl Sonar {
    /// Half-width of the unimaged strip under the fish, metres.
    pub fn nadir_m(&self) -> f64 {
        (self.nadir_factor * self.altitude_m).min(self.range_m * 0.6).max(0.0)
    }
    /// The spacing the chosen regime asks for, before it is squared up to the box.
    pub fn spacing(&self) -> f64 {
        let (r, n) = (self.range_m, self.nadir_m());
        let s = match self.regime {
            Regime::Recon => 2.0 * (r - n),
            Regime::Full => r,
            Regime::Double => r - n,
            Regime::Custom => self.spacing_m,
        };
        s.max(5.0)
    }
}

/// Defaulted field by field, so a project written before a setting existed
/// still opens: a missing one is taken from [`Rig::default`] rather than
/// refusing the whole plan -- and with it the project it is stored in.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Rig {
    /// Fish astern of the tow point, metres. Defaults to `NavConfig`'s.
    pub layback_m: f64,
    /// Antenna to tow point, metres, positive aft. The GPS is at the bow.
    pub gps_to_towpoint_m: f64,
    /// The leg before the box, metres. Drawn and steered as asked: 50 m here
    /// is 50 m of line ahead of the near edge.
    ///
    /// It buys settling. A towed body needs about three cable lengths behind a
    /// turn before the constant-offset placement in `nav` is worth trusting,
    /// and the fish gets this plus the offset -- it is already `offset` behind
    /// when the boat crosses the edge, and recording does not start until it
    /// catches up to the edge itself.
    pub run_in_m: f64,
    /// The leg after the box, metres. Drawn and steered as asked, like the
    /// run-in.
    ///
    /// It buys coverage, which is why it has a floor the run-in does not: the
    /// fish is `offset_m` astern, so anything less than that and the wheel
    /// goes over before the fish has reached the far edge. The default is 60,
    /// which clears the 54 m of the standard rig; change the layback and the
    /// plan says whether this still covers it.
    pub run_out_m: f64,
    pub speed_kn: f64,
    pub turn_radius_m: f64,
    /// Seconds lost per turn beyond the distance -- slowing, settling, talking.
    pub turn_allowance_s: f64,
}

impl Default for Rig {
    fn default() -> Rig {
        Rig {
            layback_m: 44.0,
            gps_to_towpoint_m: 10.0,
            run_in_m: 150.0,
            run_out_m: 60.0,
            speed_kn: 3.5,
            turn_radius_m: 40.0,
            turn_allowance_s: 90.0,
        }
    }
}

impl Rig {
    /// Antenna to fish, metres. This is what the lines are stretched by.
    pub fn offset_m(&self) -> f64 {
        self.gps_to_towpoint_m + self.layback_m
    }
}

/// Everything a plan is solved from, except which way to steer.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PlanSpec {
    pub sonar: Sonar,
    pub rig: Rig,
    /// Room around the targets for the boat's own position uncertainty.
    pub margin_m: f64,
    pub order: Order,
    pub direction: Direction,
    /// Degrees between the plans generated across the quadrant.
    pub step_deg: f64,
}

impl Default for PlanSpec {
    fn default() -> PlanSpec {
        PlanSpec {
            sonar: Sonar::default(),
            rig: Rig::default(),
            margin_m: 80.0,
            order: Order::Sequential,
            direction: Direction::Alternate,
            step_deg: 10.0,
        }
    }
}

/// A position to be searched, and how far it might really be from there.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Target {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// The uncertainty circle the plan has to cover, metres.
    #[serde(default)]
    pub radius_m: f64,
}

/// A local tangent plane about the targets, east and north in metres.
///
/// The same series the viewer uses, so a line solved here and a line drawn in
/// the browser agree by construction rather than by both being nearly right.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub lat0: f64,
    pub lon0: f64,
    m_lat: f64,
    m_lon: f64,
}

impl Frame {
    pub fn about(lat0: f64, lon0: f64) -> Frame {
        let (m_lat, m_lon) = geo::local_scale(lat0);
        Frame { lat0, lon0, m_lat, m_lon }
    }
    pub fn fwd(&self, lat: f64, lon: f64) -> (f64, f64) {
        ((lon - self.lon0) * self.m_lon, (lat - self.lat0) * self.m_lat)
    }
    pub fn inv(&self, e: f64, n: f64) -> (f64, f64) {
        (self.lat0 + n / self.m_lat, self.lon0 + e / self.m_lon)
    }
}

/// One line as the boat runs it.
#[derive(Clone, Debug, Serialize)]
pub struct Run {
    /// Which line of the plan, numbered across the box from one edge.
    pub line: usize,
    /// Across-track position of the line, metres in the plan frame.
    pub x: f64,
    /// +1 along the plan azimuth, -1 against it.
    pub dir: f64,
    /// Along-track positions of the antenna: where the leg starts, where
    /// recording starts and stops, and where the leg ends and the turn begins.
    /// Recording stops when the fish reaches the far edge or when the leg ends,
    /// whichever comes first -- they are the same point when the run-out is
    /// exactly the offset.
    pub a_start: f64,
    pub a_on: f64,
    pub a_off: f64,
    pub a_end: f64,
    /// Heading the helm steers, degrees true.
    pub heading_deg: f64,
    /// Antenna distance for the whole line, metres.
    pub length_m: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Turn {
    /// The path in the plan frame, (along, across).
    pub points: Vec<[f64; 2]>,
    pub length_m: f64,
    /// True when it could not be a plain semicircle and had to loop out.
    pub teardrop: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Coverage {
    /// Fraction of the box seen no times, once or more, twice or more.
    pub none: f64,
    pub once: f64,
    pub twice: f64,
    /// The fewest passes over any point of the box.
    pub worst: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Two looks or more, from both sides, at least one at a useful range.
    Good,
    /// Seen, but not well enough to argue a classification from.
    Thin,
    /// Not seen at all.
    Missed,
}

#[derive(Clone, Debug, Serialize)]
pub struct TargetLook {
    pub id: String,
    pub name: String,
    /// Worst case over the whole uncertainty circle, not the centre.
    pub looks: usize,
    /// Of those, how many fall in the band where the shadow is worth reading.
    pub good: usize,
    /// Ensonified from opposite sides, so the shadow falls both ways.
    pub both_aspects: bool,
    pub verdict: Verdict,
    /// How far the nearest line passes from the centre, metres.
    pub nearest_m: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub azimuth_deg: f64,
    #[serde(skip)]
    pub frame: Frame,
    /// The box in the plan frame: across from `x0` to `x1`, along `a0` to `a1`.
    pub x0: f64,
    pub x1: f64,
    pub a0: f64,
    pub a1: f64,
    /// Across-track position of every line, in order across the box.
    pub lines_x: Vec<f64>,
    /// Lines that fall inside the box, and those added outside it to hold the
    /// coverage standard to the edge.
    pub lines: usize,
    pub outer_lines: usize,
    /// What the regime asked for, and what the box was actually divided into.
    pub spacing_requested_m: f64,
    pub spacing_m: f64,
    pub range_m: f64,
    pub nadir_m: f64,
    /// Lines are run every `skip`-th, then the gaps filled in.
    pub skip: usize,
    pub runs: Vec<Run>,
    pub turns: Vec<Turn>,
    pub coverage: Coverage,
    pub targets: Vec<TargetLook>,
    pub line_distance_m: f64,
    pub turn_distance_m: f64,
    pub distance_m: f64,
    pub seconds: f64,
    /// Turns that had to loop out, and how far past the ends of the lines the
    /// widest of them reaches.
    pub teardrops: usize,
    pub overshoot_m: f64,
    /// How far short of the far edge the fish is when the leg ends, metres.
    ///
    /// Zero when the run-out clears the layback, which is the only state worth
    /// sailing. Anything else is box that the line crosses and does not
    /// survey, at the far end of every line -- and since the direction
    /// alternates, at both ends of the box.
    pub fish_short_m: f64,
}

fn centroid(targets: &[Target]) -> (f64, f64) {
    let n = targets.len() as f64;
    (targets.iter().map(|t| t.lat).sum::<f64>() / n, targets.iter().map(|t| t.lon).sum::<f64>() / n)
}

/// Solve one plan. `None` when there is nothing to search for.
pub fn solve(spec: &PlanSpec, targets: &[Target], az_deg: f64) -> Option<Plan> {
    if targets.is_empty() {
        return None;
    }
    let az = az_deg.rem_euclid(360.0);
    let (lat0, lon0) = centroid(targets);
    let frame = Frame::about(lat0, lon0);
    let th = az * geo::D2R;
    let (su, cu) = (th.sin(), th.cos());
    let to_ax = |e: f64, n: f64| (e * su + n * cu, e * cu - n * su);

    // The box: every uncertainty circle, plus room for the boat's own.
    let mut x0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut a0 = f64::INFINITY;
    let mut a1 = f64::NEG_INFINITY;
    let mut pts = Vec::with_capacity(targets.len());
    for t in targets {
        let (e, n) = frame.fwd(t.lat, t.lon);
        let (a, x) = to_ax(e, n);
        let r = t.radius_m.max(0.0);
        x0 = x0.min(x - r);
        x1 = x1.max(x + r);
        a0 = a0.min(a - r);
        a1 = a1.max(a + r);
        pts.push((t, a, x));
    }
    let m = spec.margin_m.max(0.0);
    x0 -= m;
    x1 += m;
    a0 -= m;
    a1 += m;

    let range = spec.sonar.range_m;
    let nadir = spec.sonar.nadir_m();
    let want = spec.sonar.spacing();
    let width = x1 - x0;
    let xc = (x0 + x1) / 2.0;

    let inner = (((width / want).ceil() as usize) + 1).max(2);
    let span = width.max(want);
    let sp = span / (inner as f64 - 1.0);

    // A line array that stops at the box edge leaves that edge one look short:
    // the outermost line's own nadir gap sits on it and there is nothing
    // outside to fill it. Where the regime promises two looks, hold that
    // promise to the edge by running one line beyond each side.
    let twice = spec.sonar.regime == Regime::Double
        || (spec.sonar.regime == Regime::Custom && want <= range - nadir);
    let pad = usize::from(twice);
    let n = inner + 2 * pad;
    let lines_x: Vec<f64> =
        (0..n).map(|i| xc - span / 2.0 + (i as f64 - pad as f64) * sp).collect();

    let one_way = spec.direction == Direction::OneWay;
    let skip = if one_way || spec.order == Order::Sequential {
        1
    } else {
        ((2.0 * spec.rig.turn_radius_m / sp).ceil() as usize).max(1)
    };
    let mut order: Vec<usize> = Vec::with_capacity(n);
    for p in 0..skip {
        let mut i = p;
        while i < n {
            order.push(i);
            i += skip;
        }
    }

    let off = spec.rig.offset_m();
    let run_in = spec.rig.run_in_m.max(0.0);
    let run_out = spec.rig.run_out_m.max(0.0);
    // What the layback costs at the far edge. The leg ends `run_out` past the
    // box, and the fish is `off` behind the boat, so this much of the box goes
    // by after the wheel has gone over.
    let fish_short = (off - run_out).max(0.0);
    let runs: Vec<Run> = order
        .iter()
        .enumerate()
        .map(|(j, &li)| {
            let dir = if one_way || j % 2 == 0 { 1.0 } else { -1.0 };
            let (fs, fe) = if dir > 0.0 { (a0, a1) } else { (a1, a0) };
            Run {
                line: li,
                x: lines_x[li],
                dir,
                a_start: fs - dir * run_in,
                a_on: fs + dir * off,
                a_off: fe + dir * off.min(run_out),
                a_end: fe + dir * run_out,
                heading_deg: if dir > 0.0 { az } else { (az + 180.0).rem_euclid(360.0) },
                length_m: (a1 - a0) + run_in + run_out,
            }
        })
        .collect();

    // Turns, from the antenna pose that ends one run to the one that starts the
    // next. The run-in makes those two poses differ along track as well as
    // across it, which is why this is solved rather than assumed.
    let r_turn = spec.rig.turn_radius_m.max(1.0);
    let mut turns: Vec<Turn> = Vec::new();
    for w in runs.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let p0 = [a.a_end, a.x];
        let v0 = [a.dir, 0.0];
        let p1 = [b.a_start, b.x];
        let v1 = [b.dir, 0.0];
        if (a.dir - b.dir).abs() < 1e-9 {
            // One-way running: turn out, run back outside the box, turn in.
            let s = (b.x - a.x).signum();
            let px = a.x + 2.0 * r_turn * if s == 0.0 { 1.0 } else { s };
            let mid0 = [p0[0], px];
            let mid1 = [p1[0], px];
            let t1 = turn_path(p0, v0, mid0, [-a.dir, 0.0], r_turn);
            let t2 = turn_path(mid1, [-a.dir, 0.0], p1, v1, r_turn);
            let straight = (p0[0] - p1[0]).abs();
            let mut points = Vec::new();
            let mut length = straight;
            let mut teardrop = false;
            if let Some(t) = &t1 {
                points.extend(t.polyline());
                length += t.length;
                teardrop |= t.teardrop;
            }
            points.push(mid1);
            if let Some(t) = &t2 {
                points.extend(t.polyline());
                length += t.length;
                teardrop |= t.teardrop;
            }
            turns.push(Turn { points, length_m: length, teardrop });
        } else if let Some(t) = turn_path(p0, v0, p1, v1, r_turn) {
            turns.push(Turn { points: t.polyline(), length_m: t.length, teardrop: t.teardrop });
        }
    }

    let line_distance: f64 = runs.iter().map(|r| r.length_m).sum();
    let turn_distance: f64 = turns.iter().map(|t| t.length_m).sum();
    let distance = line_distance + turn_distance;
    let seconds = distance / (spec.rig.speed_kn.max(0.1) * KN)
        + turns.len() as f64 * spec.rig.turn_allowance_s;

    // How far past the ends of the lines the turns reach. The helmsman needs
    // that water, and the box has to be clear of it.
    let end_hi = runs.iter().map(|r| r.a_start.max(r.a_end)).fold(f64::NEG_INFINITY, f64::max);
    let end_lo = runs.iter().map(|r| r.a_start.min(r.a_end)).fold(f64::INFINITY, f64::min);
    let mut overshoot: f64 = 0.0;
    for t in &turns {
        for p in &t.points {
            overshoot = overshoot.max(p[0] - end_hi).max(end_lo - p[0]);
        }
    }
    let teardrops = turns.iter().filter(|t| t.teardrop).count();

    let mut plan = Plan {
        azimuth_deg: az,
        frame,
        x0,
        x1,
        a0,
        a1,
        lines_x,
        lines: n,
        outer_lines: 2 * pad,
        spacing_requested_m: want,
        spacing_m: sp,
        range_m: range,
        nadir_m: nadir,
        skip,
        runs,
        turns,
        coverage: Coverage::default(),
        targets: Vec::new(),
        line_distance_m: line_distance,
        turn_distance_m: turn_distance,
        distance_m: distance,
        seconds,
        teardrops,
        overshoot_m: overshoot.max(0.0),
        fish_short_m: fish_short,
    };
    analyse(&mut plan, &pts);
    Some(plan)
}

/// Passes over one across-track position.
fn passes(lines_x: &[f64], nadir: f64, range: f64, x: f64) -> usize {
    lines_x.iter().filter(|&&xl| { let d = (x - xl).abs(); d >= nadir && d <= range }).count()
}

/// Coverage is exact in one dimension: every line spans the same along-track
/// extent, so what happens across track happens everywhere in the box.
///
/// That holds while the fish crosses the whole box, which is what a run-out of
/// at least the offset buys. Short of that these figures describe the part of
/// the box every line does cross, and `fish_short_m` is the rest of the answer
/// -- reported and drawn separately rather than folded into a percentage that
/// would then mean two things at once.
fn analyse(plan: &mut Plan, pts: &[(&Target, f64, f64)]) {
    const N: usize = 1400;
    let dx = (plan.x1 - plan.x0) / (N as f64 - 1.0);
    let (mut none, mut once, mut twice) = (0usize, 0usize, 0usize);
    let mut worst = usize::MAX;
    for i in 0..N {
        let c = passes(&plan.lines_x, plan.nadir_m, plan.range_m, plan.x0 + i as f64 * dx);
        if c == 0 {
            none += 1;
        } else {
            once += 1;
            if c >= 2 {
                twice += 1;
            }
        }
        worst = worst.min(c);
    }
    let f = N as f64;
    plan.coverage = Coverage {
        none: none as f64 / f,
        once: once as f64 / f,
        twice: twice as f64 / f,
        worst: if worst == usize::MAX { 0 } else { worst },
    };

    // The usable window across the swath: too close in and the grazing angle is
    // steep and the shadow too short to argue from; the outermost tenth of the
    // range is where the return runs out of signal.
    let lo = plan.nadir_m.max(0.15 * plan.range_m);
    let hi = 0.90 * plan.range_m;
    plan.targets = pts
        .iter()
        .map(|(t, _a, x)| {
            let r = t.radius_m.max(0.0);
            let mut looks = usize::MAX;
            let mut good = usize::MAX;
            let mut both = true;
            const K: usize = 21;
            for i in 0..K {
                let px = x - r + 2.0 * r * i as f64 / (K as f64 - 1.0);
                let (mut l, mut g, mut pos, mut neg) = (0usize, 0usize, 0usize, 0usize);
                for &xl in &plan.lines_x {
                    let d = xl - px;
                    let ad = d.abs();
                    if ad >= plan.nadir_m && ad <= plan.range_m {
                        l += 1;
                        if d > 0.0 {
                            pos += 1;
                        } else {
                            neg += 1;
                        }
                        if ad >= lo && ad <= hi {
                            g += 1;
                        }
                    }
                }
                looks = looks.min(l);
                good = good.min(g);
                if pos == 0 || neg == 0 {
                    both = false;
                }
            }
            let nearest = plan
                .lines_x
                .iter()
                .map(|xl| (xl - x).abs())
                .fold(f64::INFINITY, f64::min);
            let verdict = if looks == 0 {
                Verdict::Missed
            } else if looks >= 2 && both && good >= 1 {
                Verdict::Good
            } else {
                Verdict::Thin
            };
            TargetLook {
                id: t.id.clone(),
                name: t.name.clone(),
                looks,
                good,
                both_aspects: both,
                verdict,
                nearest_m: nearest,
            }
        })
        .collect();
}

/// What a written plan carries besides the lines themselves.
#[derive(Clone, Debug)]
pub struct GpxOptions {
    /// Write routes at all.
    ///
    /// Off by default. Plenty of plotters take a route or a track from a file
    /// but not both, and for a helm steering by hand the track is the one worth
    /// having: it is the shape to follow, turns included. Turn this on for a
    /// plotter that wants route legs, or for an autopilot.
    pub routes: bool,
    /// A route per line, rather than one route through the whole plan.
    pub route_per_line: bool,
    /// Put a point at the apex of each turn, so an autopilot does not cut it.
    pub turn_points: bool,
    /// Include the targets as waypoints.
    pub targets: bool,
    /// Mark every line's start, its recording-on point and its end as
    /// standalone waypoints.
    ///
    /// Three per line, so a twenty-three line plan puts sixty-nine marks on the
    /// plotter. Useful when the screen is the only record of where recording
    /// starts; clutter when the trace already shows the shape.
    pub line_waypoints: bool,
    /// Draw the whole path -- lines and the curves between them -- as a track.
    ///
    /// GPX has no arc: the only geometry it can express is a polyline, so a
    /// curve is points and nothing else. Which container they go in is the
    /// whole question. A route point is a steering instruction -- the plotter
    /// makes it a waypoint, sequences it, and sounds an arrival alarm at it --
    /// so a route thick enough to draw a smooth turn is unusable to steer by,
    /// and on many plotters overruns the route point limit as well. A track is
    /// just a drawn line. It carries no waypoints, raises no alarms and takes
    /// as many points as the curve needs.
    ///
    /// A helm following the picture wants this, and it is the default.
    pub track: bool,
    pub name: String,
}

impl Default for GpxOptions {
    fn default() -> GpxOptions {
        GpxOptions {
            routes: false,
            route_per_line: false,
            turn_points: false,
            targets: true,
            line_waypoints: false,
            track: true,
            name: String::new(),
        }
    }
}

/// How far the drawn turn may sit from the true arc, metres.
///
/// The arcs are generated at about seven degrees a step for the chart, which is
/// some twenty-six points per turn and far finer than anything can be steered
/// to. Thinned to this, a 40 m turn keeps about eight and is still smooth at any
/// zoom a plotter draws it at.
const TRACK_TOLERANCE_M: f64 = 1.0;

/// Douglas-Peucker: drop the points that were only ever there for the canvas.
///
/// The plan frame is metres, so the tolerance is metres, and no projection has
/// to be undone to decide what to keep.
fn simplify(pts: &[[f64; 2]], tol: f64) -> Vec<[f64; 2]> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let (first, last) = (pts[0], pts[pts.len() - 1]);
    let (dx, dy) = (last[0] - first[0], last[1] - first[1]);
    let seg = dx.hypot(dy);
    let mut worst = 0.0;
    let mut at = 0usize;
    for (i, p) in pts.iter().enumerate().take(pts.len() - 1).skip(1) {
        // Distance to the chord, or to the shared endpoint when it degenerates.
        let d = if seg < 1e-9 {
            (p[0] - first[0]).hypot(p[1] - first[1])
        } else {
            ((last[0] - first[0]) * (first[1] - p[1]) - (first[0] - p[0]) * (last[1] - first[1]))
                .abs()
                / seg
        };
        if d > worst {
            worst = d;
            at = i;
        }
    }
    if worst <= tol {
        return vec![first, last];
    }
    let mut out = simplify(&pts[..=at], tol);
    out.pop();
    out.extend(simplify(&pts[at..], tol));
    out
}

impl Plan {
    /// Plan frame back to the world.
    pub fn ll(&self, a: f64, x: f64) -> (f64, f64) {
        let th = self.azimuth_deg * geo::D2R;
        let (su, cu) = (th.sin(), th.cos());
        self.frame.inv(a * su + x * cu, a * cu - x * su)
    }

    /// Everything the lines and their swaths touch.
    pub fn bounds(&self) -> Bounds {
        let mut b = Bounds::EMPTY;
        let pad = self.range_m;
        for (a, x) in [
            (self.a0 - pad, self.x0 - pad),
            (self.a0 - pad, self.x1 + pad),
            (self.a1 + pad, self.x0 - pad),
            (self.a1 + pad, self.x1 + pad),
        ] {
            let (lat, lon) = self.ll(a, x);
            b.extend(lat, lon);
        }
        b
    }

    /// The settings this plan was solved from, in one line.
    ///
    /// Written into the GPX metadata so a file found on a laptop six months
    /// later says what it came from, the same way a mosaic names the settings
    /// that painted it.
    pub fn digest(&self, spec: &PlanSpec) -> String {
        let regime = match spec.sonar.regime {
            Regime::Recon => "reconnaissance",
            Regime::Full => "nadir filled",
            Regime::Double => "double coverage",
            Regime::Custom => "set by hand",
        };
        [
            format!("azimuth {:03.0}\u{b0}T", self.azimuth_deg),
            format!("range {:.0} m/side", self.range_m),
            format!("altitude {:.1} m", spec.sonar.altitude_m),
            format!("nadir gap {:.0} m", self.nadir_m),
            format!("spacing {:.0} m ({regime})", self.spacing_m),
            format!("layback {:.0} m", spec.rig.layback_m),
            format!("GPS to tow point {:.0} m", spec.rig.gps_to_towpoint_m),
            format!("run-in {:.0} m", spec.rig.run_in_m),
            format!("run-out {:.0} m", spec.rig.run_out_m),
            format!("{:.1} kn", spec.rig.speed_kn),
            format!("turn radius {:.0} m", spec.rig.turn_radius_m),
            format!("{} lines", self.lines),
            format!("{:.2} km", self.distance_m / 1000.0),
            hms(self.seconds),
            if spec.direction == Direction::OneWay {
                "one-way running".to_string()
            } else {
                "alternating".to_string()
            },
            if self.skip > 1 {
                format!("run every {}th line, then fill", self.skip)
            } else {
                "run in sequence".to_string()
            },
        ]
        .join(" \u{b7} ")
    }

    /// The plan as a GPX route list.
    ///
    /// Every position in here is the **GPS antenna**, not the fish. Each line
    /// carries three points: `S` where the boat settles onto the heading, `A`
    /// where the fish has reached the box and recording starts, and `E` where
    /// the fish leaves it.
    pub fn to_gpx(&self, spec: &PlanSpec, targets: &[Target], opts: &GpxOptions) -> Gpx {
        let mut g = Gpx {
            name: if opts.name.is_empty() { self.default_name() } else { opts.name.clone() },
            desc: format!(
                "{}. Waypoints steer the GPS antenna; the fish is {:.0} m astern. Each line is stretched by that offset so the fish, not the boat, crosses the box, and by {:.0} m of run-in before the near edge and {:.0} m of run-out past the far one on top of it. Recording starts at the A point of each line and stops at the E point. No data is claimed in a turn.",
                self.digest(spec),
                spec.rig.offset_m(),
                spec.rig.run_in_m,
                spec.rig.run_out_m
            ),
            ..Gpx::default()
        };

        if opts.targets {
            for t in targets {
                let (lat, lon) = (t.lat, t.lon);
                g.waypoints.push(Waypoint {
                    name: t.name.clone(),
                    desc: format!("target, uncertainty {:.0} m", t.radius_m),
                    lat,
                    lon,
                    ele: None,
                });
                g.bounds.extend(lat, lon);
            }
        }

        // Three points per run, in the order the boat meets them.
        let mut legs: Vec<(Point, Point)> = Vec::with_capacity(self.runs.len());
        for r in &self.runs {
            let id = format!("L{:02}", r.line + 1);
            let (s_lat, s_lon) = self.ll(r.a_start, r.x);
            let (o_lat, o_lon) = self.ll(r.a_on, r.x);
            let (f_lat, f_lon) = self.ll(r.a_off, r.x);
            let (e_lat, e_lon) = self.ll(r.a_end, r.x);
            // Where recording stops and where the wheel goes over are two
            // places when the run-out is longer than the layback, and telling a
            // helm to turn at the point the data stops is what the setting
            // exists to prevent. Close enough together and they are one pin:
            // two marks a boat length apart are clutter on a plotter, not
            // information.
            const APART_M: f64 = 20.0;
            let parted = (r.a_end - r.a_off).abs() > APART_M;
            let mk = |name: String, desc: String, lat: f64, lon: f64| Point {
                lat,
                lon,
                name,
                desc,
                ..Point::default()
            };
            let start = mk(
                format!("{id}S"),
                format!("line {} start of run-in, steer {:03.0}°T", r.line + 1, r.heading_deg),
                s_lat,
                s_lon,
            );
            let on = mk(
                format!("{id}A"),
                format!("line {} recording on", r.line + 1),
                o_lat,
                o_lon,
            );
            let stop = mk(
                format!("{id}E"),
                format!("line {} recording off", r.line + 1),
                f_lat,
                f_lon,
            );
            let end = if parted {
                mk(format!("{id}T"), format!("line {} turn away", r.line + 1), e_lat, e_lon)
            } else {
                mk(
                    format!("{id}E"),
                    format!("line {} recording off, turn away", r.line + 1),
                    e_lat,
                    e_lon,
                )
            };
            if opts.line_waypoints {
                let mut marks = vec![&start, &on];
                if parted {
                    marks.push(&stop);
                }
                marks.push(&end);
                for w in marks {
                    g.waypoints.push(Waypoint {
                        name: w.name.clone(),
                        desc: w.desc.clone(),
                        lat: w.lat,
                        lon: w.lon,
                        ele: None,
                    });
                    g.bounds.extend(w.lat, w.lon);
                }
            }
            legs.push((start, end));
        }

        if opts.routes && opts.route_per_line {
            for (i, (s, e)) in legs.iter().enumerate() {
                g.routes.push(Line {
                    name: format!("L{:02}", self.runs[i].line + 1),
                    points: vec![s.clone(), e.clone()],
                });
            }
        } else if opts.routes {
            let mut pts: Vec<Point> = Vec::with_capacity(legs.len() * 2 + self.turns.len());
            for (i, (s, e)) in legs.iter().enumerate() {
                pts.push(s.clone());
                pts.push(e.clone());
                if opts.turn_points {
                    if let Some(t) = self.turns.get(i) {
                        if let Some(m) = t.points.get(t.points.len() / 2) {
                            let (lat, lon) = self.ll(m[0], m[1]);
                            pts.push(Point {
                                lat,
                                lon,
                                name: format!("T{:02}", i + 1),
                                desc: "turn apex".into(),
                                ..Point::default()
                            });
                        }
                    }
                }
            }
            g.routes.push(Line { name: g.name.clone(), points: pts });
        }

        // The boat's whole path, turns and all, as one drawn line. This is the
        // part a helm steering by eye actually follows; the routes above are
        // what the plotter gives cross-track error against.
        if opts.track {
            let mut path: Vec<Point> = Vec::new();
            let mut push = |a: f64, x: f64| {
                let (lat, lon) = self.ll(a, x);
                if path.last().is_some_and(|p: &Point| {
                    (p.lat - lat).abs() < 1e-9 && (p.lon - lon).abs() < 1e-9
                }) {
                    return;
                }
                path.push(Point { lat, lon, ..Point::default() });
            };
            for (i, r) in self.runs.iter().enumerate() {
                push(r.a_start, r.x);
                push(r.a_end, r.x);
                if let Some(t) = self.turns.get(i) {
                    // The turn already begins and ends on those two points, so
                    // only what curves between them is new.
                    let thin = simplify(&t.points, TRACK_TOLERANCE_M);
                    for p in thin.iter().skip(1).take(thin.len().saturating_sub(2)) {
                        push(p[0], p[1]);
                    }
                }
            }
            for p in &path {
                g.bounds.extend(p.lat, p.lon);
            }
            g.tracks.push(Line { name: format!("{} path", g.name), points: path });
        }
        g
    }

    pub fn default_name(&self) -> String {
        format!("SEARCH {:03.0}T", self.azimuth_deg)
    }

    /// The plan as GeoJSON, so the viewer draws it with the code that already
    /// draws every other vector layer.
    pub fn to_geojson(&self) -> serde_json::Value {
        let mut features: Vec<serde_json::Value> = Vec::new();
        let ring = |pts: &[(f64, f64)]| -> serde_json::Value {
            let mut c: Vec<serde_json::Value> =
                pts.iter().map(|(a, x)| { let (lat, lon) = self.ll(*a, *x); serde_json::json!([lon, lat]) }).collect();
            if let Some(first) = c.first().cloned() {
                c.push(first);
            }
            serde_json::json!({ "type": "Polygon", "coordinates": [c] })
        };
        // Coverage is claimed for the box and for nothing else. The fish is
        // only inside it between `a0` and `a1`, so a band drawn past them -- as
        // this one used to be, by a whole range at each end -- paints coverage
        // over water the plan makes no promise about. Clipped to the box, the
        // blue on the chart and the percentage in the report are one statement.
        let band = |xa: f64, xb: f64| -> Option<serde_json::Value> {
            let (xa, xb) = (xa.max(self.x0), xb.min(self.x1));
            if xb - xa < 0.01 {
                return None;
            }
            Some(ring(&[(self.a0, xa), (self.a0, xb), (self.a1, xb), (self.a1, xa)]))
        };
        let strip = |a: f64, x: f64, b: f64, y: f64| -> serde_json::Value {
            let (la, lo) = self.ll(a, x);
            let (lb, lob) = self.ll(b, y);
            serde_json::json!({ "type": "LineString", "coordinates": [[lo, la], [lob, lb]] })
        };

        features.push(serde_json::json!({
            "type": "Feature",
            "geometry": ring(&[(self.a0, self.x0), (self.a0, self.x1), (self.a1, self.x1), (self.a1, self.x0)]),
            "properties": { "kind": "box" }
        }));
        for &xl in &self.lines_x {
            for (a, b) in [(xl + self.nadir_m, xl + self.range_m), (xl - self.range_m, xl - self.nadir_m)] {
                if let Some(g) = band(a, b) {
                    features.push(serde_json::json!({
                        "type": "Feature", "geometry": g, "properties": { "kind": "swath" }
                    }));
                }
            }
        }
        // Where nothing reaches, drawn as itself rather than left as absence.
        if self.coverage.worst == 0 {
            const N: usize = 500;
            let dx = (self.x1 - self.x0) / (N as f64 - 1.0);
            let mut run: Option<f64> = None;
            for i in 0..=N {
                let x = self.x0 + i as f64 * dx;
                let open = i < N && passes(&self.lines_x, self.nadir_m, self.range_m, x) == 0;
                match (open, run) {
                    (true, None) => run = Some(x),
                    (false, Some(from)) => {
                        if let Some(g) = band(from, x) {
                            features.push(serde_json::json!({
                                "type": "Feature", "geometry": g, "properties": { "kind": "gap" }
                            }));
                        }
                        run = None;
                    }
                    _ => {}
                }
            }
        }
        // A leg is cut at the box edge, not at the recording points.
        //
        // The chart is a statement about water, not about time: every metre of
        // the box on this line gets covered by this run -- the fish reaches it
        // a layback after the boat does -- and every metre outside it is
        // manoeuvring. Cutting the leg where recording starts instead put an
        // orange run-in dash a layback deep into the box and left the far end
        // of every line hanging a layback outside it, which reads as a plan
        // that misses the edges it actually covers.
        for r in &self.runs {
            let (enter, leave) = if r.dir > 0.0 { (self.a0, self.a1) } else { (self.a1, self.a0) };
            let ahead = |from: f64, to: f64| (to - from) * r.dir > 0.5;
            let name = format!("L{:02}", r.line + 1);
            if ahead(r.a_start, enter) {
                features.push(serde_json::json!({
                    "type": "Feature",
                    "geometry": strip(r.a_start, r.x, enter, r.x),
                    "properties": { "kind": "runin", "name": name }
                }));
            }
            // Where the fish is when the wheel goes over. With enough run-out
            // that is the far edge and the line covers the box; with less, the
            // last stretch is crossed and not surveyed, and it is drawn as
            // what it is rather than left looking like line.
            let covered_to = leave - r.dir * self.fish_short_m;
            let short_from = if ahead(enter, covered_to) {
                features.push(serde_json::json!({
                    "type": "Feature",
                    "geometry": strip(enter, r.x, covered_to, r.x),
                    "properties": { "kind": "line", "name": name, "heading": r.heading_deg }
                }));
                covered_to
            } else {
                // The shortfall is longer than the box: nothing on this line is
                // surveyed at all.
                enter
            };
            if ahead(short_from, leave) {
                features.push(serde_json::json!({
                    "type": "Feature",
                    "geometry": strip(short_from, r.x, leave, r.x),
                    "properties": { "kind": "short", "name": name }
                }));
            }
            if ahead(leave, r.a_end) {
                features.push(serde_json::json!({
                    "type": "Feature",
                    "geometry": strip(leave, r.x, r.a_end, r.x),
                    "properties": { "kind": "runout", "name": name }
                }));
            }
        }
        for t in &self.turns {
            let c: Vec<serde_json::Value> = t
                .points
                .iter()
                .map(|p| { let (lat, lon) = self.ll(p[0], p[1]); serde_json::json!([lon, lat]) })
                .collect();
            features.push(serde_json::json!({
                "type": "Feature",
                "geometry": { "type": "LineString", "coordinates": c },
                "properties": { "kind": "turn", "teardrop": t.teardrop }
            }));
        }
        serde_json::json!({ "type": "FeatureCollection", "features": features })
    }
}

/// One row of the quadrant: enough to choose by, without the geometry.
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub azimuth_deg: f64,
    pub lines: usize,
    pub spacing_m: f64,
    pub distance_m: f64,
    pub seconds: f64,
    pub coverage_none: f64,
    pub coverage_twice: f64,
    pub worst: Verdict,
    pub teardrops: usize,
}

impl Summary {
    pub fn of(p: &Plan) -> Summary {
        let worst = p.targets.iter().fold(Verdict::Good, |w, t| {
            let rank = |v: Verdict| match v {
                Verdict::Good => 2,
                Verdict::Thin => 1,
                Verdict::Missed => 0,
            };
            if rank(t.verdict) < rank(w) { t.verdict } else { w }
        });
        Summary {
            azimuth_deg: p.azimuth_deg,
            lines: p.lines,
            spacing_m: p.spacing_m,
            distance_m: p.distance_m,
            seconds: p.seconds,
            coverage_none: p.coverage.none,
            coverage_twice: p.coverage.twice,
            worst,
            teardrops: p.teardrops,
        }
    }
}

/// Every plan the box has.
///
/// Only 0 to 90 is generated, and that is not a shortcut: a box run at 100
/// degrees is the same set of lines as one run at 10, turned, and beyond 180 it
/// repeats outright. The quadrant is the whole answer.
pub fn quadrant(spec: &PlanSpec, targets: &[Target]) -> Vec<Summary> {
    let step = spec.step_deg.clamp(1.0, 45.0);
    let mut out = Vec::new();
    let mut az = 0.0;
    while az <= 90.0 + 1e-9 {
        if let Some(p) = solve(spec, targets, az) {
            out.push(Summary::of(&p));
        }
        az += step;
    }
    out
}

/// Hours and minutes, for a line of prose.
pub fn hms(secs: f64) -> String {
    let m = (secs / 60.0).round() as i64;
    if m >= 60 { format!("{} h {:02} min", m / 60, m % 60) } else { format!("{m} min") }
}

/* ---------------- turn geometry ----------------
   Two antiparallel poses with an offset both across and along track. Six words
   are tried -- curve-straight-curve and curve-curve-curve, each turning either
   way -- and the shortest wins, so a turn too tight for a semicircle comes out
   as a teardrop instead of as an impossible straight line. */

fn rot90(v: [f64; 2], s: f64) -> [f64; 2] {
    if s > 0.0 { [-v[1], v[0]] } else { [v[1], -v[0]] }
}
fn sub(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] - b[0], a[1] - b[1]]
}
fn add(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] + b[0], a[1] + b[1]]
}
fn mul(a: [f64; 2], k: f64) -> [f64; 2] {
    [a[0] * k, a[1] * k]
}
fn len2(a: [f64; 2]) -> f64 {
    a[0].hypot(a[1])
}
fn arc_ang(c: [f64; 2], a: [f64; 2], b: [f64; 2], s: f64) -> f64 {
    let ta = (a[1] - c[1]).atan2(a[0] - c[0]);
    let tb = (b[1] - c[1]).atan2(b[0] - c[0]);
    if s > 0.0 { (tb - ta).rem_euclid(std::f64::consts::TAU) } else { (ta - tb).rem_euclid(std::f64::consts::TAU) }
}
fn arc_pts(c: [f64; 2], from: [f64; 2], ang: f64, s: f64, r: f64, out: &mut Vec<[f64; 2]>) {
    let a0 = (from[1] - c[1]).atan2(from[0] - c[0]);
    let steps = ((ang / 0.12).ceil() as usize).max(2);
    for i in 1..=steps {
        let t = a0 + s * ang * i as f64 / steps as f64;
        out.push([c[0] + r * t.cos(), c[1] + r * t.sin()]);
    }
}

struct Solved {
    length: f64,
    teardrop: bool,
    s: f64,
    r: f64,
    p0: [f64; 2],
    p1: [f64; 2],
    c0: [f64; 2],
    c1: [f64; 2],
    cm: Option<[f64; 2]>,
    t0: [f64; 2],
    t1: [f64; 2],
    a1: f64,
    am: f64,
    a2: f64,
}

impl Solved {
    fn polyline(&self) -> Vec<[f64; 2]> {
        let mut pts = vec![self.p0];
        arc_pts(self.c0, self.p0, self.a1, self.s, self.r, &mut pts);
        match self.cm {
            None => pts.push(self.t1),
            Some(cm) => arc_pts(cm, self.t0, self.am, -self.s, self.r, &mut pts),
        }
        arc_pts(self.c1, self.t1, self.a2, self.s, self.r, &mut pts);
        pts.push(self.p1);
        pts
    }
}

fn turn_path(p0: [f64; 2], v0: [f64; 2], p1: [f64; 2], v1: [f64; 2], r: f64) -> Option<Solved> {
    let mut best: Option<Solved> = None;
    let mut take = |c: Solved| {
        if best.as_ref().is_none_or(|b| c.length < b.length) {
            best = Some(c);
        }
    };
    for s in [1.0f64, -1.0] {
        let c0 = add(p0, mul(rot90(v0, s), r));
        let c1 = add(p1, mul(rot90(v1, s), r));
        let d = sub(c1, c0);
        let dl = len2(d);
        if dl > 1e-6 {
            let dh = mul(d, 1.0 / dl);
            let t0 = sub(c0, mul(rot90(dh, s), r));
            let t1 = sub(c1, mul(rot90(dh, s), r));
            let a1 = arc_ang(c0, p0, t0, s);
            let a2 = arc_ang(c1, t1, p1, s);
            take(Solved {
                length: r * (a1 + a2) + dl,
                teardrop: false,
                s, r, p0, p1, c0, c1, cm: None, t0, t1, a1, am: 0.0, a2,
            });
        }
        if dl <= 4.0 * r {
            let mid = mul(add(c0, c1), 0.5);
            let h = (4.0 * r * r - dl * dl / 4.0).max(0.0).sqrt();
            let perp = if dl > 1e-6 { mul(rot90(mul(d, 1.0 / dl), 1.0), h) } else { [h, 0.0] };
            for t in [1.0f64, -1.0] {
                let cm = add(mid, mul(perp, t));
                let t0 = mul(add(c0, cm), 0.5);
                let t1 = mul(add(cm, c1), 0.5);
                let a1 = arc_ang(c0, p0, t0, s);
                let am = arc_ang(cm, t0, t1, -s);
                let a2 = arc_ang(c1, t1, p1, s);
                take(Solved {
                    length: r * (a1 + am + a2),
                    teardrop: true,
                    s, r, p0, p1, c0, c1, cm: Some(cm), t0, t1, a1, am, a2,
                });
            }
        }
    }
    best
}
