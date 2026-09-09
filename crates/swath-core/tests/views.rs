//! The two views have to agree.
//!
//! A contact marked on the waterfall and the same contact marked on the chart
//! must be the same place on the seabed. That is the one invariant the whole
//! application rests on, and it is easy to break silently: change the bottom
//! detector on one side, or measure a distance with a different earth model,
//! and the two drift apart by an amount too small to see and too large to
//! ignore when a ROV is sent to the position.
//!
//! These need the recordings and the index, so they report and return when
//! those are absent rather than failing.

use std::path::PathBuf;

mod common;

use swath_core::geo;
use swath_core::index::PingIndex;
use swath_core::mosaic::{Mosaic, MosaicConfig, PriorityTable};
use swath_core::nav::{Nav, NavConfig};
use swath_core::project::Workspace;
use swath_core::waterfall::{self, Axis, WaterfallRequest};

const DATASET: &str = "070926_measures_b2";
const SUBSYSTEM: u8 = 20;

fn root() -> PathBuf {
    common::workspace()
}

fn index() -> Option<PingIndex> {
    let p = Workspace::new(root()).index_path(DATASET);
    p.exists().then(|| PingIndex::load(&p).expect("load index"))
}

#[test]
fn waterfall_pixel_resolves_to_its_own_across_track_distance() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let cfg = NavConfig::default();
    let nav = Nav::build(&idx.records, cfg);
    let pairs = waterfall::pair_channels(&idx, SUBSYSTEM);
    let req = WaterfallRequest {
        subsystem: SUBSYSTEM,
        start: 4000,
        count: 600,
        width: 1024,
        stride: 1,
        ..Default::default()
    };
    let wf = waterfall::render(&idx, &nav, &pairs, &req, None);
    assert!(wf.height > 100, "waterfall came back empty");

    // A pixel's distance from the fish must equal the across-track distance the
    // same pixel reports. These are computed by different code paths -- one
    // walks a bearing, the other measures between two positions -- so agreeing
    // is a real constraint, not a tautology.
    let mut worst = 0.0f64;
    let probes: [(f64, f64); 8] = [
        (60.0, 40.0), (120.0, 50.0), (300.0, 150.0), (512.0, 200.0),
        (700.0, 300.0), (900.0, 450.0), (980.0, 520.0), (200.0, 500.0),
    ];
    for &(x, y) in &probes {
        let y = y.min(wf.height as f64 - 1.0);
        let (lat, lon) = wf.pixel_to_world(x, y).expect("pixel in image");
        let r = &wf.rows[y as usize];
        let half = wf.width as f64 / 2.0;
        let across = wf.axis.to_ground((x - half) / half * r.half_width_m, r.altitude);
        let d = geo::distance_m(r.fish_lat, r.fish_lon, lat, lon);
        worst = worst.max((d - across.abs()).abs());
    }
    eprintln!("waterfall pixel -> world: worst {:.4} mm", worst * 1000.0);
    assert!(worst < 0.001, "pixel resolved {worst:.4} m from its own range");
}

/// The same request, drawn on the other across-track axis.
fn req_at(axis: Axis) -> WaterfallRequest {
    WaterfallRequest {
        subsystem: SUBSYSTEM,
        start: 4000,
        count: 600,
        width: 1024,
        stride: 1,
        axis,
        ..Default::default()
    }
}

/// Changing the axis must not move the seabed.
///
/// This is the test that was missing. Every other test here builds its request
/// with `..Default::default()`, and `Ground` is the default, so slant range was
/// never rendered by anything but the application -- where the symptom was the
/// whole image sliding across-track the moment the control was touched.
///
/// One physical return, one distance along the trace, drawn into two images on
/// two different axes. It lands in different columns, which is the point of the
/// control; it has to come back as the same patch of seabed, which is the point
/// of the application.
#[test]
fn the_axis_does_not_move_the_seabed() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let nav = Nav::build(&idx.records, NavConfig::default());
    let pairs = waterfall::pair_channels(&idx, SUBSYSTEM);
    let ground = waterfall::render(&idx, &nav, &pairs, &req_at(Axis::Ground), None);
    let slant = waterfall::render(&idx, &nav, &pairs, &req_at(Axis::Slant), None);
    assert!(ground.height > 100 && slant.height == ground.height);

    let half = ground.width as f64 / 2.0;
    let mut worst = 0.0f64;
    let mut probes = 0;
    for y in (5..ground.height.min(400)).step_by(43) {
        let g = &ground.rows[y];
        let s = &slant.rows[y];
        // Same pings, same navigation, so the same fish and the same bottom.
        assert!((g.altitude - s.altitude).abs() < 1e-9, "altitude differs between axes");
        for side in [-1.0f64, 1.0] {
            for frac in [0.35f64, 0.5, 0.65, 0.8] {
                // Pick a real slant range: one distance down the trace, which
                // both images drew somewhere.
                let slant_m = frac * s.half_width_m;
                if slant_m <= g.altitude * 1.2 {
                    continue; // still in the water column, not seabed
                }
                let ground_m = (slant_m * slant_m - g.altitude * g.altitude).sqrt();
                if ground_m >= g.half_width_m {
                    continue; // past the edge of the ground-range image
                }
                let xg = half + side * ground_m / g.half_width_m * half;
                let xs = half + side * slant_m / s.half_width_m * half;
                let (alat, alon) = ground.pixel_to_world(xg, y as f64).expect("in the ground image");
                let (blat, blon) = slant.pixel_to_world(xs, y as f64).expect("in the slant image");
                worst = worst.max(geo::distance_m(alat, alon, blat, blon));
                probes += 1;
            }
        }
    }
    assert!(probes > 20, "only {probes} probes; the geometry filtered too much");
    eprintln!("ground vs slant, {probes} probes: worst {:.4} m apart", worst);
    assert!(worst < 0.01, "the same return is {worst:.2} m from itself across the two axes");
}

