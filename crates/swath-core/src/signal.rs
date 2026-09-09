//! Trace processing: bottom tracking, slant-range correction, and the
//! radiometric flattening that makes a waterfall readable.
//!
//! Ported from `tools/process.py` and the `tvg`/`stretch` helpers in
//! `tools/waterfall.py`.

/// Centred moving average along a trace, edges held.
pub fn smooth_trace(t: &[f32], k: usize) -> Vec<f32> {
    let k = k.max(1);
    if k <= 1 {
        return t.to_vec();
    }
    let n = t.len();
    let lo = k / 2;
    let hi = k - 1 - lo;
    let mut out = vec![0.0f32; n];
    for i in 0..n {
        let mut s = 0.0f64;
        for j in 0..k {
            // edge padding: clamp into range
            let idx = (i + j) as isize - lo as isize;
            let idx = idx.clamp(0, n as isize - 1) as usize;
            s += t[idx] as f64;
        }
        let _ = hi;
        out[i] = (s / k as f64) as f32;
    }
    out
}

/// Percentile of a slice, linear interpolation between order statistics --
/// the same convention as `numpy.percentile`.
pub fn percentile(v: &[f32], p: f64) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s: Vec<f32> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return 0.0;
    }
    s.sort_by(f32::total_cmp);
    let idx = (p / 100.0) * (s.len() - 1) as f64;
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    if lo == hi {
        return s[lo];
    }
    let w = (idx - lo as f64) as f32;
    s[lo] * (1.0 - w) + s[hi] * w
}

pub fn median(v: &[f32]) -> f32 {
    percentile(v, 50.0)
}

pub fn median_f64(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return f64::NAN;
    }
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// First-return sample index for one trace.
///
/// Picks the first sample where the smoothed envelope crosses `frac` of that
/// ping's strong-return level.
pub fn pick_bottom_trace(trace: &[f32], min_sample: usize, frac: f32, smooth: usize) -> f32 {
    if trace.len() <= min_sample {
        return min_sample as f32;
    }
    let t = smooth_trace(trace, smooth);
    let level = percentile(&t[min_sample..], 99.0) * frac;
    for (i, &v) in t.iter().enumerate().skip(min_sample) {
        if v >= level {
            return i as f32;
        }
    }
    min_sample as f32
}

/// Median filter a bottom pick along-track, so a few bad pings cannot pull the
/// surface around.
pub fn median_filter(v: &[f32], k: usize) -> Vec<f32> {
    let k = k.max(1) | 1;
    if k == 1 || v.len() < 2 {
        return v.to_vec();
    }
    let h = k / 2;
    let n = v.len();
    let mut buf = vec![0.0f32; k];
    (0..n)
        .map(|i| {
            for j in 0..k {
                let idx = (i + j) as isize - h as isize;
                buf[j] = v[idx.clamp(0, n as isize - 1) as usize];
            }
            buf.sort_by(f32::total_cmp);
            buf[h]
        })
        .collect()
}

/// Bottom pick for a block of traces, median-filtered along-track.
pub fn pick_bottom(
    traces: &[Vec<f32>],
    min_sample: usize,
    frac: f32,
    smooth: usize,
    medfilt: usize,
) -> Vec<f32> {
    let raw: Vec<f32> =
        traces.iter().map(|t| pick_bottom_trace(t, min_sample, frac, smooth)).collect();
    median_filter(&raw, medfilt)
}

/// Sample `trace` at fractional index `x`, linear, zero outside.
#[inline]
pub fn sample_at(trace: &[f32], x: f32) -> f32 {
    if !(x >= 0.0) || x > (trace.len() as f32 - 1.0) {
        return 0.0;
    }
    let i = x.floor() as usize;
    if i + 1 >= trace.len() {
        return trace[trace.len() - 1];
    }
    let w = x - i as f32;
    trace[i] * (1.0 - w) + trace[i + 1] * w
}

