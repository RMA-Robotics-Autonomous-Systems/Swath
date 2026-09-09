//! Survey planning: the box, the spacing, the lines and the turns between them.
//!
//! Every number here is checked against what the geometry promises rather than
//! against a recorded output, so a change that moves a line has to argue with
//! the promise instead of with a golden file.

use swath_core::gpx;
use swath_core::plan::{self, Direction, Order, PlanSpec, Regime, Target, Verdict};

/// Three marks on the seabed off Zeebrugge, about a kilometre apart.
fn targets() -> Vec<Target> {
    vec![
        Target { id: "1".into(), name: "DM-01".into(), lat: 51.42050, lon: 3.14800, radius_m: 60.0 },
        Target { id: "2".into(), name: "DM-02".into(), lat: 51.42320, lon: 3.15650, radius_m: 45.0 },
        Target { id: "3".into(), name: "DM-03".into(), lat: 51.41880, lon: 3.16250, radius_m: 80.0 },
    ]
}

fn spec() -> PlanSpec {
    PlanSpec::default()
}

#[test]
fn the_frame_is_its_own_inverse() {
    let f = plan::Frame::about(51.42, 3.155);
    let (e, n) = f.fwd(51.4232, 3.1565);
    let (lat, lon) = f.inv(e, n);
    assert!((lat - 51.4232).abs() < 1e-9 && (lon - 3.1565).abs() < 1e-9, "{lat} {lon}");
}

#[test]
fn a_plan_position_reads_back_where_it_was_put() {
    let p = plan::solve(&spec(), &targets(), 37.0).unwrap();
    // Every line's own start, through the plan frame and back out again.
    for r in &p.runs {
        let (lat, lon) = p.ll(r.a_start, r.x);
        let (e, n) = p.frame.fwd(lat, lon);
        let th = p.azimuth_deg * std::f64::consts::PI / 180.0;
        let (a, x) = (e * th.sin() + n * th.cos(), e * th.cos() - n * th.sin());
        assert!((a - r.a_start).abs() < 1e-6 && (x - r.x).abs() < 1e-6, "{a} {x}");
    }
}

/// The three spacing regimes have to mean what they are named.
#[test]
fn each_regime_delivers_what_it_promises() {
    let t = targets();

    let mut s = spec();
    s.sonar.regime = Regime::Full;
    let p = plan::solve(&s, &t, 45.0).unwrap();
    assert_eq!(p.coverage.none, 0.0, "nadir filled must leave no holes");

    s.sonar.regime = Regime::Double;
    let p = plan::solve(&s, &t, 45.0).unwrap();
    assert!(p.coverage.worst >= 2, "double coverage saw a point {} times", p.coverage.worst);

    s.sonar.regime = Regime::Recon;
    let p = plan::solve(&s, &t, 45.0).unwrap();
    assert!(p.coverage.none > 0.0, "one pass leaves the nadir open, and should say so");
}

/// A line array that stops at the box edge is one look short there: the
/// outermost line's own nadir gap sits on the boundary with nothing beyond it.
#[test]
fn double_coverage_reaches_the_edge_of_the_box() {
    let p = plan::solve(&spec(), &targets(), 45.0).unwrap();
    assert_eq!(p.outer_lines, 2, "one line beyond each side");
    assert!(p.lines > p.outer_lines);
    // Take them away and the promise breaks -- which is why they are there.
    let inner: Vec<f64> = p.lines_x[1..p.lines_x.len() - 1].to_vec();
    let edge_passes = inner
        .iter()
        .filter(|&&xl| {
            let d = (p.x0 - xl).abs();
            d >= p.nadir_m && d <= p.range_m
        })
        .count();
    assert!(edge_passes < 2, "without the outer lines the edge only gets {edge_passes}");
}

