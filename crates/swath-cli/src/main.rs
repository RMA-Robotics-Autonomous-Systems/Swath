//! Command line for swath, and the server the viewer runs on.

mod fixtures;
mod tiles;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use swath_core::api::State;
use swath_core::index::PingIndex;
use swath_core::mosaic::{Mosaic, MosaicConfig};
use swath_core::nav::{self, NavConfig, SegmentKind};
use swath_core::project::Workspace;
use swath_core::time::{hms, iso8601};

const USAGE: &str = "\
swath — sidescan survey viewer

  swath serve [--root DIR] [--port N] [--host ADDR] [--ui DIR] [--open]
      Start the viewer. Defaults to a free port on 127.0.0.1. Run `swath` with
      no arguments at all and it does this and opens a browser at it.
      --host 0.0.0.0 lets the rest of the network in; there is no password on
      it, so only do that on one you trust.

  swath index <dataset> [--root DIR] [--force]
      Build the ping index for a recording.

  swath mosaic <dataset> [--subsystem N] [--zoom N] [--force] [--model M]
      Build the georeferenced mosaic.

  swath layer <file.tif|file.gpx> [--out FILE] [--zoom N] [--preview PNG]
      Import a layer: resample a GeoTIFF onto the chart grid, or read a GPX,
      and report what it holds.

  swath info <dataset>
      Summarise a recording.

  swath report <project> [--out FILE]
      Render the survey report as HTML.

  swath plan <project> [--az DEG] [--step DEG] [--export]
      The search plan for the project's datums. With no flags it prints every
      azimuth in the quadrant; --az details one; --export writes the GPX set
      into the project's exports/ directory.

  swath tiles [<dataset>...] [--zoom 10-18] [--layers osm,seamark]
      Pre-fetch chart tiles over the survey area so the viewer works with no
      connection. Paced, and it will not bulk-download.

  swath fixtures <dir> [--dataset NAME] [--pings N]
      Dump what this code computes, in the schema the frozen fixtures use.
";

struct Args {
    cmd: String,
    rest: Vec<String>,
    flags: std::collections::HashMap<String, String>,
}

fn parse() -> Args {
    let mut rest = Vec::new();
    let mut flags = std::collections::HashMap::new();
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_default();
    while let Some(a) = it.next() {
        if let Some(k) = a.strip_prefix("--") {
            match k {
                "force" | "open" | "yes" | "export" => {
                    flags.insert(k.to_string(), "1".to_string());
                }
                _ => {
                    let v = it.next().unwrap_or_default();
                    flags.insert(k.to_string(), v);
                }
            }
        } else {
            rest.push(a);
        }
    }
    Args { cmd, rest, flags }
}