/// Remove the water column and remap slant range to ground range.
///
/// Altitude is the bottom pick in samples, so the mapping is
/// `ground = sqrt(slant^2 - altitude^2)` evaluated on a uniform ground grid.
pub fn slant_to_ground(trace: &[f32], altitude_samples: f32, out_samples: usize) -> Vec<f32> {
    let n = trace.len();
    if n < 2 || out_samples == 0 {
        return vec![0.0; out_samples];
    }
    let alt = altitude_samples.max(1.0);
    let max_ground = (((n - 1) as f32).powi(2) - alt * alt).max(1.0).sqrt();
    (0..out_samples)
        .map(|i| {
            let g = max_ground * i as f32 / (out_samples - 1).max(1) as f32;
            sample_at(trace, (g * g + alt * alt).sqrt())
        })
        .collect()
}

/// Water temperature implied by a measured sound speed, degrees C.
///
/// Absorption needs a temperature and nobody logged one, but the sound speed
/// was measured and is a good thermometer: at fixed salinity and depth
/// Mackenzie's equation is monotone in temperature, so it inverts. A degree of
/// error here moves the absorption coefficient by about a percent, which is far
/// inside everything else in this calculation.
pub fn temperature_from_sound_speed(c: f64, salinity_psu: f64, depth_m: f64) -> f64 {
    let speed = |t: f64| -> f64 {
        let s = salinity_psu;
        1448.96 + 4.591 * t - 5.304e-2 * t * t + 2.374e-4 * t.powi(3)
            + 1.340 * (s - 35.0) + 1.630e-2 * depth_m + 1.675e-7 * depth_m * depth_m
            - 1.025e-2 * t * (s - 35.0) - 7.139e-13 * t * depth_m.powi(3)
    };
    let (mut lo, mut hi) = (-2.0f64, 35.0f64);
    for _ in 0..48 {
        let mid = 0.5 * (lo + hi);
        if speed(mid) < c { lo = mid } else { hi = mid }
    }
    0.5 * (lo + hi)
}

/// Absorption of sound in seawater, dB per kilometre, one way.
///
/// Francois & Garrison (1982). At the frequencies this instrument works at the
/// magnesium-sulphate relaxation and the viscosity of the water do nearly all
/// of it, and it is not a small effect: 164 dB/km at 580 kHz and 622 dB/km at
/// 1550 kHz. Over a 30 m swath that is 37 dB of two-way loss on the high
/// channel, which is the whole reason it appears to fade out.
pub fn absorption_db_per_km(f_khz: f64, t_c: f64, salinity_psu: f64, depth_m: f64) -> f64 {
    let (f, t, s, d) = (f_khz, t_c, salinity_psu, depth_m);
    let c = 1412.0 + 3.21 * t + 1.19 * s + 0.0167 * d;
    // boric acid
    let a1 = 8.86 / c * 10f64.powf(0.78 * 8.0 - 5.0);
    let f1 = 2.8 * (s / 35.0).sqrt() * 10f64.powf(4.0 - 1245.0 / (273.0 + t));
    // magnesium sulphate
    let a2 = 21.44 * s / c * (1.0 + 0.025 * t);
    let f2 = 8.17 * 10f64.powf(8.0 - 1990.0 / (273.0 + t)) / (1.0 + 0.0018 * (s - 35.0));
    let p2 = 1.0 - 1.37e-4 * d + 6.2e-9 * d * d;
    // pure water viscosity
    let a3 = if t <= 20.0 {
        4.937e-4 - 2.59e-5 * t + 9.11e-7 * t * t - 1.50e-8 * t.powi(3)
    } else {
        3.964e-4 - 1.146e-5 * t + 1.45e-7 * t * t - 6.50e-10 * t.powi(3)
    };
    let p3 = 1.0 - 3.83e-5 * d + 4.9e-10 * d * d;
    a1 * f1 * f * f / (f1 * f1 + f * f)
        + a2 * p2 * f2 * f * f / (f2 * f2 + f * f)
        + a3 * p3 * f * f
}

