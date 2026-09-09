//! Small checks on the pieces, mostly for edges that have actually bitten.

use swath_core::nav::{interp, smooth, unwrap_deg};
use swath_core::signal;
use swath_core::time;

#[test]
fn refine_bottom_survives_a_prior_past_the_end_of_the_trace() {
    // A short range setting with a reported altitude beyond what the ping
    // recorded. This panicked in the mosaic builder on wpa20260906 subsystem
    // 20: the search window's lower bound ended up above its upper bound.
    let trace: Vec<f32> = (0..652).map(|i| if i == 400 { 100.0 } else { 1.0 }).collect();
    // Past the end: no window can be formed at all.
    for prior in [651.0, 652.0, 900.0, 1e6] {
        assert_eq!(signal::refine_bottom(&trace, prior, 0.25), prior);
    }
    // A window that fits but holds no edge: keep the prior rather than snapping
    // to the window's own first sample.
    assert_eq!(signal::refine_bottom(&trace, 640.0, 0.25), 640.0);
    // Short traces and nonsense priors must not panic either.
    assert_eq!(signal::refine_bottom(&[], 100.0, 0.25), 100.0);
    assert_eq!(signal::refine_bottom(&trace, 0.0, 0.25), 0.0);
    assert_eq!(signal::refine_bottom(&trace, -5.0, 0.25), -5.0);
}

#[test]
fn refine_bottom_finds_the_edge_near_the_prior() {
    // A flat trace with a step at 500. A prior that is short by 10% should be
    // pulled onto the step, which is the whole point: the recorded altitude on
    // these recordings sits about 1.1 m short of the real first return.
    let trace: Vec<f32> =
        (0..2000).map(|i| if i >= 500 { 80.0 } else { 0.5 }).collect();
    let got = signal::refine_bottom(&trace, 450.0, 0.25);
    assert!((got - 500.0).abs() <= 6.0, "found {got}, wanted ~500");
}

#[test]
fn slant_to_ground_puts_nadir_at_zero() {
    // Ground range 0 must sample the trace at the altitude, not at zero range.
    let alt = 200.0f32;
    let trace: Vec<f32> =
        (0..1000).map(|i| if i == alt as usize { 50.0 } else { 1.0 }).collect();
    let g = signal::slant_to_ground(&trace, alt, 256);
    assert!(g[0] > 40.0, "first return not at ground range zero: {}", g[0]);
}

#[test]
fn unwrap_crosses_north_without_a_jump() {
    let v = unwrap_deg(&[350.0, 355.0, 2.0, 8.0, 355.0]);
    for w in v.windows(2) {
        assert!((w[1] - w[0]).abs() < 180.0, "jump in {v:?}");
    }
    // and the wrapped values still come back to the same compass points
    assert!((v[2].rem_euclid(360.0) - 2.0).abs() < 1e-9);
}

#[test]
fn interp_clamps_outside_its_span() {
    let xs = [0.0, 1.0, 2.0];
    let ys = [10.0, 20.0, 30.0];
    assert_eq!(interp(&xs, &ys, -5.0), 10.0);
    assert_eq!(interp(&xs, &ys, 5.0), 30.0);
    assert!((interp(&xs, &ys, 0.5) - 15.0).abs() < 1e-12);
    assert_eq!(interp(&xs, &ys, 1.0), 20.0);
    assert!(interp(&[], &[], 1.0).is_nan());
}

#[test]
fn smooth_preserves_length_and_edges() {
    let v: Vec<f64> = (0..20).map(|i| i as f64).collect();
    let s = smooth(&v, 5);
    assert_eq!(s.len(), v.len());
    // a straight line comes back a straight line in the middle
    assert!((s[10] - 10.0).abs() < 1e-9);
    assert_eq!(smooth(&v, 1), v);
}

#[test]
fn percentile_matches_the_numpy_convention() {
    let v: Vec<f32> = (1..=5).map(|i| i as f32).collect();
    assert!((signal::percentile(&v, 0.0) - 1.0).abs() < 1e-6);
    assert!((signal::percentile(&v, 50.0) - 3.0).abs() < 1e-6);
    assert!((signal::percentile(&v, 100.0) - 5.0).abs() < 1e-6);
    // linear interpolation between order statistics
    assert!((signal::percentile(&v, 25.0) - 2.0).abs() < 1e-6);
    assert!((signal::percentile(&v, 12.5) - 1.5).abs() < 1e-6);
    assert_eq!(signal::percentile(&[], 50.0), 0.0);
}