/// The waypoints steer the antenna, so the line is stretched to put the *fish*
/// over the box, and the stretch is not symmetric.
#[test]
fn lines_are_stretched_for_the_layback_not_the_boat() {
    let s = spec();
    let p = plan::solve(&s, &targets(), 45.0).unwrap();
    let off = s.rig.offset_m();
    assert!((off - 54.0).abs() < 1e-9, "44 m of layback behind a tow point 10 m aft of the GPS");
    for r in &p.runs {
        // The whole line costs the box plus a run-in and a run-out, and
        // nothing more.
        assert!(
            (r.length_m - ((p.a1 - p.a0) + s.rig.run_in_m + s.rig.run_out_m)).abs() < 1e-6,
            "line {} is {} m",
            r.line,
            r.length_m
        );
        let (near, far) = if r.dir > 0.0 { (p.a0, p.a1) } else { (p.a1, p.a0) };
        // The legs are the lengths asked for, measured from the box edges. An
        // operator who types 150 gets 150 m of line before the box, not 150
        // less a layback they have to work out for themselves.
        assert!((r.a_start - (near - r.dir * s.rig.run_in_m)).abs() < 1e-6, "the run-in leg");
        assert!((r.a_end - (far + r.dir * s.rig.run_out_m)).abs() < 1e-6, "the run-out leg");
        // Recording starts with the fish exactly on the near edge, and stops
        // with it exactly on the far one -- which the default run-out is long
        // enough to reach.
        let fish_on = r.a_on - r.dir * off;
        let fish_off = r.a_off - r.dir * off;
        assert!((fish_on - near).abs() < 1e-6, "fish at {fish_on}, box edge {near}");
        assert!((fish_off - far).abs() < 1e-6, "fish at {fish_off}, box edge {far}");
    }
    assert_eq!(p.fish_short_m, 0.0, "the default run-out clears the default layback");
}

/// The run-in and the run-out are two settings, and each one only moves its
/// own end of the line.
///
/// They were one number for a while, with the run-out pinned to the offset --
/// which is the part that is not a choice. Asking for straight running after
/// the fish is clear is a different question from asking for it before the
/// fish arrives, and the boat pays for them at opposite ends.
#[test]
fn the_run_in_and_the_run_out_are_separate() {
    let base = plan::solve(&spec(), &targets(), 45.0).unwrap();
    let off = spec().rig.offset_m();

    // A longer run-out moves the far end of the leg and nothing else.
    let mut s = spec();
    s.rig.run_out_m = 140.0;
    let p = plan::solve(&s, &targets(), 45.0).unwrap();
    for (r, b) in p.runs.iter().zip(base.runs.iter()) {
        let far = if r.dir > 0.0 { p.a1 } else { p.a0 };
        assert!((r.a_start - b.a_start).abs() < 1e-6, "the run-out moved the start of a line");
        assert!((r.a_on - b.a_on).abs() < 1e-6, "the run-out moved where data starts");
        assert!((r.a_off - (far + r.dir * off)).abs() < 1e-6, "the run-out moved where data stops");
        assert!((r.a_end - (far + r.dir * 140.0)).abs() < 1e-6, "the leg ends where it was asked to");
        assert!((r.length_m - (b.length_m + 80.0)).abs() < 1e-6);
    }

    // A longer run-in moves the near end and nothing else.
    let mut t = spec();
    t.rig.run_in_m += 60.0;
    let q = plan::solve(&t, &targets(), 45.0).unwrap();
    for (r, b) in q.runs.iter().zip(base.runs.iter()) {
        assert!((r.a_start - (b.a_start - r.dir * 60.0)).abs() < 1e-6, "the run-in owns the start");
        assert!((r.a_end - b.a_end).abs() < 1e-6, "the run-in moved the end of a line");
    }

    // The turn starts where the leg ends, not where the data stops.
    assert!(
        (p.turns[0].points[0][0] - p.runs[0].a_end).abs() < 1e-6,
        "the turn starts at {} and the leg ends at {}",
        p.turns[0].points[0][0],
        p.runs[0].a_end
    );

    // And what is drawn is what was asked for, to the metre.
    for (kind, want) in [("runin", spec().rig.run_in_m), ("runout", 140.0)] {
        let drawn = drawn_lengths(&p, kind);
        assert_eq!(drawn.len(), p.runs.len(), "one {kind} a line");
        for d in drawn {
            assert!((d - want).abs() < 0.5, "a {kind} is drawn {d:.1} m long, {want:.0} was asked for");
        }
    }
}

