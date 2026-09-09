//! Differential tests against the reference implementation.
//!
//! The fixtures were cut by the Python reader this code was written against.
//! That reader is no longer in the tree, so they are frozen: a record of what
//! a second, independent implementation computed, and nothing here can
//! regenerate them. `swath fixtures` writes the same schema from *this* code,
//! which makes it useful for reading a failure side by side and useless as a
//! source of truth -- regenerating the gate from the thing under test would
//! assert only that it agrees with itself. A failing case is a bug here until
//! proved otherwise; it is never fixed by rewriting the fixture.
//!
//! **The fixtures live in two places, because they are two kinds of thing.**
//!
//! `proj.json` and `tiff/` are computed reference values -- projected
//! coordinates and synthetic rasters -- belonging to no survey and to nobody.
//! They ship with the source, and their tests run on any machine that clones
//! it.
//!
//! `parse.json` and `nav.json` are a particular survey: byte offsets into
//! specific recordings, and the positions of a specific boat on a specific
//! afternoon. They are not the software's to carry, so they live in the
//! workspace alongside the recordings they describe -- `out/fixtures/` -- and
//! these two tests skip without them. They could not run anyway: both need the
//! recordings themselves, which are tens of gigabytes and equally not ours.
//!
//! The three are not equally strong, and the assertions reflect that. `parse`
//! is held to exact agreement with a reader that had been read against this
//! data for months. `proj` is checked against PROJ, an implementation this
//! code has never met. `nav` is one specification implemented twice by hand --
//! it catches transcription slips, not wrong ideas, because both sides would
//! share those.

use std::path::PathBuf;

use serde_json::Value;

mod common;
use swath_core::crs;
use swath_core::geo;
use swath_core::jsf::JsfFile;

/// Reference values that belong to the software: shipped, always present.
fn fixtures_dir() -> PathBuf {
    common::repo().join("fixtures")
}

/// Fixtures cut from one survey's recordings, which live with that survey.
fn data_fixtures_dir() -> PathBuf {
    common::workspace().join("out").join("fixtures")
}

fn read(dir: PathBuf, name: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(name)).ok()?).ok()
}

fn load(name: &str) -> Option<Value> {
    read(fixtures_dir(), name)
}

fn load_survey(name: &str) -> Option<Value> {
    read(data_fixtures_dir(), name)
}

fn data_root() -> PathBuf {
    common::workspace().join("data")
}

/// Locate the recording a parse fixture came from.
fn source_file(dataset: &str, file: &str) -> Option<PathBuf> {
    let p = data_root().join(dataset).join(file);
    if p.exists() {
        return Some(p);
    }
    let flat = data_root().join(file);
    flat.exists().then_some(flat)
}

fn close(got: f64, want: f64, tol: f64, what: &str, off: u64) {
    assert!(
        (got - want).abs() <= tol * want.abs().max(1.0),
        "{what} @{off}: {got} vs {want}"
    );
}