#[test]
fn day_of_year_round_trips_across_a_leap_year() {
    for (y, doy, want) in [
        (2026, 1, (1, 1)),
        (2026, 365, (12, 31)),
        (2024, 60, (2, 29)),   // leap
        (2026, 60, (3, 1)),    // not
    ] {
        assert_eq!(time::month_day(y, doy), Some(want), "{y} day {doy}");
    }
    assert_eq!(time::month_day(2026, 366), None);
    assert_eq!(time::month_day(2026, 0), None);
}

#[test]
fn iso8601_prints_a_known_instant() {
    // the start of wpa20260906
    assert_eq!(time::iso8601(1788686028.461), "2026-09-06T09:13:48.461Z");
    assert_eq!(time::iso8601(0.0), "1970-01-01T00:00:00Z");
}


/// Contacts are a person's observation of the seabed and cannot be recreated.
/// Neither a parse failure nor a read failure may end in an empty file.
#[test]
fn contacts_are_never_silently_dropped() {
    use swath_core::project::Contacts;
    let dir = std::env::temp_dir().join(format!("wpa-contacts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("marks.geojson");

    // One good feature and one the reader cannot make sense of.
    std::fs::write(&path, r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","geometry":{"type":"Point","coordinates":[4.07,52.56]},
         "properties":{"contact_id":"C001","name":"Barrel"}},
        {"type":"Feature","geometry":{"type":"Point","coordinates":[4.08,52.57]},
         "properties":{"contact_id":"C002","lat":"not a number"}}]}"#).unwrap();
    let err = Contacts::load(&path).expect_err("a feature was dropped without complaint");
    eprintln!("{err}");

    // And a sealed set will not write itself over the file it failed to read.
    let sealed = Contacts::sealed();
    assert!(sealed.save(&path, "p").is_err(), "a sealed set wrote itself out");
    let back = std::fs::read_to_string(&path).unwrap();
    assert!(back.contains("C001"), "the file on disk was overwritten anyway");

    // A file it can read round-trips, and leaves a backup behind.
    std::fs::write(&path, r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","geometry":{"type":"Point","coordinates":[4.07,52.56]},
         "properties":{"contact_id":"C001","name":"Barrel"}}]}"#).unwrap();
    let c = Contacts::load(&path).expect("a well-formed file");
    assert_eq!(c.items.len(), 1);
    // Identity and position came from the GeoJSON, not from the properties.
    assert_eq!(c.items[0].id, "C001");
    assert!((c.items[0].lat - 52.56).abs() < 1e-9 && (c.items[0].lon - 4.07).abs() < 1e-9);
    c.save(&path, "p").unwrap();
    assert!(path.with_extension("geojson.bak").exists(), "no backup was kept");
    assert_eq!(Contacts::load(&path).unwrap().items.len(), 1, "did not round-trip");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The absorption model, against values that can be looked up.
///
/// Francois & Garrison is not something to re-derive from the paper every time
/// somebody touches the TVG, so this pins the two numbers this survey actually
/// depends on. The high channel losing four times as much as the low one is the
/// whole explanation of why it appears to fade out at range.
#[test]
fn seawater_absorption_is_what_the_tables_say() {
    let a580 = signal::absorption_db_per_km(580.0, 20.0, 34.0, 20.0);
    let a1550 = signal::absorption_db_per_km(1550.0, 20.0, 34.0, 20.0);
    eprintln!("580 kHz: {a580:.1} dB/km, 1550 kHz: {a1550:.1} dB/km");
    assert!((a580 - 164.0).abs() < 8.0, "580 kHz came out at {a580:.1} dB/km");
    assert!((a1550 - 622.0).abs() < 30.0, "1550 kHz came out at {a1550:.1} dB/km");
    // Two-way over the swaths actually flown.
    let low = 2.0 * 47.2 * a580 / 1000.0;
    let high = 2.0 * 30.0 * a1550 / 1000.0;
    eprintln!("two-way: {low:.1} dB over 47 m at 580 kHz, {high:.1} dB over 30 m at 1550 kHz");
    assert!(high > 3.0 * low / 2.0, "the high channel should lose far more");
    // Colder water absorbs less at these frequencies, and it must be monotone
    // or the temperature inversion below is pointless.
    assert!(signal::absorption_db_per_km(580.0, 5.0, 34.0, 20.0) < a580);
}