/// The round trip has to hold on the slant axis too, not only the default.
#[test]
fn a_slant_waterfall_pixel_round_trips() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let nav = Nav::build(&idx.records, NavConfig::default());
    let pairs = waterfall::pair_channels(&idx, SUBSYSTEM);
    let wf = waterfall::render(&idx, &nav, &pairs, &req_at(Axis::Slant), None);

    let mut worst = 0.0f64;
    // Clear of nadir: the columns closer in than the altitude are water, and
    // water has no position to round-trip.
    let probes: [(f64, f64); 4] =
        [(120.0, 100.0), (260.0, 250.0), (760.0, 300.0), (900.0, 400.0)];
    for &(x, y) in &probes {
        let y = y.min(wf.height as f64 - 1.0);
        let (lat, lon) = wf.pixel_to_world(x, y).expect("pixel in image");
        let Some((bx, _)) = wf.world_to_pixel(lat, lon) else {
            panic!("a slant position fell outside the image it came from");
        };
        worst = worst.max((bx - x).abs());
    }
    eprintln!("slant round trip: worst column error {worst:.3} px");
    assert!(worst < 2.0, "column drifted {worst:.2} px through the round trip");

    // And nadir itself resolves to nothing rather than to the fish.
    let half = wf.width as f64 / 2.0;
    assert!(wf.pixel_to_world(half + 1.0, 10.0).is_none(),
        "the water column answered with a position on the seabed");
}

/// Ground range is a ground extent, so the outer columns must land on samples
/// the trace actually has rather than past the end of it.
#[test]
fn the_ground_axis_reaches_only_as_far_as_the_trace() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let nav = Nav::build(&idx.records, NavConfig::default());
    let pairs = waterfall::pair_channels(&idx, SUBSYSTEM);
    let ground = waterfall::render(&idx, &nav, &pairs, &req_at(Axis::Ground), None);
    let slant = waterfall::render(&idx, &nav, &pairs, &req_at(Axis::Slant), None);

    let g = ground.rows[0].half_width_m;
    let s = slant.rows[0].half_width_m;
    let alt = ground.rows[0].altitude;
    eprintln!("half width: {g:.2} m ground, {s:.2} m slant, altitude {alt:.2} m");
    assert!(g < s, "ground half-width {g:.2} is not shorter than the slant {s:.2}");
    // The outermost ground column asks for a slant range the trace has.
    let want = (g * g + alt * alt).sqrt();
    assert!(want <= s * 1.02, "the ground edge wants {want:.2} m of a {s:.2} m trace");
}

#[test]
fn waterfall_pixel_round_trips_through_the_image() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let nav = Nav::build(&idx.records, NavConfig::default());
    let pairs = waterfall::pair_channels(&idx, SUBSYSTEM);
    let req = WaterfallRequest {
        subsystem: SUBSYSTEM,
        start: 4000,
        count: 600,
        width: 1024,
        stride: 1,
        ..Default::default()
    };
    let wf = waterfall::render(&idx, &nav, &pairs, &req, None);

    // Forward then back. `world_to_pixel` searches for the nearest row rather
    // than being handed one, so it can legitimately pick a neighbour where the
    // track doubles back -- the column is the part that has to be tight.
    let mut worst_x = 0.0f64;
    let probes: [(f64, f64); 4] =
        [(200.0, 100.0), (400.0, 250.0), (620.0, 300.0), (820.0, 400.0)];
    for &(x, y) in &probes {
        let y = y.min(wf.height as f64 - 1.0);
        let (lat, lon) = wf.pixel_to_world(x, y).expect("pixel in image");
        let Some((bx, _by)) = wf.world_to_pixel(lat, lon) else {
            panic!("position fell outside the image it came from");
        };
        worst_x = worst_x.max((bx - x).abs());
    }
    eprintln!("waterfall round trip: worst column error {worst_x:.3} px");
    assert!(worst_x < 2.0, "column drifted {worst_x:.2} px through the round trip");
}

#[test]
fn a_waterfall_position_has_mosaic_under_it() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let cfg = MosaicConfig { subsystem: SUBSYSTEM, ..Default::default() };
    let mpath = Workspace::new(root()).mosaic_path(
        DATASET,
        SUBSYSTEM,
        &swath_core::mosaic::key(&cfg)[..12],
    );
    let mosaic = if let Some(m) =
        mpath.exists().then(|| Mosaic::load(&mpath).ok()).flatten().filter(|m: &Mosaic| m.header.matches(&cfg))
    {
        m
    } else {
        Mosaic::build(&idx, &cfg).expect("build mosaic")
    };

    let nav = Nav::build(&idx.records, NavConfig::default());
    let pairs = waterfall::pair_channels(&idx, SUBSYSTEM);
    let req = WaterfallRequest {
        subsystem: SUBSYSTEM,
        start: 4000,
        count: 600,
        width: 1024,
        stride: 1,
        ..Default::default()
    };
    let wf = waterfall::render(&idx, &nav, &pairs, &req, None);

    // Away from the nadir band, which the mosaic's priority table deliberately
    // leaves unpainted, every pixel of the waterfall should have imagery under
    // it on the chart.
    let (mut hit, mut total) = (0, 0);
    for yi in (10..wf.height.min(500)).step_by(37) {
        for xi in (40..wf.width - 40).step_by(53) {
            let half = wf.width as f64 / 2.0;
            let across = (xi as f64 - half) / half * wf.rows[yi].half_width_m;
            if across.abs() < 8.0 {
                continue; // nadir, not painted by design
            }
            let Some((lat, lon)) = wf.pixel_to_world(xi as f64, yi as f64) else { continue };
            total += 1;
            if mosaic.sample(lat, lon).is_some() {
                hit += 1;
            }
        }
    }
    assert!(total > 50, "not enough samples to mean anything");
    let frac = hit as f64 / total as f64;
    eprintln!("waterfall positions with mosaic under them: {hit}/{total} ({:.1}%)", frac * 100.0);
    assert!(frac > 0.97, "only {:.1}% of waterfall pixels landed on imagery", frac * 100.0);
}

/// The report names its sections after the frequency the sonar transmitted, so
/// the frequency has to survive the index.
#[test]
fn the_index_remembers_what_each_channel_transmits() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let bands = idx.bands();
    assert_eq!(bands.len(), 2, "expected two sidescan channels, got {}", bands.len());
    for b in &bands {
        eprintln!(
            "ss{} {:.0}-{:.0} kHz, {} pings",
            b.subsystem, b.f0 / 1000.0, b.f1 / 1000.0, b.pings
        );
        assert!(b.f1 > b.f0, "ss{} does not sweep upward", b.subsystem);
        assert!(b.pings > 0, "ss{} has no pings", b.subsystem);
    }
    // The two are far enough apart to be called high and low without hedging.
    let hi = bands.iter().map(|b| b.f0).fold(f32::MIN, f32::max);
    let lo = bands.iter().map(|b| b.f0).fold(f32::MAX, f32::min);
    assert!(hi > lo * 1.5, "the two bands are not distinguishable: {lo} and {hi} Hz");
    assert_eq!(bands.iter().map(|b| b.pings).sum::<usize>(), idx.len(),
        "the bands do not account for every ping");
}