/// The gain that undoes propagation loss out to slant range `r`, as a factor on
/// *amplitude*, normalised to 1 at `r_ref`.
///
/// Two losses, neither of them anything to do with the seabed. Spreading: the
/// pulse expands, and the patch of seabed it lights up grows with range too, so
/// backscattered intensity falls as R^-3 and amplitude as R^-1.5. Absorption:
/// the water turns sound into heat at `alpha` dB per metre, two-way.
///
/// `strength` is how much of that to apply, 0 to 1. Full correction is the
/// physically right answer and also amplifies whatever is at the far edge of
/// the swath by nearly forty decibels, noise included; something under one
/// leaves the outer swath visibly dimmer, which is honest about how much less
/// the sonar knows out there.
pub fn tvg_gain(r: f64, r_ref: f64, alpha_db_per_m: f64, strength: f64) -> f32 {
    if !(r > 0.0) || !(r_ref > 0.0) || strength <= 0.0 {
        return 1.0;
    }
    let spread_db = 30.0 * (r / r_ref).log10();
    let absorb_db = 2.0 * alpha_db_per_m * (r - r_ref);
    // dB of intensity -> a factor on amplitude
    (10f64.powf(strength * (spread_db + absorb_db) / 20.0)) as f32
}

/// Flatten the across-track intensity falloff.
///
/// Each sample column is divided by the median over pings, raised to `alpha`,
/// which takes out range spreading and the beam pattern together. The profile
/// is smoothed first so a real target sitting at one range is not divided out
/// along with the shading.
pub fn tvg_profile(img: &[Vec<f32>], width: usize, alpha: f32) -> Vec<f32> {
    if img.is_empty() || width == 0 {
        return vec![1.0; width];
    }
    let mut col = Vec::with_capacity(img.len());
    let mut prof = Vec::with_capacity(width);
    for x in 0..width {
        col.clear();
        col.extend(img.iter().filter_map(|r| r.get(x).copied()));
        prof.push(median(&col));
    }
    let pos: Vec<f32> = prof.iter().copied().filter(|&v| v > 0.0).collect();
    let floor = if pos.is_empty() { 1.0 } else { percentile(&pos, 5.0) };
    for v in prof.iter_mut() {
        *v = v.max(floor);
    }
    let k = (width / 64).max(3) | 1;
    let sm = smooth_trace(&prof, k);
    sm.iter().map(|&v| v.max(1e-9).powf(alpha)).collect()
}

/// Percentile stretch to 0..255 with a gamma.
#[derive(Clone, Copy, Debug)]
pub struct Stretch {
    pub lo: f32,
    pub hi: f32,
    pub gamma: f32,
}

impl Stretch {
    /// Choose the clip points from the data itself.
    pub fn from_data(values: &[f32], lo_pct: f64, hi_pct: f64, gamma: f32) -> Stretch {
        let v: Vec<f32> = values.iter().copied().filter(|x| x.is_finite() && *x > 0.0).collect();
        let (a, b) = if v.is_empty() {
            (0.0, 1.0)
        } else {
            (percentile(&v, lo_pct), percentile(&v, hi_pct))
        };
        Stretch { lo: a, hi: b.max(a + 1e-9), gamma }
    }

    #[inline]
    pub fn apply(&self, v: f32) -> u8 {
        let t = ((v - self.lo) / (self.hi - self.lo)).clamp(0.0, 1.0);
        (t.powf(self.gamma) * 255.0).round() as u8
    }
}

/// Average every `k` columns together.
pub fn bin_columns(row: &[f32], k: usize) -> Vec<f32> {
    if k <= 1 {
        return row.to_vec();
    }
    let n = row.len() / k;
    (0..n)
        .map(|i| row[i * k..(i + 1) * k].iter().sum::<f32>() / k as f32)
        .collect()
}