fn root_of(a: &Args) -> PathBuf {
    a.flags
        .get("root")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn main() -> Result<()> {
    let mut a = parse();
    // No arguments at all: start the viewer and open it. That is what
    // double-clicking the executable does on Windows, where there is no
    // command line to have typed a subcommand on -- and a usage screen in a
    // console window that closes again with the process is not an answer.
    if a.cmd.is_empty() {
        a.flags.insert("open".to_string(), "1".to_string());
        return serve(&a);
    }
    match a.cmd.as_str() {
        "serve" => serve(&a),
        "index" => index(&a),
        "mosaic" => mosaic(&a),
        "layer" => layer(&a),
        "info" => info(&a),
        "report" => report(&a),
        "plan" => plan(&a),
        "fixtures" => fixtures::run(&a.rest, &a.flags),
        "tiles" => tiles::run(&a.rest, &a.flags),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn serve(a: &Args) -> Result<()> {
    let root = root_of(a);
    // The frontend is inside the executable. A directory is an override for
    // working on the frontend itself -- `--ui` names one, and one beside the
    // binary is picked up from a checkout -- so not finding one is the normal
    // case for an installed copy rather than something to fail on.
    let ui_dir = match a.flags.get("ui") {
        Some(p) => {
            let p = PathBuf::from(p);
            if !p.join("index.html").exists() {
                bail!("no frontend at {}", p.display());
            }
            Some(p)
        }
        None => swath_core::server::ui_dir(),
    };
    let port: u16 = a.flags.get("port").and_then(|p| p.parse().ok()).unwrap_or(0);
    // Loopback unless asked otherwise. Anything else is a decision about who
    // can reach the workspace, so it is spelled out on the command line rather
    // than guessed at from the environment.
    let host = a.flags.get("host").map(String::as_str).unwrap_or("127.0.0.1");
    let listener = TcpListener::bind((host, port))
        .with_context(|| format!("binding {host}:{port}"))?;
    let addr = listener.local_addr()?;
    // `0.0.0.0` is a bind address, not somewhere to point a browser. From this
    // machine the loopback one always works, and the line further down says
    // what to type on another machine.
    let shown = if addr.ip().is_unspecified() {
        format!("127.0.0.1:{}", addr.port())
    } else {
        addr.to_string()
    };
    let url = format!("http://{shown}/");
    println!("swath {} — {url}", swath_core::VERSION);
    println!("  workspace: {}", root.display());
    match &ui_dir {
        Some(d) => println!("  frontend:  {}", d.display()),
        None => println!("  frontend:  built in, {} files", swath_core::ui::FILES.len()),
    }
    if !addr.ip().is_loopback() {
        match lan_address() {
            Some(ip) => println!("  network:   http://{ip}:{}/", addr.port()),
            None => println!("  network:   this machine's address on port {}", addr.port()),
        }
        println!("             no password -- whoever reaches it can change the workspace");
    }
    // Bound already, so a browser that gets there first waits in the backlog
    // rather than being refused.
    if a.flags.contains_key("open") && !swath_core::server::open_in_browser(&url) {
        eprintln!("  (no browser opened -- the address above is the whole interface)");
    }

    let server = swath_core::server::Server::new(Arc::new(State::new(root)), ui_dir);
    server.serve(listener)?;
    Ok(())
}

/// The address another machine on the network would reach this one at.
///
/// The routing table is asked rather than the interface list, because the
/// interface list does not know which one matters: this machine answers to a
/// wifi address and a docker bridge, and picking the wrong one prints a URL
/// that quietly never connects. A connected UDP socket sends nothing -- it
/// only makes the kernel choose a source address for that destination, which
/// is the question being asked. The destination is in the range reserved for
/// documentation, so it is unroutable by definition and no packet could go
/// anywhere even if one were sent.
fn lan_address() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    s.connect(("192.0.2.1", 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn dataset_arg(a: &Args) -> Result<String> {
    a.rest.first().cloned().context("dataset name required")
}

fn index(a: &Args) -> Result<()> {
    let root = root_of(a);
    let ws = Workspace::new(&root);
    let name = dataset_arg(a)?;
    let out = ws.index_path(&name);
    if out.exists() && !a.flags.contains_key("force") {
        println!("{} already indexed ({}), use --force", name, out.display());
        return Ok(());
    }
    let dir = ws
        .datasets()
        .into_iter()
        .find(|d| d.name == name)
        .map(|d| d.dir)
        .with_context(|| format!("no dataset {name} under {}", ws.data_dir().display()))?;
    let files = ws.sonar_files(&dir);
    if files.is_empty() {
        bail!("no .jsf or .xtf files in {}", dir.display());
    }
    println!("indexing {} file(s)…", files.len());
    let t = std::time::Instant::now();
    let idx = PingIndex::build(&files)?;
    std::fs::create_dir_all(ws.out_dir(&name))?;
    idx.save(&out)?;
    for w in &idx.header.warnings {
        eprintln!("  warning: {w}");
    }
    println!(
        "{} pings in {:.1}s -> {} ({:.1} MB)",
        idx.len(),
        t.elapsed().as_secs_f64(),
        out.display(),
        std::fs::metadata(&out)?.len() as f64 / 1e6
    );
    Ok(())
}

fn layer(a: &Args) -> Result<()> {
    let src = a
        .rest
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("which file? `survey layer <file.tif>`"))?;
    // GPX needs no resampling: it is a few thousand coordinates, and the
    // viewer draws it as vectors.
    if swath_core::project::has_ext(&src, "gpx") {
        let g = swath_core::gpx::read(&src)
            .with_context(|| format!("reading {}", src.display()))?;
        println!("{}", src.display());
        println!("  {}", g.describe());
        println!(
            "  bounds {:.6},{:.6} .. {:.6},{:.6}",
            g.bounds.min_lat, g.bounds.min_lon, g.bounds.max_lat, g.bounds.max_lon
        );
        for t in g.tracks.iter().chain(&g.routes) {
            println!("  {:<28} {} points", t.name, t.points.len());
        }
        if let Some(p) = a.flags.get("out") {
            std::fs::write(p, serde_json::to_vec_pretty(&g.to_geojson())?)?;
            println!("  -> {p} (GeoJSON)");
        }
        return Ok(());
    }
    let t = swath_core::tiff::GeoTiff::open(&src)
        .with_context(|| format!("reading {}", src.display()))?;
    println!("{}", src.display());
    println!("  {}", t.describe());
    println!(
        "  EPSG:{}  {}",
        t.epsg.map(|e| e.to_string()).unwrap_or_else(|| "?".into()),
        t.crs_name
    );
    let (ulx, uly) = t.pixel_to_model(0.0, 0.0);
    let (lrx, lry) = t.pixel_to_model(t.width as f64 - 1.0, t.height as f64 - 1.0);
    println!("  model {ulx:.4},{uly:.4} .. {lrx:.4},{lry:.4}");
    if let Some(nd) = t.nodata {
        println!("  nodata {nd}");
    }
    let out = a
        .flags
        .get("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| src.with_extension("swl"));
    let zoom = a.flags.get("zoom").and_then(|z| z.parse().ok());
    let start = std::time::Instant::now();
    let mut last = 0usize;
    let h = swath_core::layer::import_tiff(
        &src,
        &out,
        zoom,
        Some(&mut |done, total| {
            let pct = done * 100 / total.max(1);
            if pct >= last + 10 {
                last = pct;
                eprint!("\r  resampling {pct}%   ");
            }
        }),
    )?;
    eprintln!("\r                      ");
    println!(
        "  -> {} ({:.1} MB) {}x{} at z{}, {:.1}% filled, {:.1}s",
        out.display(),
        std::fs::metadata(&out)?.len() as f64 / 1e6,
        h.width,
        h.height,
        h.base_zoom,
        h.filled as f64 / (h.width * h.height) as f64 * 100.0,
        start.elapsed().as_secs_f64()
    );
    println!("  values {:.3} .. {:.3}  (2-98%: {:.3} .. {:.3})", h.vmin, h.vmax, h.p2, h.p98);
    println!(
        "  bounds {:.6},{:.6} .. {:.6},{:.6}",
        h.bounds.min_lat, h.bounds.min_lon, h.bounds.max_lat, h.bounds.max_lon
    );
    if let Some(pv) = a.flags.get("preview") {
        let r = swath_core::layer::LayerRaster::load(&out)?;
        let mut style = swath_core::layer::LayerStyle::default();
        if let Some(name) = a.flags.get("ramp") {
            style.ramp = swath_core::layer::Ramp::parse(name)
                .ok_or_else(|| anyhow::anyhow!("no ramp called {name}"))?;
        }
        if let Some(v) = a.flags.get("shade") {
            style.shade = v.parse()?;
        }
        // Cut the tiles a viewer would ask for at a zoom that fits the whole
        // layer in about 1200 px, and paste them into one image. This is the
        // real tile path, not a separate renderer, so what it shows is what the
        // chart will show.
        let want = 1200.0;
        let mut z = h.base_zoom;
        while z > 1 {
            let (x0, y1) = swath_core::geo::lonlat_to_px(
                h.bounds.min_lon, h.bounds.min_lat, z as f64);
            let (x1, y0) = swath_core::geo::lonlat_to_px(
                h.bounds.max_lon, h.bounds.max_lat, z as f64);
            if (x1 - x0).max(y1 - y0) <= want {
                break;
            }
            z -= 1;
        }
        let (fx0, fy1) = swath_core::geo::lonlat_to_px(
            h.bounds.min_lon, h.bounds.min_lat, z as f64);
        let (fx1, fy0) = swath_core::geo::lonlat_to_px(
            h.bounds.max_lon, h.bounds.max_lat, z as f64);
        let tx0 = (fx0 / 256.0).floor() as i64;
        let tx1 = (fx1 / 256.0).floor() as i64;
        let ty0 = (fy0 / 256.0).floor() as i64;
        let ty1 = (fy1 / 256.0).floor() as i64;
        let (w, hgt) = (((tx1 - tx0 + 1) * 256) as usize, ((ty1 - ty0 + 1) * 256) as usize);
        let mut img = vec![0u8; w * hgt * 4];
        let mut tiles = 0;
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let Some(t) = r.tile(z, tx, ty, &style) else { continue };
                tiles += 1;
                for row in 0..256 {
                    let dy = (ty - ty0) as usize * 256 + row;
                    let dx = (tx - tx0) as usize * 256;
                    let d = (dy * w + dx) * 4;
                    img[d..d + 256 * 4].copy_from_slice(&t[row * 256 * 4..(row + 1) * 256 * 4]);
                }
            }
        }
        std::fs::write(pv, swath_core::mosaic::encode_png_rgba(&img, w, hgt)?)?;
        println!("  preview z{z} {w}x{hgt} from {tiles} tiles -> {pv}");
    }
    Ok(())
}

fn mosaic(a: &Args) -> Result<()> {
    let root = root_of(a);
    let ws = Workspace::new(&root);
    let name = dataset_arg(a)?;
    let idx = PingIndex::load(ws.index_path(&name))
        .with_context(|| format!("{name} is not indexed; run `survey index {name}`"))?;
    let subs: Vec<u8> = match a.flags.get("subsystem") {
        Some(s) => vec![s.parse()?],
        None => idx.subsystems(),
    };
    for sub in subs {
        let mut cfg = MosaicConfig { subsystem: sub, nav: NavConfig::default(), ..Default::default() };
        if let Some(z) = a.flags.get("zoom") {
            cfg.base_zoom = z.parse()?;
        }
        if let Some(l) = a.flags.get("layback") {
            cfg.nav.layback_m = l.parse()?;
        }
        if let Some(m) = a.flags.get("model") {
            cfg.nav.model = serde_json::from_value(serde_json::json!(m))
                .with_context(|| format!("unknown layback model {m}"))?;
        }
        // Same digest the viewer uses, so a mosaic painted here is the one the
        // window picks up rather than a second copy under a different name.
        let out = ws.mosaic_path(&name, sub, &swath_core::mosaic::key(&cfg)[..12]);
        if out.exists() && !a.flags.contains_key("force") {
            println!("subsystem {sub}: already built with these settings, use --force");
            continue;
        }
        println!("subsystem {sub}: painting…");
        let t = std::time::Instant::now();
        let m = Mosaic::build(&idx, &cfg)?;
        std::fs::create_dir_all(ws.out_dir(&name))?;
        m.save(&out)?;
        println!(
            "  {}x{} at z{}, {} pings, {:.1}s -> {}",
            m.header.width,
            m.header.height,
            m.header.base_zoom,
            m.header.pings,
            t.elapsed().as_secs_f64(),
            out.display()
        );
    }
    Ok(())
}

fn info(a: &Args) -> Result<()> {
    let root = root_of(a);
    let ws = Workspace::new(&root);
    let name = dataset_arg(a)?;
    let idx = PingIndex::load(ws.index_path(&name))
        .with_context(|| format!("{name} is not indexed; run `survey index {name}`"))?;
    let (t0, t1) = idx.time_range().unwrap_or((0.0, 0.0));
    println!("{name}");
    println!("  files      {}", idx.header.files.len());
    println!("  pings      {}", idx.len());
    println!("  start      {}", iso8601(t0));
    println!("  duration   {}", hms(t1 - t0));
    if let Some(b) = idx.bounds() {
        println!("  bounds     {:.6}, {:.6} to {:.6}, {:.6}", b.min_lat, b.min_lon, b.max_lat, b.max_lon);
        let (lat, lon) = b.centre();
        let epsg = swath_core::crs::utm_epsg_for(lat, lon);
        println!("  local grid EPSG:{epsg}");
    }
    for (s, c) in idx.channels() {
        let n = idx.records.iter().filter(|r| r.subsystem == s && r.channel == c).count();
        println!("  subsystem {s} channel {c}: {n} pings");
    }
    let nav = swath_core::nav::Nav::build(&idx.records, NavConfig::default());
    let segs = nav::detect_lines(&nav.track, 0.9, 180.0, 20.0);
    let lines: Vec<_> = segs.iter().filter(|s| s.kind == SegmentKind::Line).collect();
    println!(
        "  run lines  {} ({:.2} km)",
        lines.len(),
        lines.iter().map(|s| s.length_m).sum::<f64>() / 1000.0
    );
    Ok(())
}

/// The search plan, from the command line.
///
/// Goes through the same routes the viewer uses, so what is printed here and
/// what is drawn there cannot drift apart.
fn plan(a: &Args) -> Result<()> {
    let root = root_of(a);
    let name = a.rest.first().cloned().context("project name required")?;
    let state = State::new(&root);
    let resp = swath_core::api::handle(
        &state,
        "POST",
        "/api/project/open",
        &Default::default(),
        serde_json::to_vec(&serde_json::json!({ "name": name }))?.as_slice(),
    );
    if resp.status != 200 {
        bail!("could not open project {name}");
    }
    if let Some(step) = a.flags.get("step").and_then(|s| s.parse::<f64>().ok()) {
        let mut g = state.project.write().unwrap();
        if let Some(p) = g.as_mut() {
            p.plan.spec.step_deg = step;
        }
    }

    // `handle` takes the path and the query separately, so a `?` here has to
    // be split off rather than passed through as part of the path.
    let get = |target: &str| -> serde_json::Value {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut q = std::collections::HashMap::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            q.insert(k.to_string(), v.to_string());
        }
        let resp = swath_core::api::handle(&state, "GET", path, &q, &[]);
        if resp.status != 200 {
            eprintln!("{path}: {}", String::from_utf8_lossy(&resp.bytes()));
            std::process::exit(1);
        }
        serde_json::from_slice(&resp.bytes()).unwrap_or(serde_json::Value::Null)
    };

    let head = get("/api/plan");
    let targets = head["targets"].as_array().cloned().unwrap_or_default();
    if targets.is_empty() {
        bail!(
            "nothing to search for. Add the positions as contacts -- \"Datum...\" in the viewer takes \n\
             a typed one -- then tick them in the Plan pane. A project that has never been planned \n\
             searches for every contact whose source is `datum`."
        );
    }
    println!("{name} -- {} target{}", targets.len(), if targets.len() == 1 { "" } else { "s" });
    for t in &targets {
        println!(
            "  {:<12} {:.6}, {:.6}  +/- {:.0} m",
            t["name"].as_str().unwrap_or(""),
            t["lat"].as_f64().unwrap_or(0.0),
            t["lon"].as_f64().unwrap_or(0.0),
            t["radius_m"].as_f64().unwrap_or(0.0)
        );
    }

    if let Some(az) = a.flags.get("az").and_then(|s| s.parse::<f64>().ok()) {
        let p = get(&format!("/api/plan/solve?az={az}"));
        println!("\n{}", p["digest"].as_str().unwrap_or(""));
        println!(
            "\n  {:>5} lines, {:.0} m apart ({} outside the box)",
            p["lines"].as_u64().unwrap_or(0),
            p["spacing_m"].as_f64().unwrap_or(0.0),
            p["outer_lines"].as_u64().unwrap_or(0)
        );
        let none = p["coverage"]["none"].as_f64().unwrap_or(0.0) * 100.0;
        let twice = p["coverage"]["twice"].as_f64().unwrap_or(0.0) * 100.0;
        println!("  coverage: {none:.1}% unseen, {twice:.0}% seen twice or more");
        if p["teardrops"].as_u64().unwrap_or(0) > 0 {
            println!(
                "  turns: {} of {} loop out, up to {:.0} m past the ends of the lines",
                p["teardrops"].as_u64().unwrap_or(0),
                p["turns"].as_u64().unwrap_or(0),
                p["overshoot_m"].as_f64().unwrap_or(0.0)
            );
        }
        for t in p["targets"].as_array().cloned().unwrap_or_default() {
            println!(
                "  {:<12} {} look{}, {} -- {}",
                t["name"].as_str().unwrap_or(""),
                t["looks"].as_u64().unwrap_or(0),
                if t["looks"].as_u64() == Some(1) { "" } else { "s" },
                if t["both_aspects"].as_bool().unwrap_or(false) { "both aspects" } else { "one side" },
                t["verdict"].as_str().unwrap_or("")
            );
        }
    } else {
        println!("\n   az   lines  spacing   distance       time   unseen  targets");
        for r in head["quadrant"].as_array().cloned().unwrap_or_default() {
            println!(
                "  {:03.0}   {:>5}   {:>4.0} m   {:>6.2} km   {:>10}   {:>5.1}%  {}",
                r["azimuth_deg"].as_f64().unwrap_or(0.0),
                r["lines"].as_u64().unwrap_or(0),
                r["spacing_m"].as_f64().unwrap_or(0.0),
                r["distance_m"].as_f64().unwrap_or(0.0) / 1000.0,
                swath_core::plan::hms(r["seconds"].as_f64().unwrap_or(0.0)),
                r["coverage_none"].as_f64().unwrap_or(0.0) * 100.0,
                r["worst"].as_str().unwrap_or("")
            );
        }
    }

    if a.flags.contains_key("export") {
        let body = serde_json::json!({});
        let resp = swath_core::api::handle(
            &state,
            "POST",
            "/api/plan/export",
            &Default::default(),
            serde_json::to_vec(&body)?.as_slice(),
        );
        let status = resp.status;
        let v: serde_json::Value = serde_json::from_slice(&resp.bytes()).unwrap_or_default();
        if status != 200 {
            bail!("export failed: {}", v["error"].as_str().unwrap_or("unknown"));
        }
        println!("\n-> {}", v["dir"].as_str().unwrap_or(""));
        for f in v["files"].as_array().cloned().unwrap_or_default() {
            println!("   {}", f["file"].as_str().unwrap_or(""));
        }
    }
    Ok(())
}

fn report(a: &Args) -> Result<()> {
    let root = root_of(a);
    let name = a.rest.first().cloned().context("project name required")?;
    let state = State::new(&root);
    let resp = swath_core::api::handle(
        &state,
        "POST",
        "/api/project/open",
        &Default::default(),
        serde_json::to_vec(&serde_json::json!({ "name": name }))?.as_slice(),
    );
    if resp.status != 200 {
        bail!("could not open project {name}");
    }
    // datasets have to be loaded for the report to describe them
    let names: Vec<String> = state
        .project
        .read()
        .unwrap()
        .as_ref()
        .map(|p| p.datasets.iter().filter(|d| d.enabled).map(|d| d.name.clone()).collect())
        .unwrap_or_default();
    for n in names {
        if let Err(e) = state.load(&n, None, None) {
            eprintln!("  warning: {n}: {e}");
        }
    }
    let resp = swath_core::api::handle(&state, "GET", "/api/report", &Default::default(), &[]);
    let html = String::from_utf8_lossy(&resp.bytes()).into_owned();
    match a.flags.get("out") {
        Some(p) => {
            std::fs::write(p, &html)?;
            println!("-> {p}");
        }
        None => print!("{html}"),
    }
    Ok(())
}