/// The mosaic must not have a hole down the middle of every line.
///
/// Every priority table scores nadir at zero, which is the conventional choice
/// and blanks a strip two altitudes wide -- seven metres here -- along every
/// pass. `NADIR_FLOOR` paints it at the lowest priority instead, so it fills
/// only where nothing else reaches. This walks out from each ping's own fish
/// position and asks whether there is imagery there.
#[test]
fn the_mosaic_has_no_strip_down_the_middle() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let cfg = MosaicConfig { subsystem: SUBSYSTEM, ..Default::default() };
    let m = Mosaic::build(&idx, &cfg).expect("build mosaic");
    let recs: Vec<_> = idx.records.iter().copied().filter(|r| r.subsystem == SUBSYSTEM).collect();
    let nav = Nav::build(&recs, cfg.nav);

    for a in [1.0f64, 3.0, 10.0, 25.0, 40.0] {
        let (mut hit, mut tot) = (0usize, 0usize);
        for (k, r) in recs.iter().enumerate() {
            if k % 101 != 0 {
                continue;
            }
            let f = nav.fix(r);
            for side in [-1.0f64, 1.0] {
                let (lat, lon) = geo::offset_m(f.lat, f.lon, f.bearing + 90.0, side * a);
                tot += 1;
                hit += m.sample(lat, lon).is_some() as usize;
            }
        }
        let frac = hit as f64 / tot.max(1) as f64;
        eprintln!("{a:5.1} m across -> {:5.1}% covered", frac * 100.0);
        assert!(frac > 0.97, "only {:.1}% covered at {a} m across", frac * 100.0);
    }
}

/// The priority table decides how a cell is shaded, never where it lands.
///
/// This is the invariant behind dropping the `water_column` setting. A sample's
/// across-track position comes out of `build_stroke`'s ground-range loop; its
/// priority comes out of a lookup on the grazing angle. Nothing reads the
/// second to compute the first, and the sonar cannot see under itself any
/// better or worse for our having changed a weight. So two mosaics built with
/// opposite tables -- `Outer` favours the far field, `Nadir` favours straight
/// down -- must paint exactly the same cells, and differ only in what shade
/// they paint them.
#[test]
fn the_priority_table_shades_the_mosaic_but_does_not_move_it() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let build = |table| {
        Mosaic::build(&idx, &MosaicConfig { subsystem: SUBSYSTEM, table, ..Default::default() })
            .expect("build mosaic")
    };
    let outer = build(PriorityTable::Outer);
    let nadir = build(PriorityTable::Nadir);

    assert_eq!(
        (outer.header.x0, outer.header.y0, outer.header.width, outer.header.height),
        (nadir.header.x0, nadir.header.y0, nadir.header.width, nadir.header.height),
        "the two rasters do not even cover the same ground"
    );

    let (mut only_outer, mut only_nadir, mut both, mut shaded) = (0usize, 0usize, 0usize, 0usize);
    for i in 0..outer.value.len() {
        match (outer.cover[i] > 0, nadir.cover[i] > 0) {
            (true, false) => only_outer += 1,
            (false, true) => only_nadir += 1,
            (true, true) => {
                both += 1;
                shaded += (outer.value[i] != nadir.value[i]) as usize;
            }
            _ => {}
        }
    }
    eprintln!("painted by both tables: {both}, differently shaded: {shaded}");
    assert!(both > 1_000_000, "only {both} cells painted; the fixture is not what it was");
    assert!(shaded > both / 100, "the two tables produced the same picture; is priority applied?");
    assert_eq!(
        (only_outer, only_nadir),
        (0, 0),
        "changing the priority table moved the imagery: {only_outer} cells appear only \
         with the outer table and {only_nadir} only with the nadir table"
    );
}

/// The speed of sound scales every distance and no angle.
///
/// It multiplies a travel time, so it stretches the whole across-track axis by
/// exactly its own ratio -- both the range to a sample and the altitude beneath
/// the fish, which is why the grazing angle, being their ratio, does not move.
/// Getting this wrong is a 1.6% error in every across-track distance on this
/// survey: 0.75 m at the edge of a 47 m swath, always short.
#[test]
fn the_speed_of_sound_scales_the_swath_and_not_the_angles() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let nav = Nav::build(&idx.records, NavConfig::default());
    let pairs = waterfall::pair_channels(&idx, SUBSYSTEM);
    let shot = |c: f64| {
        waterfall::render(&idx, &nav, &pairs, &WaterfallRequest {
            subsystem: SUBSYSTEM, start: 0, count: 256, width: 512,
            sound_speed_m_s: c, ..Default::default()
        }, None)
    };
    let (slow, fast) = (shot(1500.0), shot(1524.0));
    let want = 1524.0 / 1500.0;
    assert_eq!(slow.rows.len(), fast.rows.len(), "the block changed shape");

    for y in [0usize, 40, 120, 200] {
        if y >= slow.rows.len() { continue }
        let (a, b) = (&slow.rows[y], &fast.rows[y]);
        let ratio = b.half_width_m / a.half_width_m;
        assert!((ratio - want).abs() < 2e-3,
            "row {y}: half width scaled by {ratio:.5}, wanted {want:.5}");
        let alt = b.altitude / a.altitude;
        assert!((alt - want).abs() < 2e-3,
            "row {y}: altitude scaled by {alt:.5}, wanted {want:.5}");

        // ...and the same pixel therefore lands further out, by the same ratio.
        for x in [300.0f64, 420.0, 500.0] {
            let (Some(p), Some(q)) = (slow.pixel_to_world(x, y as f64),
                                      fast.pixel_to_world(x, y as f64)) else { continue };
            let da = geo::distance_m(a.fish_lat, a.fish_lon, p.0, p.1);
            let db = geo::distance_m(b.fish_lat, b.fish_lon, q.0, q.1);
            if da < 1.0 { continue }
            let r = db / da;
            assert!((r - want).abs() < 5e-3,
                "pixel ({x}, {y}) moved from {da:.2} m to {db:.2} m, a factor of {r:.5}");
        }
    }
    let a = &slow.rows[slow.rows.len() / 2];
    let b = &fast.rows[fast.rows.len() / 2];
    eprintln!("1500 -> 1524 m/s: half swath {:.2} m -> {:.2} m, altitude {:.2} m -> {:.2} m",
        a.half_width_m, b.half_width_m, a.altitude, b.altitude);
}

