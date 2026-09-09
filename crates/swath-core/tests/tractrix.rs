//! The pursuit curve, checked against the cases where it has a closed form.
//!
//! A numerical integrator that is only ever compared with itself will happily
//! converge on the wrong curve. These drive synthetic tracks whose answer is
//! known analytically: a straight run, where the fish must end up exactly `L`
//! behind on the line, and a steady turn, where a taut cable of length `L`
//! behind a vessel on radius `R` settles on radius `sqrt(R^2 - L^2)`.

use swath_core::geo;
use swath_core::index::PingRecord;
use swath_core::nav::{LaybackModel, Nav, NavConfig};

const LAT0: f64 = 52.5;
const LON0: f64 = 4.0;

/// Build ping records from a metric path about (LAT0, LON0), 1 Hz.
fn records(xy: &[(f64, f64)]) -> Vec<PingRecord> {
    let (m_lat, m_lon) = geo::local_scale(LAT0);
    xy.iter()
        .enumerate()
        .map(|(i, &(x, y))| PingRecord {
            time: 1_700_000_000.0 + i as f64,
            lat: LAT0 + y / m_lat,
            lon: LON0 + x / m_lon,
            ..Default::default()
        })
        .collect()
}

fn cfg(model: LaybackModel, layback: f64) -> NavConfig {
    NavConfig {
        // Zero the lever arm so the geometry under test is only the cable.
        gps_to_towpoint_m: 0.0,
        layback_m: layback,
        model,
        ..Default::default()
    }
}

/// Metres from (LAT0, LON0).
fn to_xy(lat: f64, lon: f64) -> (f64, f64) {
    let (m_lat, m_lon) = geo::local_scale(LAT0);
    ((lon - LON0) * m_lon, (lat - LAT0) * m_lat)
}

#[test]
fn on_a_straight_run_the_fish_trails_exactly_astern() {
    let l = 48.0;
    // due north at 2.6 m/s for 20 minutes: long enough to forget the start
    let path: Vec<(f64, f64)> = (0..1200).map(|i| (0.0, i as f64 * 2.6)).collect();
    let recs = records(&path);
    let nav = Nav::build(&recs, cfg(LaybackModel::Tractrix, l));

    let last = nav.fix(&recs[recs.len() - 1]);
    let (fx, fy) = to_xy(last.lat, last.lon);
    let (bx, by) = to_xy(last.boat_lat, last.boat_lon);
    let behind = by - fy;
    eprintln!("straight run: {:.4} m astern, {:.4} m off the line", behind, fx - bx);
    assert!((behind - l).abs() < 0.01, "trailed {behind:.3} m, wanted {l}");
    assert!((fx - bx).abs() < 0.01, "wandered {:.3} m off the line", fx - bx);
}

#[test]
fn in_a_steady_turn_the_fish_settles_on_the_analytic_radius() {
    let l = 48.0;
    // A taut cable of length L behind a vessel on radius R rides on
    // sqrt(R^2 - L^2): the line to the tow point is tangent to the fish's own
    // circle, because that is the direction the fish is travelling.
    for r in [300.0f64, 150.0, 90.0] {
        let step = 2.6 / r; // radians per second at 2.6 m/s
        // six laps, so the transient is long gone
        let n = (6.0 * std::f64::consts::TAU / step) as usize;
        let path: Vec<(f64, f64)> =
            (0..n).map(|i| {
                let a = i as f64 * step;
                (r * a.sin(), r * a.cos())
            }).collect();
        let recs = records(&path);
        let nav = Nav::build(&recs, cfg(LaybackModel::Tractrix, l));

        // measure over the final lap
        let from = recs.len() - (std::f64::consts::TAU / step) as usize;
        let radii: Vec<f64> = recs[from..]
            .iter()
            .map(|rec| {
                let f = nav.fix(rec);
                let (x, y) = to_xy(f.lat, f.lon);
                x.hypot(y)
            })
            .collect();
        let mean = radii.iter().sum::<f64>() / radii.len() as f64;
        let spread = radii.iter().cloned().fold(f64::MIN, f64::max)
            - radii.iter().cloned().fold(f64::MAX, f64::min);
        let want = (r * r - l * l).sqrt();
        eprintln!(
            "R={r:5.0} m  fish radius {mean:7.3} m  analytic {want:7.3} m  \
             error {:+.3} m  spread over a lap {spread:.4} m",
            mean - want
        );
        assert!(
            (mean - want).abs() < 0.05,
            "R={r}: settled on {mean:.3} m, analytic {want:.3} m"
        );
        // and it must be a circle, not a spiral
        assert!(spread < 0.05, "R={r}: radius varies by {spread:.3} m over a lap");
    }
}

#[test]
fn the_three_models_order_the_way_the_geometry_says() {
    // Outermost to innermost in a turn: astern, then the wake, then the
    // pursuit curve. This is the claim the report makes to the reader, so it
    // is worth a test rather than an assertion in prose.
    let (l, r) = (48.0, 150.0);
    let step = 2.6 / r;
    let n = (6.0 * std::f64::consts::TAU / step) as usize;
    let path: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let a = i as f64 * step;
            (r * a.sin(), r * a.cos())
        })
        .collect();
    let recs = records(&path);
    let rec = &recs[recs.len() - 50];

    let radius = |m: LaybackModel| {
        let nav = Nav::build(&recs, cfg(m, l));
        let f = nav.fix(rec);
        let (x, y) = to_xy(f.lat, f.lon);
        x.hypot(y)
    };
    let (astern, wake, tract) = (
        radius(LaybackModel::Astern),
        radius(LaybackModel::Wake),
        radius(LaybackModel::Tractrix),
    );
    eprintln!("R=150 m: astern {astern:.2} m  wake {wake:.2} m  tractrix {tract:.2} m");
    assert!(astern > wake, "astern {astern:.2} should be outside the wake {wake:.2}");
    assert!(wake > tract, "wake {wake:.2} should be outside the tractrix {tract:.2}");
    // and the gap is the thing worth knowing: tens of metres, not centimetres
    assert!(astern - tract > 5.0, "models only {:.2} m apart", astern - tract);
}

#[test]
fn the_pursuit_curve_forgets_where_it_started() {
    // Two integrations of the same track that begin far apart must converge.
    // If they do not, the result depends on the first fix, which would make it
    // useless: a survey does not start with the fish in a known place.
    let l = 48.0;
    let path: Vec<(f64, f64)> = (0..1500).map(|i| (0.0, i as f64 * 2.6)).collect();
    let recs = records(&path);

    // Perturb by starting the second run partway along, so its initial
    // condition differs by tens of metres from the first run's state there.
    let nav_a = Nav::build(&recs, cfg(LaybackModel::Tractrix, l));
    let nav_b = Nav::build(&recs[400..], cfg(LaybackModel::Tractrix, l));

    let rec = &recs[recs.len() - 1];
    let (a, b) = (nav_a.fix(rec), nav_b.fix(rec));
    let d = geo::distance_m(a.lat, a.lon, b.lat, b.lon);
    eprintln!("two starts, same track: {d:.4} m apart at the end");
    assert!(d < 0.01, "initial condition still showing: {d:.3} m");
}