/// Sound speed inverts to a temperature, which is where the absorption model
/// gets one from.
#[test]
fn a_sound_speed_gives_back_the_temperature_that_made_it() {
    for t in [2.0f64, 8.0, 14.0, 21.0, 28.0] {
        let c = 1448.96 + 4.591 * t - 5.304e-2 * t * t + 2.374e-4 * t.powi(3)
            + 1.340 * (34.0 - 35.0) + 1.630e-2 * 20.0
            - 1.025e-2 * t * (34.0 - 35.0);
        let back = signal::temperature_from_sound_speed(c, 34.0, 20.0);
        assert!((back - t).abs() < 0.05, "{c:.1} m/s came back as {back:.2} C, not {t}");
    }
    // The measured speed on this survey, for the record.
    let t = signal::temperature_from_sound_speed(1524.0, 34.0, 20.0);
    eprintln!("1524 m/s at S=34, 20 m -> {t:.1} C");
    assert!((15.0..27.0).contains(&t), "1524 m/s implied {t:.1} C, which is not a sea");
}

/// The gain undoes what it is supposed to undo, and nothing at the reference.
#[test]
fn the_time_varied_gain_is_flat_at_its_own_reference() {
    let alpha = signal::absorption_db_per_km(580.0, 20.0, 34.0, 20.0) / 1000.0;
    assert_eq!(signal::tvg_gain(16.0, 16.0, alpha, 1.0), 1.0);
    // Off is off, whatever the range.
    assert_eq!(signal::tvg_gain(50.0, 16.0, alpha, 0.0), 1.0);
    // Full correction at the swath edge, in dB of intensity, against the budget
    // measured for this survey: about 27 dB of spreading plus 12 of absorption.
    let g = signal::tvg_gain(47.0, 10.0, alpha, 1.0) as f64;
    let db = 20.0 * g.log10();
    eprintln!("580 kHz, 10 m -> 47 m: {db:.1} dB of gain");
    assert!((db - 32.0).abs() < 8.0, "the correction came out at {db:.1} dB");
    // Half strength is half the decibels, not half the factor.
    let half = 20.0 * (signal::tvg_gain(47.0, 10.0, alpha, 0.5) as f64).log10();
    assert!((half - db / 2.0).abs() < 1e-3);
}