/// The mosaic must not paint a bright band down the middle of every pass.
///
/// Spreading and absorption take 39 dB out of a 47 m swath at 580 kHz, and for
/// months `MosaicConfig::tvg` was declared, defaulted and digested without being
/// read -- so the mosaic had no across-track correction at all and brightness
/// fell from 127 grey at 15 m to 28 by 45 m. That is a picture of the sonar,
/// not of the seabed, and it is also why two passes over the same ground at
/// different ranges disagreed about how bright it was.
#[test]
fn the_time_varied_gain_flattens_the_swath() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let profile = |tvg: f32| -> Vec<(f64, f64)> {
        // `angular_gain` off in both arms: it measures the across-track
        // shading and divides it out whatever the tvg did, so leaving it on
        // would flatten the uncorrected arm too and this would be testing
        // nothing.
        let cfg = MosaicConfig {
            subsystem: SUBSYSTEM, tvg, sound_speed_m_s: 1524.0,
            centre_freq_hz: 580_000.0, angular_gain: 0.0, ..Default::default()
        };
        let m = Mosaic::build(&idx, &cfg).expect("build mosaic");
        let recs: Vec<_> =
            idx.records.iter().copied().filter(|r| r.subsystem == SUBSYSTEM).collect();
        let nav = Nav::build(&recs, cfg.nav);
        [15.0f64, 25.0, 35.0, 45.0].iter().map(|&a| {
            let (mut sum, mut n) = (0u64, 0u64);
            for (k, r) in recs.iter().enumerate() {
                if k % 53 != 0 { continue }
                let f = nav.fix(r);
                for side in [-1.0f64, 1.0] {
                    let (lat, lon) = geo::offset_m(f.lat, f.lon, f.bearing + 90.0, side * a);
                    if let Some(v) = m.sample(lat, lon) { sum += v as u64; n += 1 }
                }
            }
            (a, sum as f64 / n.max(1) as f64)
        }).collect()
    };
    let fall = |p: &[(f64, f64)]| p[0].1 / p[p.len() - 1].1;

    let off = profile(0.0);
    let on = profile(0.7);
    for (label, p) in [("no gain", &off), ("gain 0.7", &on)] {
        let line: Vec<String> =
            p.iter().map(|(a, v)| format!("{a:.0} m: {v:5.1}")).collect();
        eprintln!("{label:9} {}   peak/edge = {:.2}", line.join("   "), fall(p));
    }
    assert!(fall(&off) > 2.0, "the uncorrected swath was already flat; is this the fixture?");
    assert!(fall(&on) < fall(&off) * 0.75,
        "the gain barely helped: {:.2} against {:.2}", fall(&on), fall(&off));
}

/// The water column must not show through the ground-range axis.
///
/// The ground axis reads the trace at `sqrt(g^2 + alt^2)`, so an altitude short
/// by `d` makes the inner `sqrt(A^2 - alt^2)` of every swath sample *above* the
/// seabed -- water, which is black. On `070926_measures_star` the detector was
/// picking 15.3 m where the return is at 21.5 m and the sonar's own tracker
/// said 20.0, and the result was eight to fourteen metres of black at nadir
/// coming and going from one ping to the next.
///
/// This measures the inner swath rather than counting bars, because a bar
/// counter is easy to fool: an earlier attempt at this fix reduced the bars on
/// one recording while doubling the darkness on another.
#[test]
fn the_ground_axis_does_not_show_the_water_column() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let nav = Nav::build(&idx.records, NavConfig::default());
    let pairs = waterfall::pair_channels(&idx, SUBSYSTEM);
    let (mut inner, mut dark) = (0u64, 0u64);
    for start in [2000usize, 10000, 20000] {
        if start + 600 > pairs.len() {
            continue;
        }
        let wf = waterfall::render(&idx, &nav, &pairs, &WaterfallRequest {
            subsystem: SUBSYSTEM, start, count: 600, width: 512, axis: Axis::Ground,
            sound_speed_m_s: 1524.0, ..Default::default()
        }, None);
        let half = wf.width / 2;
        for y in 0..wf.height {
            let row = &wf.pixels[y * wf.width..(y + 1) * wf.width];
            let mpp = wf.rows[y].half_width_m / half as f64;
            for (x, &v) in row.iter().enumerate() {
                if ((x as f64 - half as f64) * mpp).abs() < 15.0 {
                    inner += 1;
                    dark += (v < 30) as u64;
                }
            }
        }
    }
    let frac = 100.0 * dark as f64 / inner.max(1) as f64;
    eprintln!("{frac:.2}% of the inner 15 m is dark (was 10.76% at frac 0.25)");
    assert!(inner > 100_000, "only {inner} pixels sampled; the fixture is not what it was");
    assert!(frac < 9.0, "{frac:.2}% of the inner swath is dark; the bottom pick is short again");
}

/// The bottom pick must land on the seabed, not in the water above it.
///
/// Steadiness is not the test -- a detector that simply returned its prior
/// would be perfectly steady and perfectly wrong, and in fact the old settings
/// wandered *less* than these do while being several metres short. What matters
/// is agreement with the trace's own first strong return, which is what the
/// seabed is: nothing can precede it, because the shortest path to any seabed
/// is straight down. Measured on this fixture, the old settings picked more
/// than a metre short on 16 pings in 600; these pick short on one.
#[test]
fn the_bottom_pick_lands_on_the_seabed() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let recs: Vec<_> = idx
        .records
        .iter()
        .copied()
        .filter(|r| r.subsystem == SUBSYSTEM && r.channel == 0)
        .collect();
    let skip = 20_000.min(recs.len().saturating_sub(600));
    let priors: Vec<f32> = recs[skip..].iter().take(600)
        .map(|r| r.altitude_samples().unwrap_or(0.0)).collect();
    let priors = swath_core::signal::steady_prior(&priors, waterfall::BOTTOM_MEDIAN);

    let mut err: Vec<f64> = Vec::new();
    let mut short = 0usize;
    for (r, &p) in recs[skip..].iter().take(600).zip(priors.iter()) {
        let Ok(jf) = idx.file_of(r) else { continue };
        let Some(ping) = jf.ping_at(r.offset) else { continue };
        let res = r.resolution_m(1524.0);
        let got = swath_core::signal::refine_bottom_within(
            &ping.data, p, waterfall::BOTTOM_FRAC, waterfall::BOTTOM_LO, waterfall::BOTTOM_HI,
        ) as f64 * res;
        // The first strong return in the trace, found without any prior at all.
        let sm = swath_core::signal::smooth_trace(&ping.data, 9);
        let strong = swath_core::signal::percentile(&sm, 99.0);
        let want = sm.iter().position(|&v| v >= strong * 0.5).unwrap_or(0) as f64 * res;
        if want <= 0.0 {
            continue;
        }
        err.push(got - want);
        // Short is the failure that shows: it makes the ground axis read water.
        if got < want - 1.0 {
            short += 1;
        }
    }
    assert!(err.len() > 400, "only {} pings measured", err.len());
    let mut abs: Vec<f64> = err.iter().map(|e| e.abs()).collect();
    abs.sort_by(f64::total_cmp);
    let median = abs[abs.len() / 2];
    let bias: f64 = err.iter().sum::<f64>() / err.len() as f64;
    eprintln!(
        "bottom pick vs the trace's own first strong return: median |error| {median:.2} m, \
bias {bias:+.2} m, {short} of {} pings more than a metre short",
        err.len()
    );
    assert!(median < 0.6, "median error {median:.2} m");
    assert!(
        short * 10 < err.len(),
        "{short} of {} pings pick short of the seabed by over a metre; the ground axis \
will read water column there",
        err.len()
    );
}