/// A run-out shorter than the layback does not shorten the leg by a little.
/// It leaves the fish inside the box when the wheel goes over, and that is
/// box the line crosses and does not survey.
#[test]
fn a_short_run_out_leaves_the_box_unfinished() {
    let mut s = spec();
    s.rig.run_out_m = 20.0;
    let p = plan::solve(&s, &targets(), 45.0).unwrap();
    let off = s.rig.offset_m();
    let short = off - 20.0;
    assert!((p.fish_short_m - short).abs() < 1e-9, "short by {}", p.fish_short_m);

    for r in &p.runs {
        let far = if r.dir > 0.0 { p.a1 } else { p.a0 };
        // The leg still ends exactly where it was told to.
        assert!((r.a_end - (far + r.dir * 20.0)).abs() < 1e-6);
        // Recording stops with the leg, not at the far edge, because the fish
        // never gets there.
        assert!((r.a_off - r.a_end).abs() < 1e-6, "data outlasts the leg");
        let fish_off = r.a_off - r.dir * off;
        assert!((fish_off - (far - r.dir * short)).abs() < 1e-6, "the fish stops {short} m short");
    }

    // Drawn: the line covers the box less the shortfall, the rest is marked as
    // crossed-not-surveyed, and that mark is inside the box where the hole is.
    let box_len = p.a1 - p.a0;
    for d in drawn_lengths(&p, "line") {
        assert!((d - (box_len - short)).abs() < 0.5, "a line is drawn {d:.1} m long");
    }
    let shorts = drawn_lengths(&p, "short");
    assert_eq!(shorts.len(), p.runs.len(), "one shortfall a line");
    for d in &shorts {
        assert!((d - short).abs() < 0.5, "a shortfall is drawn {d:.1} m long");
    }
    for f in p.to_geojson()["features"].as_array().unwrap() {
        if f["properties"]["kind"] != "short" {
            continue;
        }
        for c in f["geometry"]["coordinates"].as_array().unwrap() {
            let (a, _) = along_across(&p, c[1].as_f64().unwrap(), c[0].as_f64().unwrap());
            assert!(a > p.a0 - 0.5 && a < p.a1 + 0.5, "the shortfall is outside the box");
        }
    }

    // Enough run-out and there is nothing to mark.
    let mut ok = spec();
    ok.rig.run_out_m = off;
    let q = plan::solve(&ok, &targets(), 45.0).unwrap();
    assert_eq!(q.fish_short_m, 0.0);
    assert!(drawn_lengths(&q, "short").is_empty(), "a sound plan draws no shortfall");
}

/// How long each feature of one kind is drawn, in metres of the plan frame.
fn drawn_lengths(p: &plan::Plan, kind: &str) -> Vec<f64> {
    p.to_geojson()["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["properties"]["kind"] == kind && f["geometry"]["type"] == "LineString")
        .map(|f| {
            let cs: Vec<(f64, f64)> = f["geometry"]["coordinates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| along_across(p, c[1].as_f64().unwrap(), c[0].as_f64().unwrap()))
                .collect();
            cs.windows(2).map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1)).sum()
        })
        .collect()
}

#[test]
fn lines_run_in_order_and_the_heading_alternates() {
    let p = plan::solve(&spec(), &targets(), 45.0).unwrap();
    assert_eq!(p.skip, 1, "the default is 1, 2, 3");
    for (i, r) in p.runs.iter().enumerate() {
        assert_eq!(r.line, i, "line {} ran {}th", r.line, i);
        if i > 0 {
            assert!(
                (r.dir + p.runs[i - 1].dir).abs() < 1e-9,
                "line {i} runs the same way as the one before it"
            );
        }
    }
    assert_eq!(p.turns.len(), p.runs.len() - 1, "one turn between each pair");
    let headings: Vec<f64> = p.runs.iter().map(|r| r.heading_deg).collect();
    assert!((headings[0] - 45.0).abs() < 1e-9 && (headings[1] - 225.0).abs() < 1e-9, "{headings:?}");
}