/// Refine a bottom pick around a prior, in samples.
///
/// The JSF altitude field is smooth and both channels agree on it to a
/// centimetre, which makes it a good prior -- but on this recording it sits
/// about 1.1 m short of where the first return actually is. Left uncorrected
/// that is not a small error: slant-range correction maps a true nadir return
/// to `sqrt(true^2 - assumed^2)` of ground range, so 14.8 m assumed against
/// 15.9 m actual throws the seabed under the fish out to 5.9 m off-track, and
/// the waterfall grows a phantom six-metre channel down the middle of the
/// image.
///
/// Picking the first return outright is what the prior protects against: on
/// the weak high-frequency channel it latches onto noise. Searching a window
/// around the prior gets the accuracy of a real detector with the robustness
/// of the recorded field.
pub fn refine_bottom(trace: &[f32], prior_samples: f32, frac: f32) -> f32 {
    refine_bottom_within(trace, prior_samples, frac, 0.75, 1.45)
}

/// The bottom pick, constrained to a window around the prior.
///
/// The width of that window is the whole argument. The seabed is the *first
/// strong* return and nothing can precede it -- the shortest path to any of it
/// is straight down -- so anything the detector finds earlier is water column:
/// a fish, a scattering layer, the tail of the transmit. With a wide window and
/// a low threshold it finds them, and on `070926_measures_star` it was picking
/// 15.3 m where the return is at 21.5 m and the sonar's own tracker said 20.0.
///
/// That does not merely mislabel the altitude. The ground-range axis reads the
/// trace at `sqrt(g^2 + alt^2)`, so an altitude short by `d` makes the inner
/// `sqrt(A^2 - alt^2)` of every swath read the water column instead of the
/// seabed -- eight to fourteen metres of black, on 13% of pings, in bars that
/// come and go from one ping to the next.
pub fn refine_bottom_within(
    trace: &[f32], prior_samples: f32, frac: f32, lo_f: f32, hi_f: f32,
) -> f32 {
    let n = trace.len();
    if n < 32 || !(prior_samples > 0.0) {
        return prior_samples;
    }
    // The window has to fit inside the trace, and on a short range setting the
    // prior can land at or past its end -- the reported altitude is simply
    // beyond what this ping recorded. There is nothing to refine against then,
    // so keep the prior rather than clamping into a window that is not there.
    let lo = ((prior_samples * lo_f) as usize).max(8);
    let hi = ((prior_samples * hi_f) as usize).min(n - 1);
    if lo + 4 >= hi {
        return prior_samples;
    }
    let win = &trace[lo..hi];
    // Smooth lightly: the envelope is spiky enough that a single sample can
    // cross any threshold on its own.
    let sm = smooth_trace(win, 5);
    let floor = percentile(&sm, 20.0);
    let peak = percentile(&sm, 98.0);
    // Only move off the prior when the window actually contains an edge. A
    // window with no contrast -- a weak channel, or a prior that has already
    // walked past the return -- would otherwise trip any threshold on its very
    // first sample and snap the altitude to three quarters of the prior.
    if !(peak > 0.0) || peak < floor * 3.0 + 1e-6 {
        return prior_samples;
    }
    let level = floor + (peak - floor) * frac;
    for (i, &v) in sm.iter().enumerate() {
        if v >= level {
            return (lo + i) as f32;
        }
    }
    prior_samples
}

/// A steady bottom prior for a run of pings, from the sonar's own tracker.
///
/// The seabed does not move five metres between pings a decimetre apart, so a
/// pick that does is wrong, and the cheapest way to know which one is wrong is
/// to look at its neighbours. Measured over 1200 pings, the mean step between
/// consecutive picks falls from 0.37 m to 0.04 m on `070926_measures_star` and
/// from 0.54 m to 0.10 m on `wpa20260906`.
///
/// Costs nothing: the recorded altitude is in the ping index, so this runs
/// before a single trace is decoded. Entries that are zero -- the sonar did not
/// fill it in -- are left alone for the caller to fall back on.
pub fn steady_prior(priors: &[f32], window: usize) -> Vec<f32> {
    if priors.iter().all(|v| *v <= 0.0) {
        return priors.to_vec();
    }
    // Carry the last good value across gaps so one unfilled ping does not drag
    // its neighbours' median down to zero.
    let mut filled = Vec::with_capacity(priors.len());
    let mut last = priors.iter().copied().find(|v| *v > 0.0).unwrap_or(0.0);
    for &v in priors {
        if v > 0.0 {
            last = v;
        }
        filled.push(last);
    }
    let sm = median_filter(&filled, window);
    priors.iter().zip(sm).map(|(&raw, m)| if raw > 0.0 { m } else { 0.0 }).collect()
}