/// Blanking the nadir band must remove that band and nothing else.
///
/// Not the same decision as `NADIR_FLOOR`, which is unconditional because
/// painting the band adds coverage where there is none. This is the operator
/// saying they would rather have the hole, and the only thing it may do is make
/// one -- everything outside must be untouched.
///
/// Note that coverage inside does not fall to zero, and should not: another
/// pass reaching the same ground from its own outer swath still paints it, and
/// that is a better look at it than this pass's beam edge.
#[test]
fn blanking_the_nadir_removes_the_band_and_nothing_else() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let recs: Vec<_> =
        idx.records.iter().copied().filter(|r| r.subsystem == SUBSYSTEM).collect();
    let coverage = |blank: f64| -> Vec<(f64, f64)> {
        let cfg = MosaicConfig {
            subsystem: SUBSYSTEM, nadir_blank_m: blank, ..Default::default()
        };
        let m = Mosaic::build(&idx, &cfg).expect("build mosaic");
        let nav = Nav::build(&recs, cfg.nav);
        [5.0f64, 15.0, 25.0, 40.0]
            .iter()
            .map(|&a| {
                let (mut hit, mut tot) = (0u32, 0u32);
                for (k, r) in recs.iter().enumerate() {
                    if k % 53 != 0 {
                        continue;
                    }
                    let f = nav.fix(r);
                    for side in [-1.0f64, 1.0] {
                        let (lat, lon) =
                            geo::offset_m(f.lat, f.lon, f.bearing + 90.0, side * a);
                        tot += 1;
                        hit += m.sample(lat, lon).is_some() as u32;
                    }
                }
                (a, hit as f64 / tot.max(1) as f64)
            })
            .collect()
    };
    let off = coverage(0.0);
    let on = coverage(10.0);
    for ((a, u), (_, v)) in off.iter().zip(&on) {
        eprintln!("{a:5.1} m across: {:5.1}% -> {:5.1}%", u * 100.0, v * 100.0);
    }
    assert!(on[0].1 < off[0].1 * 0.8, "5 m in, the blank did nothing");
    for i in 1..off.len() {
        assert!(
            (on[i].1 - off[i].1).abs() < 0.02,
            "coverage at {} m changed from {:.1}% to {:.1}%; the blank reached outside itself",
            off[i].0, off[i].1 * 100.0, on[i].1 * 100.0
        );
    }
}

/// The along-track gain must close the gap between the two sides.
///
/// This used to assert the opposite thing -- that a per-*ping* gain moves only
/// the component port and starboard share, because by construction that was all
/// it could move. That design is superseded: half the banding is a see-saw
/// between the sides, the sides correlate at only r = 0.50 over 8000 pings, the
/// imbalance tracks roll at r = -0.75, and a single scale factor per ping
/// multiplies both sides by the same number and cannot touch any of it. So the
/// gain is now measured and applied per side, and what it has to show for
/// itself is a smaller imbalance rather than a smaller shared component.
///
/// How much smaller is not a fixed number and the test does not pretend
/// otherwise: a per-side gain also erases genuine port-to-starboard differences
/// in the seabed, which is why the window is long. It asks only that the
/// imbalance move the right way and by more than noise.
#[test]
fn the_along_track_gain_closes_the_gap_between_the_sides() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let recs: Vec<_> =
        idx.records.iter().copied().filter(|r| r.subsystem == SUBSYSTEM).collect();
    let imbalance = |agc: f32| -> f64 {
        // `angular_gain` off in both arms. It is measured per side and
        // normalised per side, so it removes the static part of the imbalance
        // by construction -- leaving it on would hand this test a residual the
        // along-track gain was never the thing correcting.
        let cfg = MosaicConfig {
            subsystem: SUBSYSTEM, agc, angular_gain: 0.0, ..Default::default()
        };
        let m = Mosaic::build(&idx, &cfg).expect("build mosaic");
        let nav = Nav::build(&recs, cfg.nav);
        let (mut port, mut stbd) = (Vec::new(), Vec::new());
        for r in recs.iter() {
            let f = nav.fix(r);
            let mean = |sign: f64| -> Option<f64> {
                let (mut s, mut n) = (0.0f64, 0u32);
                for g in [15.0f64, 20.0, 25.0, 30.0, 35.0] {
                    let (lat, lon) = geo::offset_m(f.lat, f.lon, f.bearing + 90.0, sign * g);
                    if let Some(v) = m.sample(lat, lon) {
                        s += v as f64;
                        n += 1;
                    }
                }
                (n >= 4).then(|| s / n as f64)
            };
            if let (Some(p), Some(s)) = (mean(-1.0), mean(1.0)) {
                port.push(p);
                stbd.push(s);
            }
        }
        if port.len() < 500 {
            return f64::NAN;
        }
        // Smoothed to the scale the banding lives at. Raw, single-look speckle
        // is most of the variance and it is uncorrelated between the sides by
        // construction, so it dilutes the very thing being measured.
        let f32s = |v: &[f64]| v.iter().map(|x| *x as f32).collect::<Vec<f32>>();
        let port: Vec<f64> = swath_core::signal::smooth_trace(&f32s(&port), 9)
            .iter().map(|x| *x as f64).collect();
        let stbd: Vec<f64> = swath_core::signal::smooth_trace(&f32s(&stbd), 9)
            .iter().map(|x| *x as f64).collect();
        // The see-saw itself: how far apart the two sides sit on a ping,
        // typical over the recording.
        let mut v: Vec<f64> = port
            .iter()
            .zip(&stbd)
            .filter(|(p, s)| **p + **s > 1.0)
            .map(|(p, s)| ((p - s) / (p + s)).abs())
            .collect();
        if v.len() < 100 {
            return f64::NAN;
        }
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let off = imbalance(0.0);
    let on = imbalance(0.5);
    if !off.is_finite() || !on.is_finite() {
        eprintln!("too little overlap on this fixture; skipping");
        return;
    }
    eprintln!(
        "median |port-starboard| imbalance: {:.1} -> {:.1} points",
        off * 100.0,
        on * 100.0
    );
    assert!(off > 0.01, "the sides were level to begin with: {off:.4}");
    // Only that it moves the right way. This fixture is a single short line
    // flown at a steady altitude, where the see-saw is small and the gain has
    // little to take out -- 10.4 to 10.3 points. The recording where it earns
    // its keep is `070926_measures_star`, which wanders; the numbers there are
    // in `docs/banding.md`. Asserting a size here would be asserting a property
    // of the fixture.
    assert!(
        on <= off,
        "the per-side gain made the imbalance worse: {:.1} points against {:.1} with it off",
        on * 100.0,
        off * 100.0
    );
}