/// Turns have to start where the last line ended and finish where the next one
/// begins, and never bend tighter than the boat can.
#[test]
fn turns_join_the_lines_and_honour_the_radius() {
    let s = spec();
    let p = plan::solve(&s, &targets(), 45.0).unwrap();
    for (i, t) in p.turns.iter().enumerate() {
        let (a, b) = (&p.runs[i], &p.runs[i + 1]);
        let first = t.points.first().unwrap();
        let last = t.points.last().unwrap();
        assert!(
            (first[0] - a.a_end).abs() < 1e-6 && (first[1] - a.x).abs() < 1e-6,
            "turn {i} starts away from the end of line {}",
            a.line
        );
        assert!(
            (last[0] - b.a_start).abs() < 1e-6 && (last[1] - b.x).abs() < 1e-6,
            "turn {i} ends away from the start of line {}",
            b.line
        );
        // Menger curvature over each triple: the circumradius of three
        // consecutive points is the radius the boat is holding there.
        for w in t.points.windows(3) {
            let (p0, p1, p2) = (w[0], w[1], w[2]);
            let la = (p1[0] - p0[0]).hypot(p1[1] - p0[1]);
            let lb = (p2[0] - p1[0]).hypot(p2[1] - p1[1]);
            let lc = (p2[0] - p0[0]).hypot(p2[1] - p0[1]);
            let area = ((p1[0] - p0[0]) * (p2[1] - p0[1]) - (p2[0] - p0[0]) * (p1[1] - p0[1])).abs() / 2.0;
            if area > 1e-9 && la > 0.5 && lb > 0.5 {
                let r = la * lb * lc / (4.0 * area);
                assert!(r > s.rig.turn_radius_m * 0.9, "turn {i} bends to {r:.1} m");
            }
        }
    }
}

/// With less room across track than a semicircle needs, the turn has to loop
/// out -- and the plan has to say how far, because that is water the helm needs.
#[test]
fn a_turn_too_tight_for_the_spacing_becomes_a_teardrop() {
    let mut s = spec();
    s.rig.turn_radius_m = 150.0;
    s.sonar.regime = Regime::Custom;
    s.sonar.spacing_m = 40.0;

    s.order = Order::Sequential;
    let seq = plan::solve(&s, &targets(), 45.0).unwrap();
    assert_eq!(seq.skip, 1);
    assert!(seq.teardrops > 0, "150 m radius will not fit between 40 m lines");
    assert!(seq.overshoot_m > 100.0, "and it reaches {} m past the ends", seq.overshoot_m);

    // Running every k-th line and filling in after buys the room back.
    s.order = Order::Skip;
    let skip = plan::solve(&s, &targets(), 45.0).unwrap();
    assert!(skip.skip >= (2.0 * 150.0 / skip.spacing_m).ceil() as usize);
    assert_eq!(skip.teardrops, 0, "every turn should be a clean semicircle now");
}

#[test]
fn one_way_running_costs_about_double() {
    let t = targets();
    let mut s = spec();
    s.direction = Direction::Alternate;
    let both = plan::solve(&s, &t, 45.0).unwrap();
    s.direction = Direction::OneWay;
    let one = plan::solve(&s, &t, 45.0).unwrap();
    assert!(
        one.distance_m > both.distance_m * 1.6,
        "{:.1} km against {:.1} km",
        one.distance_m / 1000.0,
        both.distance_m / 1000.0
    );
    assert!(one.runs.iter().all(|r| r.dir > 0.0), "every line the same way");
}

/// A box run at 210 degrees is the box run at 30, turned. Generating past 90 is
/// generating the same plans again.
#[test]
fn an_azimuth_and_its_opposite_are_the_same_plan() {
    let t = targets();
    let a = plan::solve(&spec(), &t, 30.0).unwrap();
    let b = plan::solve(&spec(), &t, 210.0).unwrap();
    assert_eq!(a.lines, b.lines);
    assert!((a.distance_m - b.distance_m).abs() < 1.0, "{} vs {}", a.distance_m, b.distance_m);
    assert!((a.coverage.none - b.coverage.none).abs() < 1e-9);
}