#[test]
fn parse_matches_python() {
    let Some(fx) = load_survey("parse.json") else {
        eprintln!("no parse.json in the workspace's out/fixtures; skipping");
        return;
    };
    let mut checked = 0usize;
    for f in fx["files"].as_array().unwrap() {
        let dataset = f["dataset"].as_str().unwrap();
        let file = f["file"].as_str().unwrap();
        let Some(path) = source_file(dataset, file) else {
            eprintln!("skipping {dataset}/{file}: recording not present");
            continue;
        };
        let jf = JsfFile::open(&path).expect("open");
        for row in f["pings"].as_array().unwrap() {
            let off = row["offset"].as_u64().unwrap();
            let p = jf
                .ping_at(off)
                .unwrap_or_else(|| panic!("no ping at {off} in {file}"));

            // Integers must agree exactly. These are the fields the index is
            // built from; one of them off by a byte is the failure this whole
            // harness exists to catch.
            assert_eq!(p.subsystem as u64, row["subsystem"].as_u64().unwrap(), "subsystem @{off}");
            assert_eq!(p.channel as u64, row["channel"].as_u64().unwrap(), "channel @{off}");
            assert_eq!(p.ping_number as u64, row["ping"].as_u64().unwrap(), "ping @{off}");
            assert_eq!(p.nsamples as u64, row["nsamples"].as_u64().unwrap(), "nsamples @{off}");
            assert_eq!(
                p.sample_interval_ns as u64,
                row["interval_ns"].as_u64().unwrap(),
                "interval @{off}"
            );
            assert_eq!(p.validity as u64, row["validity"].as_u64().unwrap(), "validity @{off}");
            assert_eq!(p.weighting_factor as i64, row["weight"].as_i64().unwrap(), "weight @{off}");
            assert_eq!(p.data_format as i64, row["fmt"].as_i64().unwrap(), "fmt @{off}");
            assert_eq!(p.data.len() as u64, row["data_n"].as_u64().unwrap(), "data_n @{off}");

            // These floats are an integer divided by a constant, so they are
            // exactly representable and near-exact equality is the right test.
            close(p.time, row["time"].as_f64().unwrap(), 1e-9, "time", off);
            close(p.heading, row["heading"].as_f64().unwrap(), 1e-9, "heading", off);
            close(p.pitch, row["pitch"].as_f64().unwrap(), 1e-9, "pitch", off);
            close(p.roll, row["roll"].as_f64().unwrap(), 1e-9, "roll", off);
            close(p.depth_m, row["depth"].as_f64().unwrap(), 1e-9, "depth", off);
            close(p.altitude_m, row["altitude"].as_f64().unwrap(), 1e-9, "altitude", off);
            close(p.start_freq_hz, row["f0"].as_f64().unwrap(), 1e-9, "f0", off);
            close(p.end_freq_hz, row["f1"].as_f64().unwrap(), 1e-9, "f1", off);
            if let Some(lat) = row["lat"].as_f64() {
                close(p.latitude, lat, 1e-12, "lat", off);
                close(p.longitude, row["lon"].as_f64().unwrap(), 1e-12, "lon", off);
            }

            // The trace: first eight samples exactly, then the whole thing as a
            // sum. numpy adds pairwise and this adds in order, so the sum gets
            // a relative tolerance; the head catches any real decode error.
            for (i, v) in row["data_head"].as_array().unwrap().iter().enumerate() {
                assert_eq!(
                    p.data[i] as f64,
                    v.as_f64().unwrap(),
                    "sample {i} @{off} in {file}"
                );
            }
            let sum: f64 = p.data.iter().map(|&v| v as f64).sum();
            let want = row["data_sum"].as_f64().unwrap();
            assert!(
                (sum - want).abs() <= want.abs() * 1e-9 + 1e-6,
                "data_sum @{off} in {file}: {sum} vs {want}"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no recordings present; parse parity not exercised");
    eprintln!("parse parity: {checked} pings across the sample");
}

#[test]
fn projections_match_proj() {
    let Some(fx) = load("proj.json") else {
        eprintln!("no proj.json; run tools/make_fixtures.py");
        return;
    };
    // Tolerances are per-CRS because the datum shifts differ in kind. A
    // projection on the same datum should agree to millimetres; one reached
    // through a published Helmert cannot beat that Helmert's own residual, and
    // PROJ may well pick a different -- better -- transform than the seven
    // parameters in our registry.
    let tol_m = |epsg: u64| match epsg {
        4326 => 1e-9,
        3857 | 32631 | 25831 | 3035 => 0.001,
        28992 => 1.5, // EPSG:1672 against whatever PROJ chose; both ~1 m class
        23031 => 3.0, // three-parameter ED50, published at a few metres
        _ => 1.0,
    };
    let mut worst: Vec<(u64, f64)> = Vec::new();
    for p in fx["points"].as_array().unwrap() {
        let epsg = p["epsg"].as_u64().unwrap();
        let (lat, lon) = (p["lat"].as_f64().unwrap(), p["lon"].as_f64().unwrap());
        let sys = crs::get(epsg as u32).unwrap_or_else(|| panic!("no CRS {epsg}"));
        let (x, y) = sys.from_wgs84(lat, lon);
        let (wx, wy) = (p["x"].as_f64().unwrap(), p["y"].as_f64().unwrap());
        // For a geographic CRS the "metres" are degrees; scale so one number
        // covers both cases.
        let scale = if sys.is_geographic() { 111_320.0 } else { 1.0 };
        let d = ((x - wx).powi(2) + (y - wy).powi(2)).sqrt() * scale;
        let t = tol_m(epsg);
        assert!(
            d <= t,
            "EPSG:{epsg} at {lat},{lon}: {d:.4} m from PROJ (tolerance {t} m)\n  \
             rust {x:.4},{y:.4}\n  proj {wx:.4},{wy:.4}"
        );
        match worst.iter_mut().find(|w| w.0 == epsg) {
            Some(w) => w.1 = w.1.max(d),
            None => worst.push((epsg, d)),
        }
    }
    worst.sort_by_key(|w| w.0);
    for (e, d) in worst {
        eprintln!("EPSG:{e:<6} worst disagreement with PROJ {d:.4} m");
    }
}

#[test]
fn projections_round_trip() {
    // Independent of any fixture: forward then inverse has to land where it
    // started. Catches an inverse that was never exercised.
    for &epsg in &[4326u32, 3857, 32631, 28992, 3035, 23031, 25831] {
        let sys = crs::get(epsg).unwrap();
        let mut worst: f64 = 0.0;
        let mut lat = 52.40;
        while lat <= 52.70 {
            let mut lon = 3.90;
            while lon <= 4.20 {
                let (x, y) = sys.from_wgs84(lat, lon);
                let (blat, blon) = sys.to_wgs84(x, y);
                worst = worst.max(geo::haversine_m(lat, lon, blat, blon));
                lon += 0.1;
            }
            lat += 0.1;
        }
        assert!(worst < 0.01, "EPSG:{epsg} round trip off by {worst:.4} m");
        eprintln!("EPSG:{epsg:<6} round trip {worst:.6} m");
    }
}

#[test]
fn nav_matches_reference() {
    let Some(fx) = load_survey("nav.json") else {
        eprintln!("no nav.json in the workspace's out/fixtures; skipping");
        return;
    };
    use swath_core::index::PingIndex;
    use swath_core::nav::{Nav, NavConfig};

    let mut any = false;
    for d in fx["datasets"].as_array().unwrap() {
        let name = d["dataset"].as_str().unwrap();
        let idx_path = swath_core::project::Workspace::new(common::workspace()).index_path(name);
        if !idx_path.exists() {
            eprintln!("skipping {name}: no index (run `swath index {name}`)");
            continue;
        }
        let idx = PingIndex::load(&idx_path).expect("load index");
        let cfg = NavConfig {
            layback_m: d["layback_m"].as_f64().unwrap(),
            gps_to_towpoint_m: d["towpoint_m"].as_f64().unwrap(),
            cog_baseline_s: d["cog_baseline_s"].as_f64().unwrap(),
            ..Default::default()
        };
        let nav = Nav::build(&idx.records, cfg);

        let (mut worst_boat, mut worst_fish) = (0.0f64, 0.0f64);
        let mut n = 0usize;
        for f in d["fixes"].as_array().unwrap() {
            let t = f["time"].as_f64().unwrap();
            let Some(rec) = idx.records.iter().find(|r| (r.time - t).abs() < 1e-6) else {
                continue;
            };
            let got = nav.fix(rec);
            let boat = f["boat"].as_array().unwrap();
            let fish = f["fish"].as_array().unwrap();
            worst_boat = worst_boat.max(geo::haversine_m(
                got.boat_lat,
                got.boat_lon,
                boat[0].as_f64().unwrap(),
                boat[1].as_f64().unwrap(),
            ));
            worst_fish = worst_fish.max(geo::haversine_m(
                got.lat,
                got.lon,
                fish[0].as_f64().unwrap(),
                fish[1].as_f64().unwrap(),
            ));
            n += 1;
        }
        if n == 0 {
            continue;
        }
        any = true;
        eprintln!("{name}: {n} fixes · boat worst {worst_boat:.4} m · fish worst {worst_fish:.4} m");
        // The boat position is pure interpolation between the same fixes, so it
        // should be identical to floating-point noise.
        assert!(worst_boat < 0.01, "{name}: boat position differs by {worst_boat:.3} m");
        // The fish adds a course, and two smoothings of an unwrapped angle
        // series are not bit-identical. Five centimetres is far below the
        // 5-10 m the position is actually good to, and still tight enough that
        // a wrong sign or a swapped axis could not hide under it.
        assert!(worst_fish < 0.05, "{name}: fish position differs by {worst_fish:.3} m");
    }
    assert!(any, "no indexes present; nav parity not exercised");
}