/// The picture must not depend on where the viewer's buffer starts.
///
/// The waterfall is fetched in 2048-ping blocks and stacked, so a ping near the
/// top of one block and a ping near the bottom of the one before it are drawn
/// side by side. Before the gain model they were normalised against different
/// neighbourhoods -- a different frame width, a different across-track profile,
/// a different level to equalise towards and a different contrast stretch, all
/// four measured from whichever 2048 pings the block happened to hold. Measured
/// on `080929_demimines`, the same pings rendered in two differently aligned
/// blocks agreed on 3.4% of their pixels and the mean grey at a seam stepped
/// three times as far as it does between two ordinary neighbouring pings.
#[test]
fn the_same_pings_look_the_same_in_any_block() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let nav = Nav::build(&idx.records, NavConfig::default());
    let pairs = waterfall::pair_channels(&idx, SUBSYSTEM);
    if pairs.len() < 3000 {
        eprintln!("{DATASET} is too short for this test; skipping");
        return;
    }
    let req = |start: usize| WaterfallRequest {
        subsystem: SUBSYSTEM,
        start,
        count: 1024,
        width: 512,
        stride: 1,
        axis: Axis::Ground,
        ..Default::default()
    };
    let gains = waterfall::build_gain_model(&idx, &pairs, &req(0));

    // Two blocks 512 pings out of step, so their overlap is the same 512 pings
    // sitting at opposite ends of the two images.
    let a = waterfall::render(&idx, &nav, &pairs, &req(1000), Some(&gains));
    let b = waterfall::render(&idx, &nav, &pairs, &req(1512), Some(&gains));
    assert!(a.height >= 1024 && b.height >= 512, "blocks came back short");

    let w = a.width;
    let mut same = 0usize;
    let mut total = 0usize;
    for y in 0..512 {
        let ra = &a.pixels[(512 + y) * w..(512 + y + 1) * w];
        let rb = &b.pixels[y * w..(y + 1) * w];
        assert_eq!(
            a.rows[512 + y].ping_row, b.rows[y].ping_row,
            "the two blocks disagree about which ping row {y} is"
        );
        for (x, y2) in ra.iter().zip(rb.iter()) {
            total += 1;
            if x == y2 {
                same += 1;
            }
        }
    }
    let agree = same as f64 / total as f64;
    assert!(
        agree > 0.999,
        "the same {} pings drawn in two blocks agree on only {:.1}% of their pixels",
        512,
        100.0 * agree
    );

    // And the frame they are drawn in has to be the same, or a target lands in
    // a different column depending on which block caught it.
    for y in 0..512 {
        assert!(
            (a.rows[512 + y].half_width_m - b.rows[y].half_width_m).abs() < 1e-9,
            "row {y} is drawn at a different across-track scale in the two blocks"
        );
    }
}

/// The speckle filter has to take the noise and leave the target.
///
/// A median filter would pass the first half of this and fail the second, which
/// is exactly why the mosaic uses an adaptive one instead: a 0.4 m return is
/// three cells at zoom 19, and losing it is not a cosmetic regression.
#[test]
fn the_speckle_filter_spares_what_is_really_there() {
    use swath_core::signal;

    let (w, h) = (192usize, 192usize);
    // Flat seabed at 100, with 30% multiplicative scatter on it -- a plain
    // deterministic hash, so the test does not need a random number generator.
    let mut plane: Vec<f32> = (0..w * h)
        .map(|i| {
            let n = ((i as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407)
                >> 33) as f32
                / (1u32 << 31) as f32;
            100.0 * (0.7 + 0.6 * n)
        })
        .collect();
    // A three-cell bright target, well clear of anything speckle produces.
    let tgt = 96 * w + 96;
    for d in [0usize, 1, w, w + 1] {
        plane[tgt + d] = 900.0;
    }

    let before: Vec<f32> = plane.clone();
    signal::despeckle(&mut plane, w, h, signal::DESPECKLE_RADIUS, 1.0);

    // Background: scatter should collapse.
    let bg = |v: &[f32]| {
        let s: Vec<f32> = (0..w * h)
            .filter(|&i| i / w < 80)
            .map(|i| v[i])
            .collect();
        let m = s.iter().sum::<f32>() / s.len() as f32;
        (s.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / s.len() as f32).sqrt() / m
    };
    let (cv0, cv1) = (bg(&before), bg(&plane));
    assert!(
        cv1 < cv0 * 0.5,
        "background scatter only went from {cv0:.3} to {cv1:.3}; the filter is not working"
    );
    // Mean brightness must not move: this smooths, it does not rescale.
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    assert!(
        (mean(&plane) / mean(&before) - 1.0).abs() < 0.02,
        "the filter changed the overall level"
    );

    // Target: still there, and still most of its contrast.
    let peak = plane[tgt].max(plane[tgt + 1]).max(plane[tgt + w]).max(plane[tgt + w + 1]);
    assert!(
        peak > 0.8 * 900.0,
        "the target came out at {peak:.0} against 900; the filter ate it"
    );
}