#[test]
fn the_quadrant_is_every_plan_the_box_has() {
    let mut s = spec();
    s.step_deg = 10.0;
    let rows = plan::quadrant(&s, &targets());
    assert_eq!(rows.len(), 10, "0 to 90 every 10 degrees");
    assert!((rows[0].azimuth_deg - 0.0).abs() < 1e-9);
    assert!((rows[9].azimuth_deg - 90.0).abs() < 1e-9);
    assert!(rows.iter().all(|r| r.lines > 0 && r.distance_m > 0.0));
    assert!(rows.iter().all(|r| r.worst == Verdict::Good), "double coverage should find them all");
}

#[test]
fn targets_report_the_looks_they_actually_get() {
    let t = targets();
    let p = plan::solve(&spec(), &t, 45.0).unwrap();
    assert_eq!(p.targets.len(), 3);
    for look in &p.targets {
        assert!(look.looks >= 2, "{} got {} looks", look.name, look.looks);
        assert!(look.both_aspects, "{} was only seen from one side", look.name);
        assert_eq!(look.verdict, Verdict::Good);
    }

    // A range far too short for the spacing must be reported as a miss, not
    // quietly rounded into a pass.
    let mut s = spec();
    s.sonar.regime = Regime::Custom;
    s.sonar.spacing_m = 900.0;
    let p = plan::solve(&s, &t, 45.0).unwrap();
    assert!(p.targets.iter().any(|l| l.verdict != Verdict::Good), "{:?}", p.targets);
}

/// The written plan has to survive being read back by the same code that reads
/// a plotter's own files.
#[test]
fn a_written_plan_reads_back_as_itself() {
    let s = spec();
    let t = targets();
    let p = plan::solve(&s, &t, 45.0).unwrap();
    // Everything on, so the round trip has something of each kind to carry.
    let opts = plan::GpxOptions { routes: true, line_waypoints: true, ..Default::default() };
    let g = p.to_gpx(&s, &t, &opts);
    let text = gpx::write(&g);
    let back = gpx::parse(&text).expect("the writer must produce something the reader accepts");

    assert_eq!(back.name, g.name);
    assert_eq!(back.desc, g.desc, "the settings digest has to survive the round trip");
    assert_eq!(back.waypoints.len(), g.waypoints.len());
    assert_eq!(back.routes.len(), 1);
    assert_eq!(back.routes[0].points.len(), p.runs.len() * 2);
    for (a, b) in g.waypoints.iter().zip(&back.waypoints) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.desc, b.desc);
        assert!((a.lat - b.lat).abs() < 1e-7 && (a.lon - b.lon).abs() < 1e-7);
    }
    for (a, b) in g.routes[0].points.iter().zip(&back.routes[0].points) {
        assert_eq!(a.name, b.name, "a route point without its name is an anonymous polyline");
        assert!((a.lat - b.lat).abs() < 1e-7 && (a.lon - b.lon).abs() < 1e-7);
    }
}

/// The positions in the file have to be the plan, not merely near it.
#[test]
fn the_written_positions_are_the_lines_that_were_solved() {
    let s = spec();
    let t = targets();
    let p = plan::solve(&s, &t, 45.0).unwrap();
    let opts = plan::GpxOptions { routes: true, ..Default::default() };
    let g = p.to_gpx(&s, &t, &opts);
    let pts = &g.routes[0].points;

    let mut across: Vec<f64> = Vec::new();
    for leg in pts.chunks(2) {
        let (e0, n0) = p.frame.fwd(leg[0].lat, leg[0].lon);
        let (e1, n1) = p.frame.fwd(leg[1].lat, leg[1].lon);
        let th = p.azimuth_deg * std::f64::consts::PI / 180.0;
        let x0 = e0 * th.cos() - n0 * th.sin();
        let x1 = e1 * th.cos() - n1 * th.sin();
        assert!((x0 - x1).abs() < 0.02, "a line that is not straight: {x0} to {x1}");

        // The bearing between the two ends is the plan's own azimuth.
        let brg = swath_core::geo::initial_bearing(leg[0].lat, leg[0].lon, leg[1].lat, leg[1].lon);
        let want = if across.len() % 2 == 0 { 45.0 } else { 225.0 };
        assert!((brg - want).abs() < 0.2, "steered {brg:.2}, planned {want}");
        across.push(x0);
    }
    across.sort_by(f64::total_cmp);
    for w in across.windows(2) {
        assert!(
            (w[1] - w[0] - p.spacing_m).abs() < 0.05,
            "lines {:.2} m apart, spacing says {:.2}",
            w[1] - w[0],
            p.spacing_m
        );
    }
}