/// Painter settings have to survive a save.
///
/// They did not. The viewer put `tvg`, `agc`, `nadir_blank_m` and the rest flat
/// on the dataset entry and sent the whole project to be written; `DatasetRef`
/// carried only `nav` and `sound_speed_m_s`, so serde discarded every one of
/// them on the way through. Nothing failed and nothing was logged -- the panel
/// simply read the defaults back the next time it was opened and every control
/// looked like it had sprung back on its own. Adding a painter setting must not
/// require remembering to widen a second struct, which is why the whole
/// `MosaicConfig` is stored rather than a hand-listed subset.
#[test]
fn the_mosaic_settings_survive_a_project_round_trip() {
    use swath_core::mosaic::MosaicConfig;
    use swath_core::project::{DatasetRef, Project};
    use swath_core::waterfall::Axis;

    let dir = std::env::temp_dir().join(format!("wpa-proj-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("project.json");

    let mut p = Project::new("round-trip");
    let mut d = DatasetRef {
        name: "a-recording".to_string(),
        ..serde_json::from_str::<DatasetRef>(r#"{"name":"a-recording"}"#).unwrap()
    };
    // Every field the panel can write, set to something that is not its default.
    d.mosaic = MosaicConfig {
        axis: Axis::Slant,
        tvg: 0.42,
        agc: 0.23,
        angular_gain: 0.61,
        despeckle: 0.77,
        nadir_blank_m: 3.5,
        max_range_m: Some(21.0),
        db: true,
        gamma: 0.55,
        ..Default::default()
    };
    p.datasets.push(d);
    p.save(&path).expect("save");

    let back = Project::load(&path).expect("load");
    let m = &back.dataset("a-recording").expect("dataset survived").mosaic;
    assert_eq!(m.axis, Axis::Slant, "the axis reverted to ground");
    assert_eq!(m.tvg, 0.42);
    assert_eq!(m.agc, 0.23);
    assert_eq!(m.angular_gain, 0.61);
    assert_eq!(m.despeckle, 0.77);
    assert_eq!(m.nadir_blank_m, 3.5);
    assert_eq!(m.max_range_m, Some(21.0));
    assert!(m.db);
    assert_eq!(m.gamma, 0.55);

    // A project written before this field existed must still open, and get the
    // painter's defaults rather than a parse error.
    let old = r#"{"schema":"wpa-project/2","name":"old","title":"","meta":{},
                  "datasets":[{"name":"a-recording"}],"layers":[]}"#;
    let older = dir.join("old.json");
    std::fs::write(&older, old).unwrap();
    let back = Project::load(&older).expect("an older project must still open");
    let m = &back.dataset("a-recording").expect("dataset").mosaic;
    assert_eq!(m.axis, MosaicConfig::default().axis);
    assert_eq!(m.tvg, MosaicConfig::default().tvg);

    std::fs::remove_dir_all(&dir).ok();
}

/// Every snapshot a contact carries has to survive the GeoJSON round trip.
///
/// Contacts are stored as a FeatureCollection so the existing `marks.geojson`
/// keeps working, which means every field goes through serde twice on its way
/// to disk and back. A field that is written but not read comes back empty and
/// the sheet loses a picture, with nothing to say so -- and the sheet is four
/// pictures now, the seabed and the return at each of two bands.
#[test]
fn every_contact_snapshot_survives_the_geojson_round_trip() {
    use swath_core::project::Contact;

    let src = serde_json::json!({
        "id": "C001", "lat": 51.3, "lon": 3.1,
        "snapshot": "C001.png",
        "snapshot_wf": "C001-wf.png",
        "snapshot_lf": "C001-lf.png",
        "snapshot_hf": "C001-hf.png",
        "snapshot_wf_lf": "C001-wf-lf.png",
        "snapshot_wf_hf": "C001-wf-hf.png",
    });
    let c: Contact = serde_json::from_value(src).expect("a contact");
    let back = Contact::from_feature(&c.to_feature()).expect("survives the round trip");
    assert_eq!(back.snapshot.as_deref(), Some("C001.png"));
    assert_eq!(back.snapshot_wf.as_deref(), Some("C001-wf.png"));
    assert_eq!(back.snapshot_lf.as_deref(), Some("C001-lf.png"));
    assert_eq!(back.snapshot_hf.as_deref(), Some("C001-hf.png"));
    assert_eq!(back.snapshot_wf_lf.as_deref(), Some("C001-wf-lf.png"));
    assert_eq!(back.snapshot_wf_hf.as_deref(), Some("C001-wf-hf.png"));

    // A contact written before the band crops existed still opens, with the
    // ones it has and nothing invented for the ones it does not.
    let old: Contact = serde_json::from_value(serde_json::json!({
        "id": "C002", "lat": 51.3, "lon": 3.1, "snapshot": "C002.png",
    })).expect("an older contact");
    let back = Contact::from_feature(&old.to_feature()).expect("survives");
    assert_eq!(back.snapshot.as_deref(), Some("C002.png"));
    assert!(back.snapshot_wf_lf.is_none() && back.snapshot_wf_hf.is_none());
}

/// Recordings brought into a project, and the three ways of bringing them.
///
/// The interesting parts are not the copying: they are that a name is unique
/// across the whole workspace -- because everything derived from a recording is
/// filed under it, in `out/<name>` -- and that a link and a copy are different
/// kinds of loss when the project is deleted.
#[test]
fn a_project_can_hold_its_own_recordings() {
    use swath_core::project::{Placement, Project, Workspace};

    let root = std::env::temp_dir().join(format!("wpa-hold-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let ws = Workspace::new(&root);

    // A recording is a folder with sonar in it. Two, somewhere off in the wild.
    let away = root.join("elsewhere");
    for n in ["monday", "tuesday"] {
        std::fs::create_dir_all(away.join(n)).unwrap();
        std::fs::write(away.join(n).join("line01.jsf"), b"not really a jsf").unwrap();
    }
    // And one in the shared pool, to clash with.
    std::fs::create_dir_all(ws.data_dir().join("monday")).unwrap();
    std::fs::write(ws.data_dir().join("monday").join("a.jsf"), b"x").unwrap();

    let mut p = Project::new("job");
    p.save(ws.project_path("job")).unwrap();

    // The pool already has a "monday", so the name has to move aside.
    assert!(ws.dataset_name_taken("monday"));
    assert_eq!(ws.free_dataset_name("monday"), "monday-2");
    assert_eq!(ws.free_dataset_name("tuesday"), "tuesday");
    assert!(
        ws.import_dataset("job", &away.join("monday"), "monday", Placement::Copy).is_err(),
        "importing over a name the workspace already uses would share out/monday"
    );

    // Copy: the project gets its own, the original stays put.
    ws.import_dataset("job", &away.join("monday"), "monday-2", Placement::Copy)
        .expect("copy in");
    assert!(away.join("monday").join("line01.jsf").is_file(), "a copy left the source alone");
    assert!(ws.project_data_dir("job").join("monday-2").join("line01.jsf").is_file());

    // Link: nothing is duplicated.
    ws.import_dataset("job", &away.join("tuesday"), "tuesday", Placement::Link)
        .expect("link in");
    let link = ws.project_data_dir("job").join("tuesday");
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    assert!(link.join("line01.jsf").is_file(), "the link reads through to the files");

    // Both are visible to the project, the pool's own is still visible too, and
    // the project's copy wins the name it shares with nothing.
    let seen: Vec<String> =
        ws.datasets_for(Some("job")).into_iter().map(|d| d.name).collect();
    assert_eq!(seen, vec!["monday-2", "tuesday", "monday"], "own first, then the pool");
    // With no project open, only the pool is in view.
    let pool: Vec<String> = ws.datasets().into_iter().map(|d| d.name).collect();
    assert_eq!(pool, vec!["monday"]);

    // Only the copy is the project's to lose: a link costs nothing to remake.
    assert_eq!(ws.project_holds("job"), vec!["monday-2".to_string()]);
    assert!(
        ws.delete_project("job", false).is_err(),
        "deleting a project that holds the only copy of a recording must be said out loud"
    );

    // Dropping the link leaves what it pointed at alone.
    ws.remove_dataset("job", "tuesday").expect("unlink");
    assert!(away.join("tuesday").join("line01.jsf").is_file(), "unlink kept the files");

    // Move: the source is gone afterwards, which is the point of it.
    ws.import_dataset("job", &away.join("tuesday"), "tuesday", Placement::Move)
        .expect("move in");
    assert!(!away.join("tuesday").exists(), "a move leaves nothing behind");
    assert!(ws.project_data_dir("job").join("tuesday").join("line01.jsf").is_file());

    std::fs::remove_dir_all(&root).ok();
}

/// Renaming and copying a project, and the one thing each must not do: leave
/// the name inside the file disagreeing with the folder, and cost a second copy
/// of the recordings.
#[test]
fn projects_can_be_renamed_and_duplicated() {
    use swath_core::project::{Placement, Project, Workspace};

    let root = std::env::temp_dir().join(format!("wpa-pm-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let ws = Workspace::new(&root);

    let away = root.join("elsewhere").join("wednesday");
    std::fs::create_dir_all(&away).unwrap();
    std::fs::write(away.join("line01.jsf"), b"x").unwrap();

    let mut p = Project::new("monday job");
    p.title = "Three placed mines".into();
    p.save(ws.project_path("monday job")).unwrap();
    ws.import_dataset("monday job", &away, "wednesday", Placement::Copy).unwrap();

    ws.rename_project("monday job", "tuesday job").expect("rename");
    assert!(!ws.project_dir("monday job").exists());
    let moved = Project::load(ws.project_path("tuesday job")).expect("open under the new name");
    assert_eq!(moved.name, "tuesday job", "the name in the file follows the folder");
    assert_eq!(moved.title, "Three placed mines", "and nothing else changed");
    assert!(ws.project_data_dir("tuesday job").join("wednesday").join("line01.jsf").is_file());

    assert!(ws.rename_project("tuesday job", "../escape").is_err(), "names stay inside projects/");
    assert!(ws.rename_project("nothing", "somewhere").is_err());

    // A duplicate is a second opinion about the same day at sea: it links the
    // recordings rather than copying them.
    ws.duplicate_project("tuesday job", "tuesday job v2").expect("duplicate");
    let copy = Project::load(ws.project_path("tuesday job v2")).expect("the copy opens");
    assert_eq!(copy.name, "tuesday job v2");
    assert_eq!(copy.title, "Three placed mines");
    let linked = ws.project_data_dir("tuesday job v2").join("wednesday");
    assert!(std::fs::symlink_metadata(&linked).unwrap().is_symlink(), "duplicated by link");
    assert!(linked.join("line01.jsf").is_file());
    assert!(ws.project_holds("tuesday job v2").is_empty(), "so the copy holds nothing of its own");
    assert!(ws.duplicate_project("tuesday job", "tuesday job v2").is_err(), "no clobbering");

    // Deleting the copy takes the link and leaves the files.
    ws.delete_project("tuesday job v2", false).expect("delete the copy");
    assert!(ws.project_data_dir("tuesday job").join("wednesday").join("line01.jsf").is_file());

    std::fs::remove_dir_all(&root).ok();
}