/// A hole in the coverage stays a hole.
#[test]
fn the_speckle_filter_does_not_invent_coverage() {
    use swath_core::signal;
    let (w, h) = (64usize, 64usize);
    let mut plane: Vec<f32> = vec![50.0; w * h];
    for y in 20..30 {
        for x in 20..30 {
            plane[y * w + x] = f32::NAN;
        }
    }
    signal::despeckle(&mut plane, w, h, signal::DESPECKLE_RADIUS, 1.0);
    for y in 20..30 {
        for x in 20..30 {
            assert!(plane[y * w + x].is_nan(), "the filter painted into a hole at {x},{y}");
        }
    }
}

/// The angle-varying gain has to flatten the swath, not the seabed.
///
/// The mosaic's time-varied gain corrects spreading and absorption, which is
/// what physics predicts, and leaves the beam pattern and the seabed's angular
/// response behind. Measured on the recordings here that residual is a 5x
/// across-track swing on `080929_demimines` and 2x on `070926_measures_star` --
/// a bright core and dark edges on every pass, and a patchwork of tone wherever
/// two passes cross at different headings.
#[test]
fn the_angle_gain_flattens_the_swath_and_spares_a_target() {
    use swath_core::signal::AngleGain;

    // A swath that peaks mid-range and falls away either side, the shape a
    // depressed array actually has, with a bright target sitting at one angle.
    let shape = |a: usize| -> f32 {
        let x = (a as f32 - 45.0) / 25.0;
        40.0 * (-x * x).exp() + 1.0
    };
    let mut bins: Vec<Vec<f32>> = (0..90)
        .map(|a| {
            let base = shape(a);
            (0..200).map(|i| base * (0.8 + 0.4 * ((i % 7) as f32 / 7.0))).collect()
        })
        .collect();
    let target_angle = 30usize;
    for v in bins[target_angle].iter_mut().take(6) {
        *v *= 12.0;
    }

    let g = AngleGain::measure(&mut bins.clone(), 1.0);
    assert!(!g.is_flat(), "the curve came back flat");

    // Corrected, the swath must be level across the angles the sonar lights.
    let corrected: Vec<f32> = (10..80).map(|a| shape(a) / g.at(a as f64 + 0.5)).collect();
    let lo = corrected.iter().copied().fold(f32::MAX, f32::min);
    let hi = corrected.iter().copied().fold(0.0f32, f32::max);
    let before = shape(45) / shape(10);
    assert!(
        hi / lo < 1.15,
        "swath still swings {:.2}x after correction (it was {before:.1}x)",
        hi / lo
    );

    // The curve is a median over the whole recording, so six bright samples in
    // one bin must not pull it up and dim the target back into the seabed.
    let plain = AngleGain::measure(&mut bins, 1.0);
    let ratio = plain.at(target_angle as f64 + 0.5) / g.at(target_angle as f64 + 0.5);
    assert!(
        (ratio - 1.0).abs() < 0.05,
        "a target moved the gain curve by {:.0}%",
        100.0 * (ratio - 1.0).abs()
    );

    // Strength zero is a no-op, and the curve never divides by zero.
    assert!(AngleGain::measure(&mut bins, 0.0).is_flat());
    for a in 0..90 {
        assert!(g.at(a as f64).is_finite() && g.at(a as f64) > 0.0);
    }
}


/// The mosaic's axis has to mean what the waterfall's axis means.
///
/// Ground range is the slant-range correction: a sample goes where the seabed
/// it came off is, and the water column disappears because ground range zero
/// *is* the first bottom return. Slant range draws what the sonar measured, so
/// the water column stays -- a band two flying heights wide down the middle of
/// every pass, dark because there is nothing in it to return.
///
/// That band is the whole observable difference between the two, so it is what
/// this measures: just off the track line, ground range must be lit and slant
/// range must not.
#[test]
fn the_slant_axis_keeps_the_water_column_and_ground_does_not() {
    use swath_core::waterfall::Axis;

    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    assert_eq!(
        MosaicConfig::default().axis,
        Axis::Ground,
        "the default axis moved; every mosaic is now drawn somewhere else"
    );

    let recs: Vec<_> =
        idx.records.iter().copied().filter(|r| r.subsystem == SUBSYSTEM).collect();
    let mut alts: Vec<f64> =
        recs.iter().map(|r| r.altitude as f64).filter(|a| *a > 1.0).collect();
    alts.sort_by(f64::total_cmp);
    if alts.len() < 500 {
        eprintln!("no altitudes on this fixture; skipping");
        return;
    }
    let alt = alts[alts.len() / 2];

    // Mean painted level at an across-track distance, both sides.
    let level = |axis: Axis, across: f64| -> f64 {
        let cfg = MosaicConfig { subsystem: SUBSYSTEM, axis, ..Default::default() };
        let m = Mosaic::build(&idx, &cfg).expect("build mosaic");
        let nav = Nav::build(&recs, cfg.nav);
        let (mut sum, mut n) = (0u64, 0u64);
        for (k, r) in recs.iter().enumerate() {
            if k % 37 != 0 {
                continue;
            }
            let f = nav.fix(r);
            for side in [-1.0f64, 1.0] {
                let (lat, lon) = geo::offset_m(f.lat, f.lon, f.bearing + 90.0, side * across);
                if let Some(v) = m.sample(lat, lon) {
                    sum += v as u64;
                    n += 1;
                }
            }
        }
        sum as f64 / n.max(1) as f64
    };

    // A metre off the track: seabed on the ground axis, water on the slant one.
    // Well outside the column both axes are looking at the same seabed, which
    // is the control -- it says the two builds are otherwise comparable.
    let inner = 1.0;
    let outer = alt + 8.0;
    let (gi, go) = (level(Axis::Ground, inner), level(Axis::Ground, outer));
    let (si, so) = (level(Axis::Slant, inner), level(Axis::Slant, outer));
    eprintln!("altitude {alt:.1} m");
    eprintln!("  ground: {inner:.0} m in {gi:6.1}   {outer:.0} m out {go:6.1}");
    eprintln!("  slant : {inner:.0} m in {si:6.1}   {outer:.0} m out {so:6.1}");

    assert!(go > 1.0 && so > 1.0, "nothing was painted out at {outer:.0} m");
    assert!(
        gi > 0.5 * go,
        "the ground axis has a dark band at nadir ({gi:.1} against {go:.1} outside); \
         the water column should not be there at all"
    );
    assert!(
        si < 0.5 * gi,
        "the slant axis is as bright at nadir as the ground axis ({si:.1} against \
         {gi:.1}); the water column is not being kept"
    );
}

