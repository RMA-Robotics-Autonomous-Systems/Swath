//! Dump what Rust computes, in the schema the Python oracle writes.
//!
//! The tests in `swath-core/tests/parity.rs` are the gate; this command exists
//! so that when one of them fails the two files can be put side by side and
//! read, rather than the failure being a single assert message.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::json;
use swath_core::crs;
use swath_core::index::PingIndex;
use swath_core::jsf::JsfFile;
use swath_core::nav::{Nav, NavConfig};
use swath_core::project::Workspace;

pub fn run(rest: &[String], flags: &HashMap<String, String>) -> Result<()> {
    let out = PathBuf::from(rest.first().context("output directory required")?);
    std::fs::create_dir_all(&out)?;
    let root = flags
        .get("root")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let ws = Workspace::new(&root);
    let n_pings: usize = flags.get("pings").and_then(|p| p.parse().ok()).unwrap_or(500);

    let datasets: Vec<String> = match flags.get("dataset") {
        Some(d) => vec![d.clone()],
        None => ws.datasets().into_iter().map(|d| d.name).collect(),
    };
    if datasets.is_empty() {
        bail!("no datasets under {}", ws.data_dir().display());
    }

    // ---- parse: raw header fields straight out of the JSF ------------------
    let mut parse = Vec::new();
    for name in &datasets {
        let Some(dir) = ws.datasets().into_iter().find(|d| &d.name == name).map(|d| d.dir) else { continue };
        for path in ws.sonar_files(&dir).iter().take(2) {
            let Ok(jf) = JsfFile::open(path) else { continue };
            let mut n = 0usize;
            let mut rows = Vec::new();
            let _ = jf.walk(|hdr, body| {
                if hdr.mtype == 80 {
                    // every 97th ping, so the sample crosses files, channels
                    // and range changes rather than clustering at the start
                    if n % 97 == 0 {
                        if let Some(p) = swath_core::jsf::parse_sonar_message(hdr, body) {
                            rows.push(json!({
                                "offset": p.offset,
                                "subsystem": p.subsystem,
                                "channel": p.channel,
                                "time": p.time,
                                "ping": p.ping_number,
                                "nsamples": p.nsamples,
                                "interval_ns": p.sample_interval_ns,
                                "lat": p.latitude,
                                "lon": p.longitude,
                                "heading": p.heading,
                                "pitch": p.pitch,
                                "roll": p.roll,
                                "depth": p.depth_m,
                                "altitude": p.altitude_m,
                                "validity": p.validity,
                                "weight": p.weighting_factor,
                                "fmt": p.data_format,
                                "f0": p.start_freq_hz,
                                "f1": p.end_freq_hz,
                                // a checksum over the decoded trace: cheaper to
                                // compare than 8000 floats and just as strict
                                "data_sum": p.data.iter().map(|&v| v as f64).sum::<f64>(),
                                "data_n": p.data.len(),
                                "data_head": p.data.iter().take(8).map(|&v| v as f64).collect::<Vec<_>>(),
                            }));
                        }
                    }
                    n += 1;
                }
                rows.len() < n_pings
            });
            parse.push(json!({
                "file": path.file_name().unwrap().to_string_lossy(),
                "dataset": name,
                "pings": rows,
            }));
        }
    }
    write(&out.join("parse.json"), &json!({ "files": parse }))?;

    // ---- transform: (ping, sample) -> (lat, lon) ---------------------------
    let mut transform = Vec::new();
    for name in &datasets {
        let idx_path = ws.index_path(name);
        if !idx_path.exists() {
            continue;
        }
        let idx = PingIndex::load(&idx_path)?;
        let nav = Nav::build(&idx.records, NavConfig::default());
        let mut rows = Vec::new();
        let step = (idx.len() / n_pings.max(1)).max(1);
        for r in idx.records.iter().step_by(step) {
            let f = nav.fix(r);
            if !f.lat.is_finite() {
                continue;
            }
            rows.push(json!({
                "time": r.time, "subsystem": r.subsystem, "channel": r.channel,
                "boat": [f.boat_lat, f.boat_lon],
                "fish": [f.lat, f.lon],
                "cog": f.cog, "bearing": f.bearing, "speed": f.speed,
            }));
        }
        transform.push(json!({ "dataset": name, "nav": NavConfig::default(), "fixes": rows }));
    }
    write(&out.join("transform.json"), &json!({ "datasets": transform }))?;

    // ---- projection: this crate's geodesy against PROJ ---------------------
    let mut proj = Vec::new();
    let codes = [4326u32, 3857, 32631, 28992, 3035, 23031, 25831];
    let mut lat = 52.50;
    while lat <= 52.62 {
        let mut lon = 4.02;
        while lon <= 4.10 {
            for &c in &codes {
                if let Some(sys) = crs::get(c) {
                    let (x, y) = sys.from_wgs84(lat, lon);
                    let (blat, blon) = sys.to_wgs84(x, y);
                    proj.push(json!({
                        "epsg": c, "lat": lat, "lon": lon,
                        "x": x, "y": y,
                        "round_trip_m": swath_core::geo::haversine_m(lat, lon, blat, blon),
                    }));
                }
            }
            lon += 0.04;
        }
        lat += 0.06;
    }
    write(&out.join("proj.json"), &json!({ "points": proj }))?;

    println!("-> {}", out.display());
    Ok(())
}

fn write(path: &std::path::Path, v: &serde_json::Value) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(v)?)?;
    println!("   {} ({:.0} kB)", path.display(), std::fs::metadata(path)?.len() as f64 / 1e3);
    Ok(())
}