/// Per-ping gain, so along-track brightness steps do not read as seabed.
///
/// Returns the divisor for one row: its own robust level raised to `strength`.
/// At 0 nothing changes; at 1 every ping is normalised to the same brightness,
/// which also flattens a genuine hard-to-soft transition, so the useful range
/// is in between.
pub fn row_gain(row: &[f32], strength: f32, reference: f32) -> f32 {
    if strength <= 0.0 {
        return 1.0;
    }
    let level = percentile(row, 60.0);
    if !(level > 0.0) || !(reference > 0.0) {
        return 1.0;
    }
    (level / reference).powf(strength)
}

/// Radius of the window the speckle filter measures its statistics over.
///
/// Two cells is 0.9 m across at zoom 19, which is wide enough to hold ~25
/// samples -- enough for a variance to mean anything -- and narrow enough that
/// the seabed inside it is genuinely one thing.
pub const DESPECKLE_RADIUS: usize = 2;

/// The local coefficient of variation taken as "this is only speckle".
///
/// Measured rather than assumed: the flattest ground in the image is by
/// definition the place where nothing but speckle is left, so a low percentile
/// of the local CV over the whole raster is the speckle CV. The 10th rather
/// than the minimum because the minimum is one unlucky window.
const DESPECKLE_CV_PCT: f64 = 10.0;

