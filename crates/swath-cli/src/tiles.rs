//! Warm the chart tile cache for a survey area, so the viewer works at sea.
//!
//! The tile servers are a courtesy, not a service we are entitled to. The
//! OpenStreetMap Foundation's tile usage policy names bulk downloading as
//! unacceptable use, so this fetches one tile at a time with a deliberate
//! pause, sends a real User-Agent, never re-fetches something already cached,
//! and refuses a run that would pull more than `MAX_TILES` without being told
//! twice. That is enough to prepare one survey area for one boat, which is the
//! whole intent -- it is not a way to mirror a region.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use swath_core::api::TileCache;
use swath_core::geo;
use swath_core::index::{Bounds, PingIndex};
use swath_core::project::Workspace;

/// Above this many tiles in one run, ask for `--yes` before starting.
const MAX_TILES: usize = 4000;
/// Minimum gap between requests that actually hit the network.
const PACE: Duration = Duration::from_millis(250);

pub fn run(rest: &[String], flags: &HashMap<String, String>) -> Result<()> {
    let root = flags
        .get("root")
        .map(PathBuf::from)
        .map_or_else(|| std::env::current_dir().context("cwd"), Ok)?;
    let ws = Workspace::new(&root);
    let cache = TileCache::new(ws.root.join("out").join("tilecache"));

    let (z0, z1) = parse_zooms(flags.get("zoom").map(String::as_str).unwrap_or("10-18"))?;
    let layers: Vec<String> = flags
        .get("layers")
        .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_else(|| vec!["osm".into(), "seamark".into()]);
    let margin: f64 = flags.get("margin").and_then(|m| m.parse().ok()).unwrap_or(800.0);

    // The area to cover: every named dataset, or all of them.
    let names: Vec<String> = if rest.is_empty() {
        ws.datasets().into_iter().map(|d| d.name).collect()
    } else {
        rest.to_vec()
    };
    let mut area = Bounds::EMPTY;
    for n in &names {
        let p = ws.index_path(n);
        if !p.exists() {
            eprintln!("  skipping {n}: not indexed");
            continue;
        }
        if let Some(b) = PingIndex::load(&p)?.bounds() {
            area = area.union(&b);
        }
    }
    if area.is_empty() {
        bail!("no indexed datasets to cover; run `survey index <dataset>` first");
    }
    let area = area.pad_m(margin);
    println!(
        "area {:.5},{:.5} to {:.5},{:.5}  (+{margin:.0} m)",
        area.min_lat, area.min_lon, area.max_lat, area.max_lon
    );

    // Count the work before doing any of it.
    let mut todo: Vec<(String, u32, i64, i64)> = Vec::new();
    for layer in &layers {
        for z in z0..=z1 {
            // OpenSeaMap's overlays are not drawn past z18; asking deeper only
            // returns empty squares.
            let z = if layer != "osm" && layer != "osmde" && layer != "topo" { z.min(18) } else { z };
            for (x, y) in tiles_for(&area, z) {
                if cache.get(layer, z, x, y).is_none() {
                    todo.push((layer.clone(), z, x, y));
                }
            }
        }
    }
    todo.sort();
    todo.dedup();
    if todo.is_empty() {
        println!("nothing to fetch; the cache already covers this area at z{z0}-{z1}");
        return Ok(());
    }
    println!("{} tiles to fetch across {} layer(s)", todo.len(), layers.len());
    if todo.len() > MAX_TILES && !flags.contains_key("yes") {
        bail!(
            "that is {} tiles, over the {MAX_TILES} this will do unprompted.\n\
             Narrow --zoom, or pass --yes if you have thought about it. The tile\n\
             servers are donated capacity and bulk downloading is against their\n\
             usage policy.",
            todo.len()
        );
    }
    let eta = PACE.mul_f64(todo.len() as f64);
    println!("paced at one request per {} ms, about {:.0} min", PACE.as_millis(), eta.as_secs_f64() / 60.0);

    let (mut got, mut empty, mut failed) = (0usize, 0usize, 0usize);
    let start = Instant::now();
    for (i, (layer, z, x, y)) in todo.iter().enumerate() {
        let Some(url) = TileCache::url(layer, *z, *x, *y) else {
            bail!("unknown layer {layer}; have {:?}",
                  swath_core::api::TILE_SOURCES.iter().map(|(n, _)| *n).collect::<Vec<_>>());
        };
        let t = Instant::now();
        match fetch(&url) {
            Ok(bytes) => {
                // A fully transparent overlay square is still worth caching:
                // it is the answer, and caching it stops the viewer asking
                // again on every pan.
                if bytes.len() <= 400 {
                    empty += 1;
                } else {
                    got += 1;
                }
                cache.put(layer, *z, *x, *y, &bytes)
                    .with_context(|| format!("writing {layer}/{z}/{x}/{y}"))?;
            }
            Err(e) => {
                failed += 1;
                if failed <= 3 {
                    eprintln!("  {layer}/{z}/{x}/{y}: {e}");
                }
            }
        }
        if i % 50 == 49 || i + 1 == todo.len() {
            println!(
                "  {}/{}  {got} with content, {empty} empty, {failed} failed  [{:.0}s]",
                i + 1, todo.len(), start.elapsed().as_secs_f64()
            );
        }
        if let Some(rest) = PACE.checked_sub(t.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    println!(
        "done in {:.0}s: {got} tiles with content, {empty} empty, {failed} failed",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    let r = ureq::get(url)
        .set("User-Agent", "swath/1.0 (local survey tool; single user)")
        .timeout(Duration::from_secs(20))
        .call()?;
    let mut buf = Vec::new();
    r.into_reader().take(4 << 20).read_to_end(&mut buf)?;
    if buf.is_empty() {
        bail!("empty response");
    }
    Ok(buf)
}

fn parse_zooms(s: &str) -> Result<(u32, u32)> {
    let (a, b) = match s.split_once('-') {
        Some((a, b)) => (a.parse()?, b.parse()?),
        None => {
            let z = s.parse()?;
            (z, z)
        }
    };
    if a > b || b > 20 {
        bail!("bad zoom range {s}");
    }
    Ok((a, b))
}

fn tiles_for(b: &Bounds, z: u32) -> Vec<(i64, i64)> {
    let (x0f, y0f) = geo::lonlat_to_px(b.min_lon, b.max_lat, z as f64);
    let (x1f, y1f) = geo::lonlat_to_px(b.max_lon, b.min_lat, z as f64);
    let (x0, y0) = ((x0f / 256.0).floor() as i64, (y0f / 256.0).floor() as i64);
    let (x1, y1) = ((x1f / 256.0).floor() as i64, (y1f / 256.0).floor() as i64);
    let n = 1i64 << z;
    let mut v = Vec::new();
    for y in y0..=y1 {
        for x in x0..=x1 {
            if x >= 0 && y >= 0 && x < n && y < n {
                v.push((x, y));
            }
        }
    }
    v
}