/// A contact's two band crops have to come from the same instant.
///
/// The sonar crop for each band is taken from a block fetched around the ping
/// the contact was seen on, and that ping used to be found by asking which
/// fish position was nearest. It is the wrong question: a contact sits up to a
/// swath off the track, so every row for tens of metres either side is very
/// nearly equidistant from it, and on a recording that loops over itself the
/// answer can be a row from an entirely different pass -- the right seabed from
/// the wrong look. `Waterfall::world_to_pixel` says so in its own docstring,
/// which is why it asks which row has the point abeam instead.
///
/// The time is unambiguous, and the two subsystems ping together, so it picks
/// the same moment in either band.
#[test]
fn a_time_resolves_the_same_moment_in_both_bands() {
    let Some(idx) = index() else {
        eprintln!("no index for {DATASET}; skipping");
        return;
    };
    let subs = idx.subsystems();
    if subs.len() < 2 {
        eprintln!("{DATASET} has one band; skipping");
        return;
    }
    let (lo, hi) = (subs[0], subs[subs.len() - 1]);
    let bands = waterfall::pair_bands(&idx);
    let a = &bands[&lo];
    let b = &bands[&hi];
    assert!(a.len() > 500 && b.len() > 500, "not enough pings to test with");

    // The rows belong to the recording, not to a band. One fish, one instant
    // per row, so the bands cannot be different lengths -- which is what the
    // panes beside each other depend on.
    assert_eq!(a.len(), b.len(), "the two bands came out different lengths");

    let at = |rows: &[waterfall::Row], i: usize| idx.records[rows[i].at as usize].time;
    // The ping interval, so "the same moment" has a scale.
    let dt = (at(a, a.len() - 1) - at(a, 0)) / (a.len() - 1) as f64;

    let mut worst = 0.0f64;
    for k in [0, 10, a.len() / 4, a.len() / 2, (3 * a.len()) / 4, a.len() - 1] {
        let t = at(a, k);
        // Exact on its own channel: this is the row that time came from.
        let (same, off) = waterfall::row_at_time(&idx, a, t).expect("a row on the low band");
        assert_eq!(same, k, "a time did not resolve to the row it came from");
        assert!(off <= 1e-6, "and it was {off} s out");

        // And the *same row number* on the other one, not merely a nearby one.
        // Pairing each band by its own ping counter used to leave these a whole
        // ping apart for the length of `080929_demimines`.
        let (other, off) = waterfall::row_at_time(&idx, b, t).expect("a row on the high band");
        worst = worst.max(off);
        assert_eq!(other, k, "row {k} is a different moment in the two bands");
        assert!(off <= 1e-6, "and the two bands' row {k} were {off:.4} s apart");
    }

    // Every row of every band is the same instant, not just the ones sampled.
    let mut absent = 0usize;
    for (i, (ra, rb)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(ra.at, rb.at, "row {i} points at two different pings");
        absent += usize::from(ra.own().is_none()) + usize::from(rb.own().is_none());
    }
    eprintln!(
        "ping interval {dt:.4} s, {} rows in both bands, {absent} band-rows with no ping, \
         worst cross-band disagreement {worst:.6} s",
        a.len()
    );
}

/// Every recording in the workspace, not just the fixture one: the bands share
/// a row space, and a row is the same instant in all of them.
///
/// This is the property two waterfall panes stand on. It cannot be checked on
/// one recording, because the way it used to break depended on how the file
/// happened to start and stop: `070926_measures_b2` came out aligned by luck,
/// `080929_demimines` came out a whole ping out of step from end to end, and
/// `070926_measures` came out as two bands of different lengths -- 30071 rows
/// against 30012 -- because the high band dropped 62 pings along the way.
#[test]
fn every_recording_gives_its_bands_one_row_space() {
    let dir = root().join("out");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        eprintln!("no out/ to read; skipping");
        return;
    };
    let mut checked = 0;
    for e in rd.flatten() {
        let name = e.file_name();
        let p = Workspace::new(root()).index_path(&name.to_string_lossy());
        if !p.exists() {
            continue;
        }
        let Ok(idx) = PingIndex::load(&p) else { continue };
        let bands = waterfall::pair_bands(&idx);
        if bands.len() < 2 {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let lens: Vec<usize> = bands.values().map(|v| v.len()).collect();
        assert!(
            lens.windows(2).all(|w| w[0] == w[1]),
            "{name}: bands came out {lens:?} rows long; they were one fish at one instant"
        );

        // Row i is the same ping in every band, and every row has a fish.
        let first = bands.values().next().unwrap();
        let mut blank = 0usize;
        for v in bands.values() {
            for (i, row) in v.iter().enumerate() {
                assert_eq!(row.at, first[i].at, "{name}: row {i} is two different instants");
                blank += usize::from(row.own().is_none());
            }
        }

        // The rows are in time order.
        let t = |i: usize| idx.records[first[i].at as usize].time;
        let mut gaps: Vec<f64> = (1..first.len()).map(|i| t(i) - t(i - 1)).collect();
        assert!(gaps.iter().all(|g| *g > 0.0), "{name}: rows are not in time order");
        gaps.sort_by(f64::total_cmp);
        let median = gaps[gaps.len() / 2];

        // And each band's row `i` is that band's part of *this* cycle rather
        // than of a neighbouring one. A ping rate is not regular enough to
        // check by counting -- `wpa20260906` runs from 15 ms to 690 ms between
        // pings -- so the check is that the bands never disagree by as much as
        // a whole cycle. That recording staggers its bands by 42 ms; the rest
        // fire them together.
        let stamp = |v: &Vec<waterfall::Row>, i: usize| {
            v[i].own().map(|j| idx.records[j as usize].time)
        };
        let mut stagger: f64 = 0.0;
        for v in bands.values() {
            for i in 0..first.len() {
                let (Some(a), Some(b)) = (stamp(first, i), stamp(v, i)) else { continue };
                stagger = stagger.max((a - b).abs());
            }
        }
        assert!(
            stagger < median,
            "{name}: two bands' row is {stagger:.4} s apart, which is more than the \
             {median:.4} s between pings -- they are on different cycles"
        );
        eprintln!(
            "{name}: {} rows x {} bands, ping {median:.4} s, band stagger {stagger:.4} s, \
             {blank} band-rows with no ping",
            first.len(),
            bands.len()
        );
        checked += 1;
    }
    assert!(checked > 0, "no dual-band recording indexed; nothing was checked");
}