/// Adaptive speckle filter, after Lee.
///
/// Sidescan speckle is multiplicative -- the pixel-to-pixel scatter over a
/// patch of uniform seabed is proportional to how bright that seabed is -- so a
/// filter with one fixed idea of how much noise there is would be wrong at both
/// ends of the image. Lee's estimator instead asks, per cell, how much of the
/// local variance is more than speckle can account for, and keeps that much of
/// the original:
///
/// ```text
///   k   = max(0, 1 - (cu/ci)^2)      cu: speckle CV, ci: this window's CV
///   out = mean + k * (x - mean)
/// ```
///
/// On flat seabed `ci` falls to `cu`, `k` goes to zero and the cell becomes its
/// local mean. On a target or the edge of a shadow `ci` is far above `cu`, `k`
/// goes to one and the cell is left exactly as it was. That is the whole reason
/// this is not a median filter: a 0.4 m target is three cells at zoom 19 and a
/// median erases it, where this leaves it standing. It matters here -- the
/// recordings this was written for are looking for moorings and ordnance.
///
/// `strength` blends the result back towards the original, so 0 is a no-op and
/// 1 is the full estimator.
///
/// `v` is the value plane, with `f32::NAN` marking a cell nothing was painted
/// into. Those cells are neither read nor written: a hole stays a hole, because
/// filling one would invent coverage the sonar never had.
pub fn despeckle(v: &mut [f32], w: usize, h: usize, radius: usize, strength: f32) {
    if strength <= 0.0 || radius == 0 || w == 0 || h == 0 || v.len() != w * h {
        return;
    }
    let (bsum, bsq, bcnt) = box_stats(v, w, h, radius);

    // The speckle CV, from the flattest windows in the image.
    let mut cvs: Vec<f32> = Vec::new();
    for i in 0..v.len() {
        if let Some((m, var)) = window(&bsum, &bsq, &bcnt, i) {
            if m > 0.0 && var > 0.0 {
                cvs.push(var.sqrt() / m);
            }
        }
    }
    if cvs.is_empty() {
        return;
    }
    let cu = percentile(&cvs, DESPECKLE_CV_PCT);
    let cu2 = (cu * cu) as f64;
    if !(cu2 > 0.0) {
        return;
    }

    for i in 0..v.len() {
        if !v[i].is_finite() {
            continue;
        }
        let Some((m, var)) = window(&bsum, &bsq, &bcnt, i) else { continue };
        if !(m > 0.0) {
            continue;
        }
        // ci^2 = var / mean^2, so k = 1 - cu^2 * mean^2 / var without a divide
        // by a variance that may be zero on perfectly flat ground.
        let denom = var as f64;
        let k = if denom > 0.0 {
            (1.0 - cu2 * (m as f64) * (m as f64) / denom).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let lee = m + k * (v[i] - m);
        v[i] += strength * (lee - v[i]);
    }
}

/// Local mean and variance at one cell, or None where the window held nothing.
#[inline]
fn window(bsum: &[f32], bsq: &[f32], bcnt: &[f32], i: usize) -> Option<(f32, f32)> {
    let n = bcnt[i];
    if n < 1.0 {
        return None;
    }
    let m = bsum[i] / n;
    Some((m, (bsq[i] / n - m * m).max(0.0)))
}

/// Box sums of `v`, `v^2` and the count of finite cells, over a `2r+1` square.
///
/// Separable: a running sum along each row, then along each column, so the
/// work does not grow with the radius. NaN marks a cell with no data and is
/// carried through as a zero contribution and a zero count, which is what makes
/// the mean at the edge of a swath the mean of the cells that are actually
/// there rather than of the void beside them.
fn box_stats(v: &[f32], w: usize, h: usize, r: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let mut sum = vec![0.0f32; w * h];
    let mut sq = vec![0.0f32; w * h];
    let mut cnt = vec![0.0f32; w * h];
    // rows
    for y in 0..h {
        let o = y * w;
        let (mut s, mut q, mut c) = (0.0f64, 0.0f64, 0.0f64);
        let add = |s: &mut f64, q: &mut f64, c: &mut f64, x: f32| {
            if x.is_finite() {
                *s += x as f64;
                *q += (x as f64) * (x as f64);
                *c += 1.0;
            }
        };
        for x in 0..(r + 1).min(w) {
            add(&mut s, &mut q, &mut c, v[o + x]);
        }
        for x in 0..w {
            sum[o + x] = s as f32;
            sq[o + x] = q as f32;
            cnt[o + x] = c as f32;
            // slide: drop x-r, take x+r+1
            if x >= r {
                let d = v[o + x - r];
                if d.is_finite() {
                    s -= d as f64;
                    q -= (d as f64) * (d as f64);
                    c -= 1.0;
                }
            }
            if x + r + 1 < w {
                add(&mut s, &mut q, &mut c, v[o + x + r + 1]);
            }
        }
    }
    // columns, in place over the row sums
    let mut cs = vec![0.0f32; h];
    let mut cq = vec![0.0f32; h];
    let mut cc = vec![0.0f32; h];
    for x in 0..w {
        for y in 0..h {
            cs[y] = sum[y * w + x];
            cq[y] = sq[y * w + x];
            cc[y] = cnt[y * w + x];
        }
        let (mut s, mut q, mut c) = (0.0f64, 0.0f64, 0.0f64);
        for y in 0..(r + 1).min(h) {
            s += cs[y] as f64;
            q += cq[y] as f64;
            c += cc[y] as f64;
        }
        for y in 0..h {
            sum[y * w + x] = s as f32;
            sq[y * w + x] = q as f32;
            cnt[y * w + x] = c as f32;
            if y >= r {
                s -= cs[y - r] as f64;
                q -= cq[y - r] as f64;
                c -= cc[y - r] as f64;
            }
            if y + r + 1 < h {
                s += cs[y + r + 1] as f64;
                q += cq[y + r + 1] as f64;
                c += cc[y + r + 1] as f64;
            }
        }
    }
    (sum, sq, cnt)
}

/// A gain curve measured against grazing angle, one bin per degree from
/// vertical.
///
/// The time-varied gain corrects what physics predicts -- spherical spreading
/// and absorption -- and stops there. What it leaves behind is the transducer's
/// beam pattern and the seabed's own angular response, and on a fish flying low
/// against a long range setting that residual is the largest thing in the
/// picture: measured after `tvg_gain` at full strength, the swath still swings
/// 5x from its brightest angle to its dimmest on `080929_demimines` and 2x on
/// `070926_measures_star`. Every pass then paints a bright core with dark
/// edges, and where two passes cross at different headings the mosaic breaks
/// into patches of different tone.
///
/// So it is measured instead of modelled: the median amplitude at each angle
/// over a sample of the whole recording, divided back out. This is the
/// angle-varying gain of the sidescan literature, and the same thing
/// `tvg_profile` does for the waterfall -- which is exactly why the waterfall
/// has always looked even and the mosaic has not.
///
/// Measured *per side*, because the two are not the same curve: the fish flies
/// with a standing list, which tips its fan down on one side and up on the
/// other, and starboard comes back about 12% brighter than port for the whole
/// of one recording.
#[derive(Clone, Debug)]
pub struct AngleGain {
    /// Divisor per whole degree from vertical. A single element means flat.
    bins: Vec<f32>,
}

/// Samples an angle bin needs before its median is believed.
const ANGLE_MIN_SAMPLES: usize = 20;
/// Bins the curve is smoothed over, so a target sitting at one angle is not
/// divided out along with the shading.
const ANGLE_SMOOTH: usize = 7;

impl AngleGain {
    /// A curve that corrects nothing.
    pub fn flat() -> AngleGain {
        AngleGain { bins: vec![1.0] }
    }

    pub fn is_flat(&self) -> bool {
        self.bins.len() < 2
    }

    /// Reduce per-angle samples to a divisor curve.
    ///
    /// Bins that never held enough samples take their neighbour's value rather
    /// than a 1.0 that would read as a hole in the curve and be smeared across
    /// the bins either side of it by the smoothing. The curve is then
    /// normalised to its own median so that dividing by it re-shades the swath
    /// without rescaling the whole recording, and raised to `strength`, which
    /// is 0 for no correction and 1 for all of it.
    pub fn measure(samples: &mut [Vec<f32>], strength: f32) -> AngleGain {
        if strength <= 0.0 || samples.len() < 2 {
            return AngleGain::flat();
        }
        let mut prof: Vec<f32> = samples
            .iter_mut()
            .map(|v| if v.len() >= ANGLE_MIN_SAMPLES { median(v) } else { f32::NAN })
            .collect();
        if prof.iter().all(|v| !v.is_finite() || *v <= 0.0) {
            return AngleGain::flat();
        }
        // Hold the last measured value forward, then backward, so the ends of
        // the curve extend rather than collapse.
        let mut last = f32::NAN;
        for v in prof.iter_mut() {
            if v.is_finite() && *v > 0.0 {
                last = *v;
            } else {
                *v = last;
            }
        }
        let mut last = f32::NAN;
        for v in prof.iter_mut().rev() {
            if v.is_finite() && *v > 0.0 {
                last = *v;
            } else {
                *v = last;
            }
        }
        let pos: Vec<f32> = prof.iter().copied().filter(|v| v.is_finite() && *v > 0.0).collect();
        if pos.is_empty() {
            return AngleGain::flat();
        }
        let floor = percentile(&pos, 5.0).max(1e-6);
        for v in prof.iter_mut() {
            *v = if v.is_finite() { v.max(floor) } else { floor };
        }
        let sm = smooth_trace(&prof, ANGLE_SMOOTH);
        let mid = median(&sm).max(1e-6);
        AngleGain { bins: sm.iter().map(|&v| (v / mid).max(1e-3).powf(strength)).collect() }
    }

    /// The divisor at an angle, interpolated between bin centres.
    pub fn at(&self, theta_deg: f64) -> f32 {
        let n = self.bins.len();
        if n < 2 {
            return 1.0;
        }
        let x = (theta_deg - 0.5).clamp(0.0, (n - 1) as f64);
        let i = x.floor() as usize;
        if i + 1 >= n {
            return self.bins[n - 1];
        }
        let f = (x - i as f64) as f32;
        self.bins[i] + (self.bins[i + 1] - self.bins[i]) * f
    }
}
