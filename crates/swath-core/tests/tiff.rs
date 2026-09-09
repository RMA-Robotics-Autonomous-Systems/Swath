//! The GeoTIFF reader against GDAL.
//!
//! Every file here was written by GDAL through rasterio, and the expected
//! pixels were read back by rasterio, so agreement means this reader and the
//! reference implementation understand the same bytes the same way. That is the
//! only kind of assurance worth having for a format with this many optional
//! encodings: a reader tested against its own output would pass while getting
//! the predictor backwards.
//!
//! The harness that wrote them is gone, so they are frozen -- see `parity.rs`
//! for why that is a property and not a problem.
//!
//! The fourteen small files are synthetic and belong to the software, so they
//! are committed here. The external sample is a real grid from a real survey:
//! its probes live in the workspace (`out/fixtures/tiff-external.json`) next to
//! the 373 MB file they describe, and that half is skipped when neither is
//! present.

use std::path::PathBuf;

mod common;

use serde_json::Value;
use swath_core::tiff::GeoTiff;

/// The repository root, where the committed fixtures live.
fn root() -> PathBuf {
    common::repo()
}

/// The workspace, where `data/` is. The external sample is referenced from
/// there because it is 373 MB and does not belong in the repository.
fn workspace() -> PathBuf {
    common::workspace()
}

fn read(path: PathBuf) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The synthetic rasters: computed reference values, shipped with the source.
fn fixtures() -> Option<Value> {
    read(root().join("fixtures/tiff.json"))
}

/// Probes for a grid belonging to a survey, which live with that survey.
fn external_fixtures() -> Option<Value> {
    read(workspace().join("out/fixtures/tiff-external.json"))
}

/// Values are compared as f64 with a tolerance that suits the sample format:
/// integers must be exact, float32 need only survive the round trip through
/// f64 that both sides do.
fn close(got: f64, want: f64, dtype: &str) -> bool {
    if dtype.starts_with("float") {
        (got - want).abs() <= want.abs() * 1e-6 + 1e-9
    } else {
        got == want
    }
}

fn check_file(entry: &Value, path: PathBuf) -> Result<String, String> {
    let t = GeoTiff::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = entry["file"].as_str().unwrap_or("?");
    let dtype = entry["dtype"].as_str().unwrap_or("");

    let want_w = entry["width"].as_u64().unwrap() as usize;
    let want_h = entry["height"].as_u64().unwrap() as usize;
    if (t.width, t.height) != (want_w, want_h) {
        return Err(format!("{name}: size {}x{} vs {want_w}x{want_h}", t.width, t.height));
    }
    let want_bands = entry["bands"].as_u64().unwrap() as usize;
    if t.samples != want_bands {
        return Err(format!("{name}: {} bands vs {want_bands}", t.samples));
    }
    if let Some(e) = entry["epsg"].as_u64() {
        if t.epsg != Some(e as u32) {
            return Err(format!("{name}: EPSG {:?} vs {e}", t.epsg));
        }
    }

    // Corner coordinates: the georeferencing, not the pixels. A transform that
    // is transposed or half a pixel out puts an imported layer in the wrong
    // place, which is the failure that matters most here.
    for (key, col, row) in [
        ("ul", 0usize, 0usize),
        ("lr", want_w - 1, want_h - 1),
    ] {
        let want = entry["corners"][key].as_array().unwrap();
        let (x, y) = t.pixel_to_model(col as f64, row as f64);
        let (wx, wy) = (want[0].as_f64().unwrap(), want[1].as_f64().unwrap());
        let tol = 1e-6 * wx.abs().max(wy.abs()).max(1.0);
        if (x - wx).abs() > tol || (y - wy).abs() > tol {
            return Err(format!("{name}: {key} corner {x},{y} vs {wx},{wy}"));
        }
        // and the inverse has to land back on the pixel it came from
        let (bc, br) = t.model_to_pixel(x, y);
        if (bc - col as f64).abs() > 1e-6 || (br - row as f64).abs() > 1e-6 {
            return Err(format!("{name}: {key} round trip {bc},{br} vs {col},{row}"));
        }
    }

    let mut n = 0;
    for p in entry["probes"].as_array().unwrap() {
        let (c, r) = (p["col"].as_u64().unwrap() as usize, p["row"].as_u64().unwrap() as usize);
        for (b, want) in p["values"].as_array().unwrap().iter().enumerate() {
            let want = want.as_f64().unwrap();
            let got = t
                .value(c, r, b)
                .ok_or_else(|| format!("{name}: no value at {c},{r} band {b}"))?;
            if !close(got, want, dtype) {
                return Err(format!("{name}: ({c},{r}) band {b} = {got} vs {want}"));
            }
            n += 1;
        }
    }
    Ok(format!("{name:<18} {:<28} {n} probes", t.describe()))
}

#[test]
fn reads_what_gdal_wrote() {
    let Some(fx) = fixtures() else {
        eprintln!("no fixtures/tiff.json; run tools/make_tiff_fixtures.py");
        return;
    };
    let dir = root().join("fixtures/tiff");
    let mut failures = Vec::new();
    let mut checked = 0;
    for f in fx["files"].as_array().unwrap() {
        let path = dir.join(f["file"].as_str().unwrap());
        match check_file(f, path) {
            Ok(line) => {
                eprintln!("  {line}");
                checked += 1;
            }
            Err(e) => failures.push(e),
        }
    }
    assert!(checked > 0, "no fixture files present");
    assert!(failures.is_empty(), "{} files disagree:\n  {}", failures.len(), failures.join("\n  "));
    eprintln!("{checked} encodings agree with GDAL");
}

#[test]
fn whole_image_checksum_matches() {
    // Probes catch a wrong pixel; a sum over every pixel catches a wrong
    // *stride*, where the probes happen to land on rows that still line up.
    let Some(fx) = fixtures() else { return };
    let dir = root().join("fixtures/tiff");
    for f in fx["files"].as_array().unwrap() {
        let name = f["file"].as_str().unwrap();
        let Ok(t) = GeoTiff::open(dir.join(name)) else { continue };
        let want = f["checksum"].as_f64().unwrap();
        let mut sum = 0.0f64;
        for r in 0..t.height {
            for c in 0..t.width {
                for b in 0..t.samples {
                    sum += t.value(c, r, b).unwrap_or(f64::NAN);
                }
            }
        }
        let tol = want.abs() * 1e-9 + 1e-6;
        assert!(
            (sum - want).abs() <= tol,
            "{name}: pixel sum {sum} vs {want}"
        );
    }
    eprintln!("every pixel of every fixture sums to what rasterio read");
}

#[test]
fn reads_the_survey_dtm() {
    let Some(fx) = external_fixtures() else {
        eprintln!("no out/fixtures/tiff-external.json in the workspace; skipping");
        return;
    };
    let Some(ext) = fx.get("external").and_then(|e| e.as_array()) else {
        eprintln!("no external sample in the fixture");
        return;
    };
    for e in ext {
        let path = workspace().join(e["file"].as_str().unwrap());
        if !path.exists() {
            eprintln!("skipping {}: not present", path.display());
            continue;
        }
        match check_file(e, path) {
            Ok(line) => eprintln!("  {line}"),
            Err(msg) => panic!("{msg}"),
        }
    }
}