#[test]
fn the_preview_geojson_covers_what_the_chart_needs() {
    let p = plan::solve(&spec(), &targets(), 45.0).unwrap();
    let gj = p.to_geojson();
    let feats = gj["features"].as_array().unwrap();
    let kinds: Vec<&str> =
        feats.iter().filter_map(|f| f["properties"]["kind"].as_str()).collect();
    for want in ["box", "swath", "runin", "runout", "line", "turn"] {
        assert!(kinds.contains(&want), "no {want} in the preview");
    }
    assert_eq!(kinds.iter().filter(|k| **k == "line").count(), p.runs.len());
    assert!(!kinds.contains(&"gap"), "double coverage should draw no gaps");
    for f in feats {
        assert!(f["geometry"]["coordinates"].is_array());
    }
}

/// What is inside the box is line, and only line.
///
/// The run-in, the run-out and the turns are how the boat gets onto the next
/// line; none of them is survey, and none of them belongs inside the rectangle
/// the plan is measured against. Drawing them there is not a cosmetic mistake:
/// it reads as a plan whose lines do not reach the edges they actually cover.
#[test]
fn only_the_lines_are_inside_the_box() {
    for (az, run_out) in [(0.0, 60.0), (30.0, 54.0), (45.0, 120.0), (90.0, 300.0)] {
        let mut s = spec();
        s.rig.run_out_m = run_out;
        let p = plan::solve(&s, &targets(), az).unwrap();
        let gj = p.to_geojson();
        let tol = 0.5;
        for f in gj["features"].as_array().unwrap() {
            let kind = f["properties"]["kind"].as_str().unwrap_or("");
            if f["geometry"]["type"] != "LineString" {
                continue;
            }
            for c in f["geometry"]["coordinates"].as_array().unwrap() {
                let (lon, lat) = (c[0].as_f64().unwrap(), c[1].as_f64().unwrap());
                let (a, _) = along_across(&p, lat, lon);
                match kind {
                    "line" => assert!(
                        a > p.a0 - tol && a < p.a1 + tol,
                        "{az}\u{b0}: a line reaches {:.1} m outside the box",
                        (p.a0 - a).max(a - p.a1)
                    ),
                    "runin" | "runout" | "turn" => assert!(
                        a < p.a0 + tol || a > p.a1 - tol,
                        "{az}\u{b0}: a {kind} runs {:.1} m into the box",
                        (a - p.a0).min(p.a1 - a)
                    ),
                    _ => {}
                }
            }
        }
        // And the lines fill it: every one spans the box end to end.
        for f in gj["features"].as_array().unwrap() {
            if f["properties"]["kind"] != "line" {
                continue;
            }
            let c = f["geometry"]["coordinates"].as_array().unwrap();
            let (a0, _) = along_across(&p, c[0][1].as_f64().unwrap(), c[0][0].as_f64().unwrap());
            let (a1, _) = along_across(&p, c[1][1].as_f64().unwrap(), c[1][0].as_f64().unwrap());
            assert!(
                ((a1 - a0).abs() - (p.a1 - p.a0)).abs() < 0.5,
                "{az}\u{b0}: a drawn line is {:.1} m long, the box is {:.1}",
                (a1 - a0).abs(),
                p.a1 - p.a0
            );
        }
    }
}

/// A position back into the plan frame: along-track, across-track.
fn along_across(p: &plan::Plan, lat: f64, lon: f64) -> (f64, f64) {
    let (e, n) = p.frame.fwd(lat, lon);
    let th = p.azimuth_deg.to_radians();
    let (su, cu) = (th.sin(), th.cos());
    (e * su + n * cu, e * cu - n * su)
}

/// GPX cannot express an arc, so a turn is points -- but they belong in a track,
/// where they cost nothing, rather than in a route a plotter steers by.
///
/// The default file is a trace and nothing else: plenty of plotters take a route
/// or a track but not both, and the shape to follow is the more useful of the
/// two to a helm steering by hand.
#[test]
fn the_default_file_is_a_trace() {
    let s = spec();
    let t = targets();
    let p = plan::solve(&s, &t, 45.0).unwrap();
    let g = p.to_gpx(&s, &t, &plan::GpxOptions::default());

    assert!(g.routes.is_empty(), "a route would compete with the track for the one slot");
    assert_eq!(g.tracks.len(), 1, "one track for the whole path");
    assert_eq!(
        g.waypoints.len(),
        t.len(),
        "the targets, and not sixty-nine line marks nobody asked for"
    );

    let path = &g.tracks[0].points;
    assert!(path.len() > p.runs.len() * 2, "the track has to carry the curves as well");

    // Thinned, not raw: the chart draws the arcs far finer than anything can be
    // steered to, and a plotter should not be asked to hold all of it.
    let raw: usize = p.turns.iter().map(|t| t.points.len()).sum();
    assert!(
        path.len() < p.runs.len() * 2 + raw / 2,
        "{} points against {} raw turn points",
        path.len(),
        raw
    );

    // Continuous: consecutive points never jump further than a line is long.
    let longest = p.runs[0].length_m * 1.05;
    for w in path.windows(2) {
        let d = swath_core::geo::distance_m(w[0].lat, w[0].lon, w[1].lat, w[1].lon);
        assert!(d <= longest, "a {d:.0} m gap in the drawn path");
    }

    // And it still follows the real curve. Every point of the solved turn has to
    // sit close to the thinned line that replaced it.
    let frame = p.frame;
    let xy: Vec<[f64; 2]> = path
        .iter()
        .map(|q| {
            let (e, n) = frame.fwd(q.lat, q.lon);
            let th = p.azimuth_deg * std::f64::consts::PI / 180.0;
            [e * th.sin() + n * th.cos(), e * th.cos() - n * th.sin()]
        })
        .collect();
    let mut worst: f64 = 0.0;
    for turn in &p.turns {
        for q in &turn.points {
            let mut best = f64::INFINITY;
            for w in xy.windows(2) {
                best = best.min(point_to_segment(*q, w[0], w[1]));
            }
            worst = worst.max(best);
        }
    }
    assert!(worst <= 1.05, "the drawn path strays {worst:.2} m from the turn it stands for");
}

/// Asked for, a route is still sparse: every route point is a waypoint the
/// plotter sequences and sounds an arrival alarm at.
#[test]
fn a_route_stays_two_points_a_line() {
    let s = spec();
    let t = targets();
    let p = plan::solve(&s, &t, 45.0).unwrap();

    let g = p.to_gpx(&s, &t, &plan::GpxOptions { routes: true, ..Default::default() });
    assert_eq!(g.routes.len(), 1);
    assert_eq!(g.routes[0].points.len(), p.runs.len() * 2);
    assert_eq!(g.tracks.len(), 1, "the trace is still there when both are wanted");

    let per = plan::GpxOptions { routes: true, route_per_line: true, ..Default::default() };
    let g = p.to_gpx(&s, &t, &per);
    assert_eq!(g.routes.len(), p.runs.len(), "one route each, for a plotter that wants that");
    assert!(g.routes.iter().all(|r| r.points.len() == 2));

    // Line marks are opt-in: three per line is sixty-nine pins on the screen.
    let marked = plan::GpxOptions { line_waypoints: true, ..Default::default() };
    let g = p.to_gpx(&s, &t, &marked);
    assert_eq!(g.waypoints.len(), t.len() + p.runs.len() * 3);

    let bare = p.to_gpx(&s, &t, &plan::GpxOptions { track: false, ..Default::default() });
    assert!(bare.tracks.is_empty());
}

fn point_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (vx, vy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = vx * vx + vy * vy;
    if len2 < 1e-12 {
        return (p[0] - a[0]).hypot(p[1] - a[1]);
    }
    let t = (((p[0] - a[0]) * vx + (p[1] - a[1]) * vy) / len2).clamp(0.0, 1.0);
    (p[0] - (a[0] + t * vx)).hypot(p[1] - (a[1] + t * vy))
}
