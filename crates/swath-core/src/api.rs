//! One router, two shells.
//!
//! The desktop app and the headless server answer exactly the same requests,
//! because both dispatch into `handle` here. Nothing in this module knows what
//! a socket is; it takes a method, a path, a query map and a body, and returns
//! bytes plus a content type.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::crs;
use crate::geo;
use crate::gpx::Gpx;
use crate::index::PingIndex;
use crate::layer::{LayerRaster, LayerStyle, Ramp};
use crate::mosaic::{Mosaic, MosaicConfig, MosaicStyle};
use crate::nav::{self, Nav, NavConfig, SegmentKind};
use crate::plan::{self, GpxOptions, PlanSpec};
use crate::project::{
    Contact, Contacts, Layer, PlanState, Project, Workspace, KIND_RASTER, KIND_VECTOR,
};
use crate::report::{self, DatasetSummary, LegendEntry, ReportInput};
use crate::waterfall::{self, Waterfall, WaterfallRequest};

/// Mosaics kept on disk per subsystem before the oldest is dropped.
const MOSAIC_KEEP: usize = 4;

/// Waterfall blocks held in memory. At the default block size that is roughly
/// a quarter of a gigabyte of decoded imagery, which buys about forty screens
/// of scrolling in either direction.
const BLOCK_CACHE: usize = 24;

pub enum Body {
    Json(Value),
    Png(Vec<u8>),
    Html(String),
    Text(String),
    /// Bytes with a content type chosen by the caller, for static files.
    Raw(Vec<u8>, &'static str),
    Empty,
}

pub struct Response {
    pub status: u16,
    pub body: Body,
    /// Seconds a client may cache this. Tiles are immutable for a given build.
    pub cache_s: u32,
}

impl Response {
    pub fn json(v: Value) -> Response {
        Response { status: 200, body: Body::Json(v), cache_s: 0 }
    }
    pub fn png(b: Vec<u8>, cache_s: u32) -> Response {
        Response { status: 200, body: Body::Png(b), cache_s }
    }
    /// Asked for, not here yet -- a base map tile being fetched in the
    /// background. A blank so anything treating it as an image sees one, and a
    /// status the viewer can tell apart from a square that is genuinely empty.
    pub fn pending() -> Response {
        Response { status: 202, body: Body::Png(BLANK_PNG.to_vec()), cache_s: 0 }
    }
    pub fn html(s: String) -> Response {
        Response { status: 200, body: Body::Html(s), cache_s: 0 }
    }
    pub fn raw(bytes: Vec<u8>, ct: &'static str, cache_s: u32) -> Response {
        Response { status: 200, body: Body::Raw(bytes, ct), cache_s }
    }
    pub fn err(status: u16, msg: impl Into<String>) -> Response {
        Response { status, body: Body::Json(json!({ "error": msg.into() })), cache_s: 0 }
    }
    /// The same body under a different status, for an answer that is a refusal
    /// and a suggestion at once.
    pub fn with_status(mut self, status: u16) -> Response {
        self.status = status;
        self
    }
    pub fn not_found() -> Response {
        Response { status: 404, body: Body::Empty, cache_s: 0 }
    }
    pub fn content_type(&self) -> &'static str {
        match self.body {
            Body::Json(_) => "application/json",
            Body::Png(_) => "image/png",
            Body::Html(_) => "text/html; charset=utf-8",
            Body::Text(_) => "text/plain; charset=utf-8",
            Body::Raw(_, ct) => ct,
            Body::Empty => "text/plain",
        }
    }
    pub fn bytes(self) -> Vec<u8> {
        match self.body {
            Body::Json(v) => serde_json::to_vec(&v).unwrap_or_default(),
            Body::Png(b) => b,
            Body::Html(s) | Body::Text(s) => s.into_bytes(),
            Body::Raw(b, _) => b,
            Body::Empty => Vec::new(),
        }
    }
}

/// A recording, loaded and ready to draw.
pub struct Loaded {
    pub name: String,
    pub index: PingIndex,
    pub nav: Nav,
    /// The recording's rows, seen from each band. Every band has the same
    /// number of them and row `i` is the same instant in all of them; see
    /// `waterfall::pair_bands`.
    pub pairs: HashMap<u8, Vec<waterfall::Row>>,
    /// One slot per subsystem. The map is only ever locked to hand a slot out;
    /// the painting itself happens under the slot's own gate, so loading
    /// subsystem 20 no longer holds up a tile of subsystem 21.
    pub mosaics: RwLock<HashMap<u8, Arc<MosaicSlot>>>,
    pub cfg: NavConfig,
    /// Mosaic settings for this recording. The subsystem field is a
    /// placeholder; `mosaic_cfg` fills it in per channel.
    pub mosaic_cfg: MosaicConfig,
    /// Where the recording's files are, for the things the index does not
    /// carry -- the true frequency of each band, for one.
    pub dir: Option<std::path::PathBuf>,
    /// Centre frequency per subsystem, Hz, recovered once at load.
    ///
    /// Cached because `scan_bands` opens the XTF headers to unwrap the JSF's
    /// sweep field, and `mosaic_cfg` -- which now needs the frequency, for
    /// absorption -- is called on the tile path.
    pub band_centre_hz: HashMap<u8, f64>,
    /// The bands, resolved once at load.
    pub bands: Vec<crate::index::Band>,
    /// Median and 95th-percentile disagreement between the three layback
    /// models, metres. See `nav::model_spread_m`.
    pub position_spread_m: (f64, f64),
    /// The recording's own waterfall gain model, one per drawing style.
    ///
    /// Measured lazily and kept: it costs a decode of about twelve hundred
    /// pings per range setting, which is worth paying once for a recording and
    /// not once for each of its blocks. See `waterfall::GainModel`.
    pub gains: RwLock<HashMap<String, Arc<waterfall::GainModel>>>,
    /// Held while a gain model is being measured, so the burst of block
    /// requests a fresh waterfall fires does not measure the same one four
    /// times over.
    pub gain_gate: Mutex<()>,
}

/// A subsystem's mosaic, and the right to be the one who paints it.
///
/// Two locks rather than one because they answer different questions. `ready`
/// is read on every tile and must never be held across work. `gate` is held
/// across the load or the paint, so twenty tile requests arriving together
/// cost one build -- but it is per subsystem, and it is not the map.
#[derive(Default)]
pub struct MosaicSlot {
    ready: RwLock<Option<Arc<Mosaic>>>,
    gate: Mutex<()>,
}

impl MosaicSlot {
    /// The mosaic here, if it was painted with these settings.
    fn matching(&self, cfg: &MosaicConfig) -> Option<Arc<Mosaic>> {
        self.ready.read().unwrap().as_ref().filter(|m| m.header.matches(cfg)).cloned()
    }
}

impl Loaded {
    /// The settings a mosaic of this subsystem would be built with.
    /// The frequency bands, with the JSF's wrapped sweep field repaired from
    /// the XTF the same acquisition wrote beside it where that is possible.
    /// Cheap: a few file headers, read once per load.
    fn scan_bands(&self) -> Vec<crate::index::Band> {
        let mut bands = self.index.bands();
        let mut observed: Vec<f64> = Vec::new();
        if let Some(dir) = &self.dir {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.filter_map(|e| e.ok()) {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str())
                        .is_none_or(|x| !x.eq_ignore_ascii_case("xtf")) {
                        continue;
                    }
                    if let Ok(f) = crate::xtf::XtfFile::open(&p) {
                        observed.extend(f.header.frequency_hz.iter().copied()
                            .filter(|v| *v > 0.0));
                    }
                }
            }
        }
        crate::index::recover_band_centres(&mut bands, &observed);
        bands
    }

    pub fn bands(&self) -> &[crate::index::Band] {
        &self.bands
    }

    pub fn mosaic_cfg(&self, subsystem: u8) -> MosaicConfig {
        MosaicConfig {
            subsystem,
            nav: self.cfg,
            centre_freq_hz: self.band_centre_hz.get(&subsystem).copied().unwrap_or(0.0),
            ..self.mosaic_cfg.clone()
        }
    }

    /// Cache key for that mosaic, in the file name and in the tile URL.
    ///
    /// The viewer appends it to every tile request, so re-solving the
    /// navigation changes the URL and the browser fetches the new imagery
    /// instead of showing the cached raster from the previous layback.
    pub fn mosaic_key(&self, subsystem: u8) -> String {
        crate::mosaic::key(&self.mosaic_cfg(subsystem))[..12].to_string()
    }

    pub fn summary(&self) -> Value {
        let b = self.index.bounds();
        let (t0, t1) = self.index.time_range().unwrap_or((0.0, 0.0));
        let segs = nav::detect_lines(&self.nav.track, 0.9, 180.0, 20.0);
        let line_km: f64 = segs
            .iter()
            .filter(|s| s.kind == SegmentKind::Line)
            .map(|s| s.length_m)
            .sum::<f64>()
            / 1000.0;
        json!({
            "name": self.name,
            "pings": self.index.len(),
            "channels": self.index.channels().iter().map(|(s, c)| json!([s, c])).collect::<Vec<_>>(),
            "subsystems": self.index.subsystems(),
            "bands": self.bands(),
            "t0": t0, "t1": t1,
            "duration_s": t1 - t0,
            "bounds": b,
            "lines": segs.iter().filter(|s| s.kind == SegmentKind::Line).count(),
            "line_km": line_km,
            "nav": self.cfg,
            "sound_speed_m_s": self.mosaic_cfg.sound_speed_m_s,
            "recorded_sound_speed_m_s": crate::C_RECORDED,
            "position_spread_m": [self.position_spread_m.0, self.position_spread_m.1],
            "altitude_m": median_altitude(self),
            "mosaic_rev": self.index.subsystems().iter()
                .map(|s| (s.to_string(), Value::from(self.mosaic_key(*s))))
                .collect::<serde_json::Map<_, _>>(),
            "warnings": self.index.header.warnings,
            "files": self.index.header.files,
        })
    }
}

pub struct State {
    pub ws: Workspace,
    pub loaded: RwLock<HashMap<String, Arc<Loaded>>>,
    pub project: RwLock<Option<Project>>,
    pub contacts: RwLock<Contacts>,
    /// Base map tiles, cached on disk and fetched on demand.
    pub tiles: crate::api::TileCache,
    /// Rendered waterfall blocks, kept so scrolling back over ground the
    /// operator has already seen costs nothing, and so a click can be resolved
    /// against the exact image they were looking at.
    pub waterfalls: RwLock<BlockCache>,
    /// Imported layers, by layer id. Rasters are memory mapped, so holding one
    /// open costs an address range rather than the file.
    pub rasters: RwLock<HashMap<String, Arc<LayerRaster>>>,
    /// One gate per layer id, held while that layer is resampled. Separate
    /// from `rasters` so the resample never blocks a lookup.
    pub imports: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pub vectors: RwLock<HashMap<String, Arc<Gpx>>>,
}

/// One rendered block: the image, and the metadata that turns it into
/// positions.
pub struct Block {
    pub wf: Waterfall,
    pub png: Vec<u8>,
}

/// Bounded, in-memory, least-recently-used.
///
/// The viewer keeps several blocks either side of the visible window so that
/// scrolling never waits on a render. That only helps if the ones behind it
/// survive a change of direction, and only stays affordable if the ones far
/// away do not.
#[derive(Default)]
pub struct BlockCache {
    map: HashMap<String, Arc<Block>>,
    /// Keys oldest-used first.
    order: Vec<String>,
}

impl BlockCache {
    pub fn get(&mut self, key: &str) -> Option<Arc<Block>> {
        let b = self.map.get(key)?.clone();
        self.touch(key);
        Some(b)
    }

    /// Look without promoting, for callers that only need to test presence.
    pub fn peek(&self, key: &str) -> Option<Arc<Block>> {
        self.map.get(key).cloned()
    }

    fn touch(&mut self, key: &str) {
        self.order.retain(|k| k != key);
        self.order.push(key.to_string());
    }

    pub fn insert(&mut self, key: String, b: Arc<Block>) {
        self.map.insert(key.clone(), b);
        self.touch(&key);
        while self.order.len() > BLOCK_CACHE {
            let old = self.order.remove(0);
            self.map.remove(&old);
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl State {
    pub fn new(root: impl Into<PathBuf>) -> State {
        let ws = Workspace::new(root);
        let tiles = TileCache::new(ws.root.join("out").join("tilecache"));
        let state = State {
            ws,
            loaded: RwLock::new(HashMap::new()),
            project: RwLock::new(None),
            contacts: RwLock::new(Contacts::default()),
            tiles,
            waterfalls: RwLock::new(BlockCache::default()),
            rasters: RwLock::new(HashMap::new()),
            imports: Mutex::new(HashMap::new()),
            vectors: RwLock::new(HashMap::new()),
        };
        // Reopen whatever was open when the app last closed. The window should
        // come back to the work, not to an empty chart.
        if let Some(name) = state.last_project() {
            if let Ok(p) = Project::load(state.ws.project_path(&name)) {
                *state.contacts.write().unwrap() =
                    Contacts::load(state.ws.contacts_path(&name)).unwrap_or_else(|e| {
                        eprintln!("contacts: {e}");
                        Contacts::sealed()
                    });
                *state.project.write().unwrap() = Some(p);
            }
        }
        state
    }

    /// Recordings in view right now: the open project's own first, then the
    /// shared pool. Which recordings exist depends on which project is open,
    /// so this is the one place that asks.
    pub fn datasets(&self) -> Vec<crate::project::Dataset> {
        let p = self.project.read().unwrap();
        self.ws.datasets_for(p.as_ref().map(|p| p.name.as_str()))
    }

    fn last_project_path(&self) -> PathBuf {
        self.ws.root.join("out").join("last-project")
    }

    fn last_project(&self) -> Option<String> {
        let s = std::fs::read_to_string(self.last_project_path()).ok()?;
        let s = s.trim().to_string();
        (!s.is_empty() && self.ws.project_path(&s).exists()).then_some(s)
    }

    fn remember_project(&self, name: &str) {
        let p = self.last_project_path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, name);
    }

    /// Load a dataset, building the index if it is not on disk yet.
    pub fn load(
        &self,
        name: &str,
        cfg: Option<NavConfig>,
        mosaic: Option<MosaicConfig>,
    ) -> io::Result<Arc<Loaded>> {
        let cfg = cfg.unwrap_or_default();
        let mosaic = mosaic.unwrap_or_default();
        if let Some(l) = self.loaded.read().unwrap().get(name) {
            // a nav change means the fixes move, so rebuild rather than reuse
            if crate::fingerprint(&l.cfg) == crate::fingerprint(&cfg)
                && crate::fingerprint(&l.mosaic_cfg) == crate::fingerprint(&mosaic)
            {
                return Ok(l.clone());
            }
        }
        let idx_path = self.ws.index_path(name);
        let index = if idx_path.exists() {
            PingIndex::load(&idx_path)?
        } else {
            let dir = self
                .datasets()
                .into_iter()
                .find(|d| d.name == name)
                .map(|d| d.dir)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no dataset {name}")))?;
            let files = self.ws.sonar_files(&dir);
            if files.is_empty() {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no sonar files"));
            }
            let idx = PingIndex::build(&files)?;
            std::fs::create_dir_all(self.ws.out_dir(name))?;
            idx.save(&idx_path)?;
            idx
        };
        let nav = Nav::build(&index.records, cfg);
        // One pass over the index for every band at once, so they cannot come
        // out on different row spaces.
        let pairs: HashMap<u8, Vec<waterfall::Row>> =
            waterfall::pair_bands(&index).into_iter().collect();
        let dir = self.datasets().into_iter().find(|d| d.name == name).map(|d| d.dir);
        let mut l = Loaded {
            name: name.to_string(),
            index,
            nav,
            pairs,
            mosaics: RwLock::new(HashMap::new()),
            cfg,
            mosaic_cfg: mosaic,
            dir,
            band_centre_hz: HashMap::new(),
            bands: Vec::new(),
            position_spread_m: (f64::NAN, f64::NAN),
            gains: RwLock::new(HashMap::new()),
            gain_gate: Mutex::new(()),
        };
        l.bands = l.scan_bands();
        l.band_centre_hz = l.bands.iter()
            .filter(|b| b.centre_hz > 0.0)
            .map(|b| (b.subsystem, b.centre_hz as f64))
            .collect();
        l.position_spread_m = nav::model_spread_m(&l.index.records, l.cfg);
        let l = Arc::new(l);
        self.loaded.write().unwrap().insert(name.to_string(), l.clone());
        Ok(l)
    }

    /// The mosaic for a channel, built if this exact set of settings has not
    /// been built before.
    ///
    /// The old version keyed the cache on the subsystem alone, which meant a
    /// change of layback re-solved the navigation, moved the track, and then
    /// loaded the mosaic that had been painted with the *previous* navigation
    /// straight back off disk. The imagery and the track it was drawn from
    /// disagreed, and nothing said so.
    pub fn mosaic(&self, ds: &Arc<Loaded>, subsystem: u8) -> io::Result<Arc<Mosaic>> {
        let cfg = ds.mosaic_cfg(subsystem);
        // The map is locked to find the slot and released immediately. What
        // used to happen here was that the write lock was taken for the whole
        // load, so an eighty-megabyte read off a cold page cache stalled every
        // tile of every channel behind it -- measured at five seconds for
        // subsystem 20, then four more for 21, which waited its turn for no
        // reason at all. The read is the same length; nothing else waits on it.
        let slot = {
            let have = ds.mosaics.read().unwrap().get(&subsystem).cloned();
            match have {
                Some(s) => s,
                None => ds
                    .mosaics
                    .write()
                    .unwrap()
                    .entry(subsystem)
                    .or_insert_with(|| Arc::new(MosaicSlot::default()))
                    .clone(),
            }
        };
        if let Some(m) = slot.matching(&cfg) {
            return Ok(m);
        }
        // A pan over fresh ground fires a dozen tile requests at once, and
        // without this every one of them would start its own build of the same
        // mosaic. The gate is held across the build so the first request does
        // the work and the rest wait for it -- which they would have done
        // anyway, but now only once, and only for this channel.
        let _painting = slot.gate.lock().unwrap();
        // Whoever held the gate may have painted exactly what this request
        // wanted while it was waiting.
        if let Some(m) = slot.matching(&cfg) {
            return Ok(m);
        }
        let path = self.ws.mosaic_path(&ds.name, subsystem, &ds.mosaic_key(subsystem));
        // The digest is in the file name, but check the header too: a file can
        // outlive the meaning of its name across a format change, and painting
        // is cheap compared with showing the wrong seabed.
        let cached = path
            .exists()
            .then(|| Mosaic::load(&path).ok())
            .flatten()
            .filter(|m| m.header.matches(&cfg));
        let m = match cached {
            Some(m) => m,
            None => {
                let m = Mosaic::build(&ds.index, &cfg)?;
                std::fs::create_dir_all(self.ws.out_dir(&ds.name))?;
                m.save(&path)?;
                self.ws.prune_mosaics(&ds.name, subsystem, MOSAIC_KEEP);
                m
            }
        };
        let m = Arc::new(m);
        *slot.ready.write().unwrap() = Some(m.clone());
        Ok(m)
    }

    /// The layer with this id in the open project.
    pub fn layer(&self, id: &str) -> Option<Layer> {
        self.project.read().unwrap().as_ref()?.layers.iter().find(|l| l.id == id).cloned()
    }

    /// Cache name for a resampled source file.
    ///
    /// Derived from the file's identity rather than stored, so editing the
    /// source and re-opening the project resamples it instead of drawing the
    /// old grid under the new name.
    fn layer_key(path: &std::path::Path) -> io::Result<String> {
        let md = std::fs::metadata(path)?;
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(crate::fingerprint(&json!({
            "p": path.to_string_lossy(),
            "n": md.len(),
            "t": mtime,
            "v": 1,
        }))[..16]
            .to_string())
    }

    /// The resampled raster for an imported layer, resampling it if this is the
    /// first time this file has been seen.
    pub fn raster(&self, id: &str) -> io::Result<Arc<LayerRaster>> {
        if let Some(r) = self.rasters.read().unwrap().get(id) {
            return Ok(r.clone());
        }
        let l = self
            .layer(id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such layer"))?;
        let src = PathBuf::from(&l.file);
        let key = Self::layer_key(&src)?;
        let cache = self.ws.layer_cache_path(&key);
        // One importer per layer, so a pan that fires twenty tile requests at
        // once does the work once rather than twenty times. The gate is not the
        // rasters map: resampling a 357 MB GeoTIFF takes seconds, and every
        // other layer's tiles have to read that map to draw anything at all.
        let gate = self.imports.lock().unwrap().entry(id.to_string()).or_default().clone();
        let _importing = gate.lock().unwrap();
        if let Some(r) = self.rasters.read().unwrap().get(id) {
            return Ok(r.clone());
        }
        if !cache.exists() {
            crate::layer::import_tiff(&src, &cache, None, None)?;
        }
        let r = Arc::new(match LayerRaster::load(&cache) {
            Ok(r) => r,
            Err(_) => {
                // A cache written by an older build, or a half-written file.
                let _ = std::fs::remove_file(&cache);
                crate::layer::import_tiff(&src, &cache, None, None)?;
                LayerRaster::load(&cache)?
            }
        });
        self.rasters.write().unwrap().insert(id.to_string(), r.clone());
        Ok(r)
    }

    pub fn vector(&self, id: &str) -> io::Result<Arc<Gpx>> {
        if let Some(g) = self.vectors.read().unwrap().get(id) {
            return Ok(g.clone());
        }
        let l = self
            .layer(id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such layer"))?;
        let g = Arc::new(crate::gpx::read(&l.file)?);
        self.vectors.write().unwrap().insert(id.to_string(), g.clone());
        Ok(g)
    }

    /// The open project's name, for the routes that write into its folder.
    pub fn project_name(&self) -> Option<String> {
        self.project.read().unwrap().as_ref().map(|p| p.name.clone())
    }

    fn save_contacts(&self) -> io::Result<()> {
        let Some(name) = self.project_name() else { return Ok(()) };
        self.contacts.read().unwrap().save(self.ws.contacts_path(&name), &name)
    }
}

// ---- tile cache ------------------------------------------------------------

/// Sources the viewer is allowed to proxy. Nothing else is fetched.
pub const TILE_SOURCES: &[(&str, &str)] = &[
    ("osm", "https://tile.openstreetmap.org/{z}/{x}/{y}.png"),
    ("osmde", "https://tile.openstreetmap.de/{z}/{x}/{y}.png"),
    ("topo", "https://a.tile.opentopomap.org/{z}/{x}/{y}.png"),
    ("seamark", "https://tiles.openseamap.org/seamark/{z}/{x}/{y}.png"),
    ("contour", "https://tiles.openseamap.org/contour/{z}/{x}/{y}.png"),
];

/// A 1x1 transparent PNG, returned when a source has no tile for that square --
/// very common for the OpenSeaMap overlays -- so the browser does not retry it
/// on every pan.
pub const BLANK_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
    0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
    0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
    0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// How long a "this square is empty" answer from upstream is trusted.
///
/// Not forever. A 404 is usually the truth -- OpenSeaMap has nothing to draw on
/// most squares -- but it is also what a rate limit or a bad afternoon looks
/// like, and a blank written permanently is a hole in the chart that no amount
/// of panning will fill.
const MISS_TTL_S: u64 = 24 * 60 * 60;

/// How long ago a blank was written, or `u64::MAX` if that cannot be told --
/// which errs towards fetching again rather than towards a permanent hole.
fn miss_age_s(p: &std::path::Path) -> u64 {
    let Ok(t) = std::fs::metadata(p).and_then(|m| m.modified()) else {
        return u64::MAX;
    };
    t.elapsed().map(|d| d.as_secs()).unwrap_or(0)
}

pub struct TileCache {
    pub root: PathBuf,
}

impl TileCache {
    pub fn new(root: PathBuf) -> TileCache {
        TileCache { root }
    }
    pub fn path(&self, layer: &str, z: u32, x: i64, y: i64) -> PathBuf {
        self.root.join(layer).join(z.to_string()).join(x.to_string()).join(format!("{y}.png"))
    }
    /// Read a tile from disk. Fetching is the shell's job -- the router stays
    /// free of the network so it can be tested without one.
    pub fn get(&self, layer: &str, z: u32, x: i64, y: i64) -> Option<Vec<u8>> {
        if !TILE_SOURCES.iter().any(|(n, _)| *n == layer) {
            return None;
        }
        let p = self.path(layer, z, x, y);
        let b = std::fs::read(&p).ok().filter(|b| !b.is_empty())?;
        // A blank is what upstream says when a square has nothing on it, which
        // for the seamark overlays is most of them -- worth keeping. But it is
        // also what a rate limit looks like, so it is believed for a day and
        // then asked again, rather than leaving a hole in the chart forever.
        if b == BLANK_PNG && miss_age_s(&p) > MISS_TTL_S {
            return None;
        }
        Some(b)
    }
    pub fn put(&self, layer: &str, z: u32, x: i64, y: i64, data: &[u8]) -> io::Result<()> {
        let p = self.path(layer, z, x, y);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, data)
    }
    pub fn url(layer: &str, z: u32, x: i64, y: i64) -> Option<String> {
        TILE_SOURCES.iter().find(|(n, _)| *n == layer).map(|(_, t)| {
            t.replace("{z}", &z.to_string())
                .replace("{x}", &x.to_string())
                .replace("{y}", &y.to_string())
        })
    }
}

// ---- request types ---------------------------------------------------------

#[derive(Deserialize)]
struct LoadReq {
    name: String,
    #[serde(default)]
    nav: Option<NavConfig>,
    /// Mosaic settings for this recording. Absent means the defaults.
    #[serde(default)]
    mosaic: Option<MosaicConfig>,
}

#[derive(Deserialize)]
struct WaterfallReq {
    dataset: String,
    #[serde(flatten)]
    req: WaterfallRequest,
}

#[derive(Deserialize)]
struct PickReq {
    key: String,
    x: f64,
    y: f64,
}

#[derive(Serialize)]
struct TrackOut {
    time: Vec<f64>,
    boat: Vec<[f64; 2]>,
    fish: Vec<[f64; 2]>,
    bearing: Vec<f64>,
    speed: Vec<f64>,
    depth: Vec<f64>,
    altitude: Vec<f64>,
    roll: Vec<f64>,
    clean: Vec<bool>,
}

/// A file name that cannot escape the directory it is written into.
fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// The legend for a chart: the visible layers, in stack order.
///
/// Built by walking the chart's own layer list in the order it was drawn, so
/// the legend and the picture cannot disagree. Not the layer tree: what is
/// ticked on screen is a working view, and a chart in a deliverable is not.
fn legend_for(p: &Project, chart: &crate::project::ChartSpec) -> Vec<LegendEntry> {
    let mut out = Vec::new();
    for cl in &chart.layers {
        if !cl.on {
            continue;
        }
        let Some(l) = p.layers.iter().find(|l| l.id == cl.id) else {
            continue;
        };
        if l.kind == crate::project::KIND_RECORDING {
            continue;
        }
        let ramp_stops = |r: Ramp| -> Vec<String> {
            (0..7)
                .map(|i| {
                    let c = r.at(i as f32 / 6.0);
                    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
                })
                .collect()
        };
        let (colour, ramp, detail) = match l.kind.as_str() {
            crate::project::KIND_MOSAIC => {
                let st: crate::mosaic::MosaicStyle =
                    serde_json::from_value(serde_json::to_value(&l.style).unwrap_or_default())
                        .unwrap_or_default();
                (
                    String::new(),
                    ramp_stops(st.ramp),
                    format!("sidescan backscatter, {} scheme", st.ramp.name()),
                )
            }
            crate::project::KIND_RASTER => (
                String::new(),
                ramp_stops(l.style.ramp),
                if l.info.is_empty() { "imported grid".into() } else { l.info.clone() },
            ),
            // A track layer can draw two lines, and which of them a chart
            // shows is a property of the chart. The legend has to name the one
            // that is actually on the paper.
            crate::project::KIND_TRACK => (
                if l.colour.is_empty() { "#35b8a6".into() } else { l.colour.clone() },
                Vec::new(),
                match (cl.fish, cl.boat) {
                    (true, true) => "towfish and vessel track",
                    (true, false) => "towfish track",
                    (false, true) => "vessel track",
                    (false, false) => continue,
                }
                .into(),
            ),
            _ => (
                if l.colour.is_empty() { "#f0b429".into() } else { l.colour.clone() },
                Vec::new(),
                if l.info.is_empty() { "imported vectors".into() } else { l.info.clone() },
            ),
        };
        out.push(LegendEntry {
            label: if l.label.is_empty() { l.id.clone() } else { l.label.clone() },
            kind: l.kind.clone(),
            colour,
            ramp,
            detail,
        });
    }
    out
}

/// The charts of one kind the report should carry, in spec order.
///
/// The viewer draws them and posts them here under the chart id, so a chart
/// with no image is one that was never rendered -- the report says so rather
/// than silently dropping the section.
fn charts_for(
    p: &Project,
    img: &dyn Fn(&str) -> Option<String>,
    kind: &str,
    dataset: Option<&str>,
) -> Vec<report::Chart> {
    p.report
        .charts
        .iter()
        .filter(|c| c.enabled && c.kind == kind)
        .filter(|c| dataset.map_or(true, |d| c.dataset == d))
        .map(|c| report::Chart {
            title: c.title.clone(),
            subtitle: c.subtitle.clone(),
            image: img(&safe_name(&c.id)),
            legend: legend_for(p, c),
        })
        .collect()
}

/// Contacts in a project, without parsing them.
///
/// A count for a list of projects has to be cheap and must not care whether
/// every mark still reads -- `Contacts::load` refuses a file it cannot fully
/// understand, which is right when opening one and wrong when counting many.
fn count_contacts(path: &std::path::Path) -> usize {
    let Ok(s) = std::fs::read_to_string(path) else { return 0 };
    let Ok(v) = serde_json::from_str::<Value>(&s) else { return 0 };
    v.get("features").and_then(|f| f.as_array()).map(|a| a.len()).unwrap_or(0)
}


/// Which importer, if any, claims a file.
fn importable(p: &std::path::Path) -> Option<&'static str> {
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "tif" | "tiff" | "gtif" | "gtiff" => Some(KIND_RASTER),
        "gpx" => Some(KIND_VECTOR),
        _ => None,
    }
}

/// Overlay the style parameters a tile request carries onto the layer's own.
fn style_from_query(q: &HashMap<String, String>, mut st: LayerStyle) -> LayerStyle {
    if let Some(r) = q.get("ramp").and_then(|r| Ramp::parse(r)) {
        st.ramp = r;
    }
    if let Some(v) = q.get("reverse") {
        st.reverse = v == "1" || v == "true";
    }
    if let Some(v) = q_num::<f64>(q, "min") {
        st.min = Some(v);
    }
    if let Some(v) = q_num::<f64>(q, "max") {
        st.max = Some(v);
    }
    if let Some(v) = q_num::<f64>(q, "shade") {
        st.shade = v.clamp(0.0, 1.0);
    }
    if let Some(v) = q_num::<f64>(q, "azimuth") {
        st.sun_azimuth_deg = v;
    }
    if let Some(v) = q_num::<f64>(q, "elevation") {
        st.sun_elevation_deg = v.clamp(1.0, 89.0);
    }
    if let Some(v) = q_num::<f64>(q, "exaggeration") {
        st.shade_exaggeration = v.max(0.0);
    }
    st
}

fn mosaic_style_from_query(q: &HashMap<String, String>) -> MosaicStyle {
    let mut st = MosaicStyle::default();
    if let Some(r) = q.get("ramp").and_then(|r| Ramp::parse(r)) {
        st.ramp = r;
    }
    if let Some(v) = q.get("reverse") {
        st.reverse = v == "1" || v == "true";
    }
    if let Some(v) = q_num::<f64>(q, "lo") {
        st.lo = v.clamp(0.0, 255.0) as u8;
    }
    if let Some(v) = q_num::<f64>(q, "hi") {
        st.hi = v.clamp(0.0, 255.0) as u8;
    }
    st
}

/// Names a rendered block by everything that determines its pixels.
///
/// The navigation is in there because a re-solve moves every row's position,
/// and a block keyed only by its ping range would come back from the cache
/// still carrying the old fixes.
/// The recording's median flying height, metres.
fn median_altitude(l: &Loaded) -> f64 {
    let mut a: Vec<f64> = l
        .index
        .records
        .iter()
        .filter(|r| r.altitude_valid())
        .map(|r| r.altitude as f64)
        .collect();
    if a.is_empty() {
        return f64::NAN;
    }
    a.sort_by(f64::total_cmp);
    a[a.len() / 2]
}

/// Where on the seabed the main beam actually falls, metres either side.
///
/// The 4125's array is depressed 33 degrees with a 50 degree vertical beam, so
/// it lights the ground between 32 and 82 degrees off vertical. The inner
/// figure is where coverage really begins; inside it the sonar is looking at
/// the seabed with the edge of its beam, which is what the nadir band is. The
/// outer is clipped to how far the traces reach.
fn illuminated_swath(l: &Loaded) -> (f64, f64) {
    const LOBE_NEAR_DEG: f64 = 32.0;
    const LOBE_FAR_DEG: f64 = 82.0;
    let alt = median_altitude(l);
    if !alt.is_finite() || alt <= 0.0 {
        return (f64::NAN, f64::NAN);
    }
    let c = l.mosaic_cfg.sound_speed_m_s;
    let mut reach: Vec<f64> = l.index.records.iter().map(|r| r.slant_range_m(c)).collect();
    reach.sort_by(f64::total_cmp);
    let slant = reach.get(reach.len() / 2).copied().unwrap_or(0.0);
    let ground = (slant * slant - alt * alt).max(0.0).sqrt();
    (
        alt * LOBE_NEAR_DEG.to_radians().tan(),
        ground.min(alt * LOBE_FAR_DEG.to_radians().tan()),
    )
}

fn block_key(dataset: &str, nav: &NavConfig, req: &WaterfallRequest) -> String {
    crate::fingerprint(&json!({ "d": dataset, "n": nav, "r": req }))
}

/// The cache key for a recording's gain model.
///
/// Everything about *how* the waterfall is drawn and nothing about which pings
/// are on screen, which is the whole point of the model: `start`, `count` and
/// `stride` must not appear here or it would be measured per block again.
fn gain_key(dataset: &str, req: &WaterfallRequest) -> String {
    crate::fingerprint(&json!({
        "d": dataset,
        "s": req.subsystem,
        "w": req.width,
        "a": req.axis,
        "t": req.tvg,
        "g": req.agc,
        "y": req.gamma,
        "lo": req.clip_lo,
        "hi": req.clip_hi,
        "m": req.max_range_m,
        "c": req.sound_speed_m_s,
    }))
}

fn q_num<T: std::str::FromStr>(q: &HashMap<String, String>, k: &str) -> Option<T> {
    q.get(k).and_then(|v| v.parse().ok())
}

// ---- the router ------------------------------------------------------------

/// Which picture of a contact this is.
///
/// The chart crop and the waterfall crop answer different questions, and the
/// two bands answer a third one between them: a target that is bright at one
/// frequency and absent at the other is telling you what it is made of.
#[derive(Clone, Copy, PartialEq)]
pub enum SnapKind {
    Map,
    Waterfall,
    Low,
    High,
    WaterfallLow,
    WaterfallHigh,
}

impl SnapKind {
    fn from_query(q: &HashMap<String, String>) -> Option<SnapKind> {
        Some(match q.get("kind").map(String::as_str) {
            None | Some("") | Some("map") => SnapKind::Map,
            Some("waterfall") => SnapKind::Waterfall,
            Some("lf") => SnapKind::Low,
            Some("hf") => SnapKind::High,
            Some("wf-lf") => SnapKind::WaterfallLow,
            Some("wf-hf") => SnapKind::WaterfallHigh,
            Some(_) => return None,
        })
    }

    /// What the file is called, after the contact id.
    fn suffix(self) -> &'static str {
        match self {
            SnapKind::Map => "",
            SnapKind::Waterfall => "-wf",
            SnapKind::Low => "-lf",
            SnapKind::High => "-hf",
            SnapKind::WaterfallLow => "-wf-lf",
            SnapKind::WaterfallHigh => "-wf-hf",
        }
    }

    fn slot(self, c: &mut Contact) -> &mut Option<String> {
        match self {
            SnapKind::Map => &mut c.snapshot,
            SnapKind::Waterfall => &mut c.snapshot_wf,
            SnapKind::Low => &mut c.snapshot_lf,
            SnapKind::High => &mut c.snapshot_hf,
            SnapKind::WaterfallLow => &mut c.snapshot_wf_lf,
            SnapKind::WaterfallHigh => &mut c.snapshot_wf_hf,
        }
    }
}

/// The report, as one self-contained HTML page.
///
/// Built here rather than in the route so the same page can be served to a tab
/// and written to a file. The desktop shell has no tabs to open -- `window.open`
/// there returns null and nothing happens at all -- so "export" has to mean a
/// file on disk, and it may as well mean that everywhere.
pub fn report_html(state: &State) -> Result<String, String> {
    let guard = state.project.read().unwrap();
    let p = guard.as_ref().ok_or("no project open")?;

            let contacts = state.contacts.read().unwrap().items.clone();
            let loaded = state.loaded.read().unwrap();
            let img = |name: &str| -> Option<String> {
                let b = std::fs::read(state.ws.report_dir(&p.name).join(format!("{name}.png"))).ok()?;
                Some(format!("data:image/png;base64,{}", b64(&b)))
            };

            let datasets: Vec<DatasetSummary> = p
                .datasets
                .iter()
                // A recording can be loaded in the viewer but kept out of the
                // report; that is what the tick box in the project dialog does.
                .filter(|d| d.enabled)
                .filter_map(|d| {
                    let l = loaded.get(&d.name)?;
                    let (t0, t1) = l.index.time_range()?;
                    let segs = nav::detect_lines(&l.nav.track, 0.9, 180.0, 20.0);
                    Some(DatasetSummary {
                        name: d.name.clone(),
                        label: d.label.clone(),
                        pings: l.index.len(),
                        t0,
                        t1,
                        line_km: segs.iter().filter(|s| s.kind == SegmentKind::Line)
                            .map(|s| s.length_m).sum::<f64>() / 1000.0,
                        lines: segs.iter().filter(|s| s.kind == SegmentKind::Line).count(),
                        subsystems: l.index.subsystems(),
                        bounds: l.index.bounds(),
                        layback_m: l.cfg.layback_m,
                        sound_speed_m_s: l.mosaic_cfg.sound_speed_m_s,
                        position_spread_m: l.position_spread_m,
                        altitude_m: median_altitude(&l),
                        illuminated_m: illuminated_swath(&l),
                        nav: l.cfg,
                        charts: charts_for(p, &img, "dataset", Some(&d.name)),
                    })
                })
                .collect();

            let mut snaps: Vec<(String, String, String)> = Vec::new();
            for c in &contacts {
                for (kind, f) in [
                    ("map", &c.snapshot),
                    ("waterfall", &c.snapshot_wf),
                    ("lf", &c.snapshot_lf),
                    ("hf", &c.snapshot_hf),
                    ("wf-lf", &c.snapshot_wf_lf),
                    ("wf-hf", &c.snapshot_wf_hf),
                ] {
                    let Some(f) = f else { continue };
                    let Ok(b) = std::fs::read(state.ws.snaps_dir(&p.name).join(f)) else {
                        continue;
                    };
                    snaps.push((c.id.clone(), kind.to_string(),
                                format!("data:image/png;base64,{}", b64(&b))));
                }
            }
            let overviews = charts_for(p, &img, "overview", None);
            let html = report::render(&ReportInput {
                project: p,
                contacts: &contacts,
                datasets: &datasets,
                overviews: &overviews,
                snapshots: snaps,
            });
    Ok(html)
}

/// Write the report beside the project's other output, and say where.
pub fn save_report(state: &State) -> Result<PathBuf, String> {
    let html = report_html(state)?;
    let name = state.project_name().ok_or("no project open")?;
    let dir = state.ws.report_dir(&name);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("report.html");
    std::fs::write(&path, html).map_err(|e| e.to_string())?;
    // Absolute, because the answer is shown to someone who has to find it, and
    // the workspace root is wherever the process happened to be started from.
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}


/// The plan settings and the contacts they are for.
///
/// Both come out of the open project, so every plan route agrees about what is
/// being searched for without the viewer having to send it back each time.
fn plan_inputs(state: &State) -> Result<(PlanState, Vec<plan::Target>), &'static str> {
    let guard = state.project.read().unwrap();
    let Some(p) = guard.as_ref() else { return Err("no project open") };
    let ps = p.plan.clone();
    let targets = ps.targets_from(&state.contacts.read().unwrap().items);
    Ok((ps, targets))
}

/// Everything a plan says about itself, once solved.
fn plan_json(p: &plan::Plan, spec: &PlanSpec) -> Value {
    json!({
        "azimuth": p.azimuth_deg,
        "lines": p.lines,
        "outer_lines": p.outer_lines,
        "spacing_m": p.spacing_m,
        "spacing_requested_m": p.spacing_requested_m,
        "range_m": p.range_m,
        "nadir_m": p.nadir_m,
        "skip": p.skip,
        "box_m": [p.x1 - p.x0, p.a1 - p.a0],
        "line_length_m": (p.a1 - p.a0) + spec.rig.run_in_m.max(0.0) + spec.rig.run_out_m.max(0.0),
        "run_in_m": spec.rig.run_in_m.max(0.0),
        "run_out_m": spec.rig.run_out_m.max(0.0),
        "offset_m": spec.rig.offset_m(),
        "fish_short_m": p.fish_short_m,
        "line_distance_m": p.line_distance_m,
        "turn_distance_m": p.turn_distance_m,
        "distance_m": p.distance_m,
        "seconds": p.seconds,
        "turns": p.turns.len(),
        "teardrops": p.teardrops,
        "overshoot_m": p.overshoot_m,
        "turn_room_m": 2.0 * spec.rig.turn_radius_m,
        "coverage": p.coverage,
        "targets": p.targets,
        "bounds": p.bounds(),
        "digest": p.digest(spec),
        "name": p.default_name(),
    })
}

pub fn handle(
    state: &State,
    method: &str,
    path: &str,
    q: &HashMap<String, String>,
    body: &[u8],
) -> Response {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, seg.as_slice()) {
        // ---- workspace ----
        ("GET", ["api", "state"]) => {
            let datasets: Vec<Value> = state
                .datasets()
                .into_iter()
                .map(|d| {
                    json!({
                        "name": d.name,
                        "path": d.dir.display().to_string(),
                        "indexed": state.ws.index_path(&d.name).exists(),
                        "files": state.ws.sonar_files(&d.dir).len(),
                        // Held by the project, so removing it from the project
                        // is a question about files rather than about a list.
                        "owned": d.owned,
                        "linked": std::fs::symlink_metadata(&d.dir)
                            .map(|m| m.is_symlink())
                            .unwrap_or(false),
                    })
                })
                .collect();
            let loaded: Vec<Value> =
                state.loaded.read().unwrap().values().map(|l| l.summary()).collect();
            Response::json(json!({
                "root": state.ws.root.display().to_string(),
                "version": crate::VERSION,
                "datasets": datasets,
                "projects": state.ws.projects(),
                "project": *state.project.read().unwrap(),
                "loaded": loaded,
                "tile_layers": TILE_SOURCES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            }))
        }

        // ---- project ----
        ("POST", ["api", "project", "open"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            let Some(name) = v.get("name").and_then(|n| n.as_str()) else {
                return Response::err(400, "name required");
            };
            match Project::load(state.ws.project_path(name)) {
                Ok(p) => {
                    let c = Contacts::load(state.ws.contacts_path(name)).unwrap_or_else(|e| {
                        eprintln!("contacts: {e}");
                        Contacts::sealed()
                    });
                    *state.contacts.write().unwrap() = c;
                    // Layer ids are derived from the file path and so are
                    // stable across projects, but holding every raster every
                    // project ever referenced open is not.
                    state.rasters.write().unwrap().clear();
                    state.vectors.write().unwrap().clear();
                    state.remember_project(name);
                    let out = json!(p);
                    *state.project.write().unwrap() = Some(p);
                    Response::json(out)
                }
                Err(e) => Response::err(404, e.to_string()),
            }
        }
        ("POST", ["api", "project", "new"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            let Some(name) = v.get("name").and_then(|n| n.as_str()) else {
                return Response::err(400, "name required");
            };
            let mut p = Project::new(name);
            if let Err(e) = p.save(state.ws.project_path(name)) {
                return Response::err(500, e.to_string());
            }
            *state.contacts.write().unwrap() = Contacts::default();
            state.remember_project(name);
            let out = json!(p);
            *state.project.write().unwrap() = Some(p);
            Response::json(out)
        }
        ("POST", ["api", "project", "save"]) => {
            let Ok(mut p) = serde_json::from_slice::<Project>(body) else {
                return Response::err(400, "bad project");
            };
            let path = state.ws.project_path(&p.name);
            if let Err(e) = p.save(&path) {
                return Response::err(500, e.to_string());
            }
            let out = json!(p);
            *state.project.write().unwrap() = Some(p);
            Response::json(out)
        }

        // Every project in the workspace, with enough of each to choose
        // between them without opening it.
        ("GET", ["api", "projects"]) => {
            let open = state.project_name();
            let list: Vec<Value> = state
                .ws
                .projects()
                .into_iter()
                .map(|name| {
                    let dir = state.ws.project_dir(&name);
                    let p = Project::load(dir.join("project.json")).ok();
                    let holds = state.ws.project_holds(&name);
                    json!({
                        "name": name,
                        "open": open.as_deref() == Some(name.as_str()),
                        "title": p.as_ref().map(|p| p.title.clone()).unwrap_or_default(),
                        "area": p.as_ref().map(|p| p.meta.area.clone()).unwrap_or_default(),
                        "vessel": p.as_ref().map(|p| p.meta.vessel.clone()).unwrap_or_default(),
                        "modified": p.as_ref().map(|p| p.modified.clone()).unwrap_or_default(),
                        "created": p.as_ref().map(|p| p.created.clone()).unwrap_or_default(),
                        "datasets": p.as_ref().map(|p| p.datasets.len()).unwrap_or(0),
                        "contacts": count_contacts(&state.ws.contacts_path(&name)),
                        "holds": holds,
                        "readable": p.is_some(),
                        "path": dir.display().to_string(),
                    })
                })
                .collect();
            Response::json(json!({ "projects": list, "open": open }))
        }
        ("POST", ["api", "project", "rename"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            let from = v.get("from").and_then(|x| x.as_str()).unwrap_or_default();
            let to = v.get("to").and_then(|x| x.as_str()).unwrap_or_default();
            if let Err(e) = state.ws.rename_project(from, to) {
                return Response::err(409, e.to_string());
            }
            // The open project is the one being renamed often enough that
            // leaving the window pointing at a folder that no longer exists is
            // the normal case, not the edge one.
            let mut open = state.project.write().unwrap();
            if open.as_ref().map(|p| p.name.as_str()) == Some(from) {
                if let Ok(p) = Project::load(state.ws.project_path(to)) {
                    *open = Some(p);
                }
                drop(open);
                state.remember_project(to);
            }
            Response::json(json!({ "name": to }))
        }
        ("POST", ["api", "project", "duplicate"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            let from = v.get("from").and_then(|x| x.as_str()).unwrap_or_default();
            let to = v.get("to").and_then(|x| x.as_str()).unwrap_or_default();
            match state.ws.duplicate_project(from, to) {
                Ok(()) => Response::json(json!({ "name": to })),
                Err(e) => Response::err(409, e.to_string()),
            }
        }
        ("POST", ["api", "project", "delete"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or_default();
            let with_data = v.get("with_data").and_then(|x| x.as_bool()).unwrap_or(false);
            if let Err(e) = state.ws.delete_project(name, with_data) {
                return Response::err(409, e.to_string());
            }
            let mut open = state.project.write().unwrap();
            if open.as_ref().map(|p| p.name.as_str()) == Some(name) {
                *open = None;
                *state.contacts.write().unwrap() = Contacts::default();
                state.loaded.write().unwrap().clear();
            }
            Response::json(json!({ "ok": true }))
        }

        // ---- datasets ----
        // Bring a folder of recordings into the open project.
        //
        // The name is checked before anything is written, and the answer says
        // what it ended up being called, because the operator's chosen name can
        // collide with a recording in another project and `out/<name>` is what
        // that would silently share.
        ("POST", ["api", "dataset", "import"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            let Some(project) = state.project_name() else {
                return Response::err(409, "open or create a project first");
            };
            let Some(path) = v.get("path").and_then(|x| x.as_str()) else {
                return Response::err(400, "path required");
            };
            let src = PathBuf::from(path);
            let want = v
                .get("name")
                .and_then(|x| x.as_str())
                .filter(|x| !x.trim().is_empty())
                .map(|x| x.to_string())
                .unwrap_or_else(|| {
                    src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                });
            let name = crate::project::sanitise_name(&want);
            if name.is_empty() {
                return Response::err(400, "a recording needs a name");
            }
            let how: crate::project::Placement =
                match v.get("mode").and_then(|x| x.as_str()).unwrap_or("link") {
                    "copy" => crate::project::Placement::Copy,
                    "move" => crate::project::Placement::Move,
                    "link" => crate::project::Placement::Link,
                    other => return Response::err(400, format!("unknown mode {other}")),
                };
            match state.ws.import_dataset(&project, &src, &name, how) {
                Ok(dir) => Response::json(json!({
                    "name": name,
                    "path": dir.display().to_string(),
                    "files": state.ws.sonar_files(&dir).len(),
                    "mode": how,
                })),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Response::json(json!({
                    "error": e.to_string(),
                    "suggest": state.ws.free_dataset_name(&name),
                }))
                .with_status(409),
                Err(e) => Response::err(422, e.to_string()),
            }
        }
        // Take a recording out of the project's own folder again.
        //
        // The counterpart of `import`, and it exists because without it a
        // recording copied into a project by mistake could only be got rid of
        // from outside the application.
        ("POST", ["api", "dataset", "remove"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            let Some(project) = state.project_name() else {
                return Response::err(409, "no project open");
            };
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or_default();
            if let Err(e) = state.ws.remove_dataset(&project, name) {
                return Response::err(409, e.to_string());
            }
            state.loaded.write().unwrap().remove(name);
            // Everything derived from it is now a picture of nothing.
            if v.get("derived").and_then(|x| x.as_bool()).unwrap_or(true) {
                let _ = std::fs::remove_dir_all(state.ws.out_dir(name));
            }
            Response::json(json!({ "ok": true }))
        }
        // A free name for a folder about to be brought in, so the dialog can
        // offer one rather than having the operator find the clash by hand.
        ("GET", ["api", "dataset", "name"]) => {
            let want = q.get("want").cloned().unwrap_or_default();
            let free = state.ws.free_dataset_name(&want);
            Response::json(json!({
                "name": free,
                "taken": free != crate::project::sanitise_name(&want),
            }))
        }
        ("POST", ["api", "dataset", "load"]) => {
            let Ok(r) = serde_json::from_slice::<LoadReq>(body) else {
                return Response::err(400, "bad request");
            };
            match state.load(&r.name, r.nav, r.mosaic) {
                Ok(l) => Response::json(l.summary()),
                Err(e) => Response::err(500, e.to_string()),
            }
        }
        ("POST", ["api", "dataset", "unload"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
                state.loaded.write().unwrap().remove(n);
            }
            Response::json(json!({ "ok": true }))
        }
        ("GET", ["api", "dataset", name, "track"]) => {
            let Some(l) = state.loaded.read().unwrap().get(*name).cloned() else {
                return Response::err(404, "not loaded");
            };
            let sub: u8 = q_num(q, "subsystem").unwrap_or_else(|| {
                l.index.subsystems().first().copied().unwrap_or(20)
            });
            let max: usize = q_num(q, "max").unwrap_or(4000);
            // one row per ping, decimated to `max` points -- the track is for
            // looking at, and 460k points is not a picture, it is a wait.
            //
            // Any band's rows will do and the `subsystem` parameter no longer
            // changes the answer: there is one fish and it was in one place at
            // each instant, so the track is a property of the recording. It is
            // still accepted, and still reported, so an older client asking by
            // channel gets the same line rather than an error.
            let pairs = l
                .pairs
                .get(&sub)
                .or_else(|| l.pairs.values().next())
                .cloned()
                .unwrap_or_default();
            let stride = (pairs.len() / max.max(1)).max(1);
            let mut out = TrackOut {
                time: Vec::new(),
                boat: Vec::new(),
                fish: Vec::new(),
                bearing: Vec::new(),
                speed: Vec::new(),
                depth: Vec::new(),
                altitude: Vec::new(),
                roll: Vec::new(),
                clean: Vec::new(),
            };
            for p in pairs.iter().step_by(stride) {
                let r = &l.index.records[p.at as usize];
                let f = l.nav.fix(r);
                if !f.lat.is_finite() || !f.boat_lat.is_finite() {
                    continue;
                }
                out.time.push(f.time);
                out.boat.push([f.boat_lat, f.boat_lon]);
                out.fish.push([f.lat, f.lon]);
                out.bearing.push(f.bearing);
                out.speed.push(f.speed);
                out.depth.push(f.depth);
                out.altitude.push(f.altitude);
                out.roll.push(f.roll);
                out.clean.push(f.clean);
            }
            Response::json(json!({
                "dataset": name, "subsystem": sub,
                "stride": stride, "pings": pairs.len(),
                "track": out,
            }))
        }
        // Which ping was closest to a position.
        //
        // This is what links the chart to the waterfall: pan the chart and the
        // waterfall scrolls to the pings that ran over the ground now in view.
        // The answer is an ordinal in the channel's selection, which is exactly
        // the axis the waterfall scrolls on.
        ("GET", ["api", "dataset", name, "nearest"]) => {
            let want_time = q_num::<f64>(q, "time");
            let (lat, lon) = match (q_num::<f64>(q, "lat"), q_num::<f64>(q, "lon")) {
                (Some(a), Some(b)) => (a, b),
                _ if want_time.is_some() => (f64::NAN, f64::NAN),
                _ => return Response::err(400, "lat and lon, or time, required"),
            };
            let Some(l) = state.loaded.read().unwrap().get(*name).cloned() else {
                return Response::err(404, "not loaded");
            };
            let sub: u8 = q_num(q, "subsystem")
                .unwrap_or_else(|| l.index.subsystems().first().copied().unwrap_or(20));
            let Some(pairs) = l.pairs.get(&sub) else {
                return Response::err(404, "no such subsystem");
            };

            // A time asks a different and much better question than a position.
            //
            // A contact is up to a swath off the track, and every row for tens
            // of metres either side is very nearly the same distance from it --
            // so nearest-by-position picks among them essentially at random, and
            // on a survey that crosses itself it can pick a row from an entirely
            // different pass. `Waterfall::world_to_pixel` says the same thing
            // from the other direction, which is why it asks which row has the
            // point *abeam* rather than which fish is nearest.
            //
            // Whatever was marked on a waterfall knows the instant it was seen,
            // and the two subsystems ping together, so a time picks the same
            // moment in either band. That is what makes a pair of band crops a
            // comparison rather than two pictures of roughly the same place.
            if let Some(t) = want_time {
                let Some((best, best_dt)) = waterfall::row_at_time(&l.index, pairs, t) else {
                    return Response::err(404, "no positioned pings");
                };
                let j = pairs[best].at;
                let f = l.nav.fix(&l.index.records[j as usize]);
                return Response::json(json!({
                    "dataset": name, "subsystem": sub,
                    "row": best, "pings": pairs.len(),
                    "dt_s": best_dt,
                    "time": f.time,
                    "fish": [f.lat, f.lon],
                }));
            }
            // Coarse sweep, then a fine one around the winner. A survey line
            // doubles back on itself, so the fine pass is deliberately narrow:
            // widening it would let the search jump to the neighbouring pass,
            // which is a different place on the image and the same place on the
            // seabed.
            let dist = |i: usize| -> f64 {
                let j = pairs[i].at;
                let f = l.nav.fix(&l.index.records[j as usize]);
                if !f.lat.is_finite() {
                    return f64::MAX;
                }
                geo::distance_m(f.lat, f.lon, lat, lon)
            };
            let coarse = (pairs.len() / 4000).max(1);
            let mut best = 0usize;
            let mut best_d = f64::MAX;
            let mut i = 0usize;
            while i < pairs.len() {
                let d = dist(i);
                if d < best_d {
                    best_d = d;
                    best = i;
                }
                i += coarse;
            }
            let lo = best.saturating_sub(coarse);
            let hi = (best + coarse).min(pairs.len().saturating_sub(1));
            for i in lo..=hi {
                let d = dist(i);
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            if best_d == f64::MAX {
                return Response::err(404, "no positioned pings");
            }
            let j = pairs[best].at;
            let f = l.nav.fix(&l.index.records[j as usize]);
            Response::json(json!({
                "dataset": name, "subsystem": sub,
                "row": best, "pings": pairs.len(),
                "distance_m": best_d,
                "time": f.time,
                "fish": [f.lat, f.lon],
            }))
        }
        ("GET", ["api", "dataset", name, "lines"]) => {
            let Some(l) = state.loaded.read().unwrap().get(*name).cloned() else {
                return Response::err(404, "not loaded");
            };
            let segs = nav::detect_lines(&l.nav.track, 0.9, 180.0, 20.0);
            let out: Vec<Value> = segs
                .iter()
                .map(|s| {
                    let (a_lat, a_lon) = l.nav.track.position(s.t0);
                    let (b_lat, b_lon) = l.nav.track.position(s.t1);
                    json!({
                        "kind": s.kind, "t0": s.t0, "t1": s.t1,
                        "course": s.course, "length_m": s.length_m,
                        "from": [a_lat, a_lon], "to": [b_lat, b_lon],
                    })
                })
                .collect();
            Response::json(json!({ "dataset": name, "segments": out }))
        }

        // ---- waterfall ----
        // Metadata now, image on its own URL.
        //
        // The image used to come back inline as a base64 data URI, which put a
        // megabyte and a half of text through JSON.parse on the main thread for
        // every screenful. Splitting them lets the browser fetch and decode the
        // picture the way it decodes any other image, and lets a block the
        // viewer already has be answered from its own HTTP cache.
        ("POST", ["api", "waterfall"]) => {
            let Ok(mut r) = serde_json::from_slice::<WaterfallReq>(body) else {
                return Response::err(400, "bad request");
            };
            let Some(l) = state.loaded.read().unwrap().get(&r.dataset).cloned() else {
                return Response::err(404, "not loaded");
            };
            // The recording owns the speed of sound, not the request. A client
            // that sent its own -- or an older one that sends none -- would
            // otherwise draw a waterfall on a different across-track scale from
            // the mosaic beside it, and a contact marked on one would land
            // somewhere else on the other. It is also in `block_key` below, so
            // changing it invalidates the cached blocks.
            r.req.sound_speed_m_s = l.mosaic_cfg.sound_speed_m_s;
            let Some(pairs) = l.pairs.get(&r.req.subsystem) else {
                return Response::err(404, "no such subsystem");
            };
            if r.req.start >= pairs.len() {
                return Response::err(400, "past the end of the recording");
            }
            let key = block_key(&r.dataset, &l.cfg, &r.req);
            let block = match state.waterfalls.write().unwrap().get(&key) {
                Some(b) => Some(b),
                None => None,
            };
            let block = match block {
                Some(b) => b,
                None => {
                    // The recording's own frame and gain, measured once.
                    // Without it every block normalises against itself and the
                    // seams between them are visible -- see `GainModel`.
                    let gk = gain_key(&r.dataset, &r.req);
                    let have = l.gains.read().unwrap().get(&gk).cloned();
                    let gains = match have {
                        Some(g) => g,
                        None => {
                            let _measuring = l.gain_gate.lock().unwrap();
                            let have = l.gains.read().unwrap().get(&gk).cloned();
                            match have {
                                Some(g) => g,
                                None => {
                                    let g = Arc::new(waterfall::build_gain_model(
                                        &l.index, pairs, &r.req,
                                    ));
                                    l.gains.write().unwrap().insert(gk, g.clone());
                                    g
                                }
                            }
                        }
                    };
                    let wf =
                        waterfall::render(&l.index, &l.nav, pairs, &r.req, Some(&gains));
                    let png = match crate::mosaic::encode_png_grey(&wf.pixels, wf.width, wf.height)
                    {
                        Ok(p) => p,
                        Err(e) => return Response::err(500, e.to_string()),
                    };
                    let b = Arc::new(Block { wf, png });
                    state.waterfalls.write().unwrap().insert(key.clone(), b.clone());
                    b
                }
            };
            Response::json(json!({
                "key": key,
                "dataset": r.dataset,
                "subsystem": r.req.subsystem,
                "start": r.req.start,
                "count": r.req.count,
                "stride": r.req.stride,
                "width": block.wf.width, "height": block.wf.height,
                "total_pings": pairs.len(),
                // The browser has to invert this image, and a column offset
                // means a different distance in each axis.
                "axis": block.wf.axis,
                "metres_per_px": block.wf.metres_per_px,
                "rows": block.wf.rows,
                "png": format!("/api/waterfall/{key}.png"),
            }))
        }
        ("GET", ["api", "waterfall", key]) => {
            let key = key.trim_end_matches(".png");
            let Some(b) = state.waterfalls.write().unwrap().get(key) else {
                return Response::err(404, "block has been evicted; ask again");
            };
            // The colour scheme rides in the query rather than in the block, so
            // one rendered block serves every colour it might be drawn in --
            // two channels of the same recording shown side by side in
            // different ramps are still one render each, not one per ramp.
            let style = mosaic_style_from_query(q);
            if style.is_grey() {
                // The key is a digest of everything that went into the render,
                // so this URL's content can never change. Say so, and the
                // browser will not ask twice.
                return Response::png(b.png.clone(), 86_400);
            }
            let rgba = style.colour_grey(&b.wf.pixels);
            match crate::mosaic::encode_png_rgba(&rgba, b.wf.width, b.wf.height) {
                Ok(p) => Response::png(p, 86_400),
                Err(e) => Response::err(500, e.to_string()),
            }
        }
        ("POST", ["api", "waterfall", "pick"]) => {
            let Ok(r) = serde_json::from_slice::<PickReq>(body) else {
                return Response::err(400, "bad request");
            };
            let Some(block) = state.waterfalls.write().unwrap().get(&r.key) else {
                return Response::err(404, "no such waterfall");
            };
            let wf = &block.wf;
            let Some((lat, lon)) = wf.pixel_to_world(r.x, r.y) else {
                return Response::err(400, "outside image");
            };
            let row = wf.rows.get((r.y.max(0.0)) as usize).copied();
            // Ground across-track, whichever axis the image was drawn on --
            // the same arithmetic `pixel_to_world` just used, not a second copy
            // of it that can be forgotten when the transform changes.
            let across = row.map(|rw| {
                let half = wf.width as f64 / 2.0;
                wf.axis.to_ground((r.x - half) / half * rw.half_width_m, rw.altitude)
            });
            Response::json(json!({
                "lat": lat, "lon": lon,
                "across_m": across,
                "row": row,
            }))
        }

        // ---- tiles ----
        ("GET", ["api", "mosaic", name, sub, z, x, y]) => {
            let (Ok(sub), Ok(z), Ok(x), Ok(y)) = (
                sub.parse::<u8>(),
                z.parse::<u32>(),
                x.parse::<i64>(),
                y.trim_end_matches(".png").parse::<i64>(),
            ) else {
                return Response::err(400, "bad tile");
            };
            let Some(l) = state.loaded.read().unwrap().get(*name).cloned() else {
                return Response::err(404, "not loaded");
            };
            let m = match state.mosaic(&l, sub) {
                Ok(m) => m,
                Err(e) => return Response::err(500, e.to_string()),
            };
            // The colour scheme rides in the query, like an imported layer's,
            // so the URL still names exactly one picture and can be cached for
            // as long as the settings digest in it holds.
            let style = mosaic_style_from_query(q);
            match m.tile_styled(z, x, y, &style) {
                Some(rgba) => match crate::mosaic::encode_png_rgba(&rgba, 256, 256) {
                    Ok(p) => Response::png(p, 86_400),
                    Err(e) => Response::err(500, e.to_string()),
                },
                None => Response::png(BLANK_PNG.to_vec(), 86_400),
            }
        }
        ("POST", ["api", "mosaic", "build"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            let name = v.get("dataset").and_then(|d| d.as_str()).unwrap_or_default();
            let sub = v.get("subsystem").and_then(|s| s.as_u64()).unwrap_or(20) as u8;
            let Some(l) = state.loaded.read().unwrap().get(name).cloned() else {
                return Response::err(404, "not loaded");
            };
            let force = v.get("force").and_then(|f| f.as_bool()).unwrap_or(false);
            if force {
                l.mosaics.write().unwrap().remove(&sub);
                let _ = std::fs::remove_file(state.ws.mosaic_path(name, sub, &l.mosaic_key(sub)));
            }
            match state.mosaic(&l, sub) {
                Ok(m) => Response::json(json!({
                    "dataset": name, "subsystem": sub,
                    "rev": l.mosaic_key(sub),
                    "bounds": m.header.bounds,
                    "base_zoom": m.header.base_zoom,
                    "size": [m.header.width, m.header.height],
                    "pings": m.header.pings,
                    "config": m.header.config,
                })),
                Err(e) => Response::err(500, e.to_string()),
            }
        }

        // ---- imported layers ----

        // Browse the filesystem so a file can be chosen.
        //
        // The webview cannot hand back a path from a file input, and the shell
        // has no native dialog wired in, so the server lists directories and
        // the viewer draws the picker. It also means the headless server has a
        // file picker, which the desktop-only route would not have given.
        ("GET", ["api", "browse"]) => {
            let want = q.get("path").cloned().unwrap_or_default();
            let dir = if want.is_empty() {
                state.ws.root.clone()
            } else {
                PathBuf::from(&want)
            };
            let dir = match std::fs::canonicalize(&dir) {
                Ok(d) => d,
                Err(e) => return Response::err(404, format!("{}: {e}", dir.display())),
            };
            let mut dirs: Vec<Value> = Vec::new();
            let mut files: Vec<Value> = Vec::new();
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => return Response::err(403, format!("{}: {e}", dir.display())),
            };
            // Two things are being picked in this one browser: a file to
            // import as a layer, and a folder of recordings to bring into the
            // project. The second needs to know which folders hold sonar, and
            // finding that out is a read of every folder listed -- so it is
            // only done when that is what is being looked for.
            let recordings = q.get("want").map(|w| w == "recordings").unwrap_or(false);
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                if p.is_dir() {
                    let mut row = json!({ "name": name, "path": p.display().to_string() });
                    if recordings {
                        let files = state.ws.sonar_files(&p);
                        let bytes: u64 =
                            files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
                        row["sonar"] = json!(files.len());
                        row["bytes"] = json!(bytes);
                        row["link"] = json!(std::fs::symlink_metadata(&p)
                            .map(|m| m.is_symlink())
                            .unwrap_or(false));
                    }
                    dirs.push(row);
                } else if recordings {
                    continue;
                } else if let Some(kind) = importable(&p) {
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    files.push(json!({
                        "name": name, "path": p.display().to_string(),
                        "kind": kind, "size": size,
                    }));
                }
            }
            dirs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            // The folder being looked at can itself be the recording: an
            // operator navigates into it to see the files before choosing it.
            let (sonar, bytes) = if recordings {
                let f = state.ws.sonar_files(&dir);
                let b: u64 = f.iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
                (f.len(), b)
            } else {
                (0, 0)
            };
            Response::json(json!({
                "path": dir.display().to_string(),
                "parent": dir.parent().map(|p| p.display().to_string()),
                "dirs": dirs, "files": files,
                "sonar": sonar, "bytes": bytes,
            }))
        }

        // Import a file as a layer. The heavy resampling happens here, once,
        // so the first tile request is not the thing that takes ten seconds.
        ("POST", ["api", "layers", "import"]) => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return Response::err(400, "bad json");
            };
            let Some(path) = v.get("path").and_then(|p| p.as_str()) else {
                return Response::err(400, "path required");
            };
            let src = PathBuf::from(path);
            if !src.is_file() {
                return Response::err(404, format!("{path} is not a file"));
            }
            let Some(kind) = importable(&src) else {
                return Response::err(
                    415,
                    "only GeoTIFF (.tif/.tiff) and GPX (.gpx) can be imported",
                );
            };
            let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let label = v
                .get("label")
                .and_then(|l| l.as_str())
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .unwrap_or(stem);
            let id = format!("{kind}:{}", crate::fingerprint(&path)[..10].to_string());

            let mut layer = Layer {
                id: id.clone(),
                label,
                kind: kind.to_string(),
                parent: String::new(),
                visible: true,
                opacity: 1.0,
                file: src.display().to_string(),
                dataset: String::new(),
                subsystem: None,
                bounds: None,
                crs: String::new(),
                style: LayerStyle::default(),
                colour: String::new(),
                show_boat: false,
                show_fish: true,
                info: String::new(),
                added: crate::time::iso8601(crate::project::now_unix()),
            };

            if kind == KIND_RASTER {
                let key = match State::layer_key(&src) {
                    Ok(k) => k,
                    Err(e) => return Response::err(500, e.to_string()),
                };
                let cache = state.ws.layer_cache_path(&key);
                let header = if cache.exists() {
                    match LayerRaster::load(&cache) {
                        Ok(r) => r.header.clone(),
                        Err(_) => {
                            let _ = std::fs::remove_file(&cache);
                            match crate::layer::import_tiff(&src, &cache, None, None) {
                                Ok(h) => h,
                                Err(e) => return Response::err(422, e.to_string()),
                            }
                        }
                    }
                } else {
                    match crate::layer::import_tiff(&src, &cache, None, None) {
                        Ok(h) => h,
                        Err(e) => return Response::err(422, e.to_string()),
                    }
                };
                layer.bounds = Some(header.bounds);
                layer.crs = header
                    .source_epsg
                    .map(|e| format!("EPSG:{e}"))
                    .unwrap_or_else(|| "unknown".into());
                layer.info = format!(
                    "{}x{} source · {:.0}% covered · {:.2} to {:.2}",
                    header.source_size[0],
                    header.source_size[1],
                    header.filled as f64 / (header.width * header.height) as f64 * 100.0,
                    header.vmin,
                    header.vmax
                );
                if header.kind == crate::layer::PlaneKind::Rgba {
                    layer.info.push_str(" · colour");
                }
                state.rasters.write().unwrap().remove(&id);
            } else {
                let g = match crate::gpx::read(&src) {
                    Ok(g) => g,
                    Err(e) => return Response::err(422, e.to_string()),
                };
                if g.points() == 0 {
                    return Response::err(422, "the file has no track, route or waypoint in it");
                }
                layer.bounds = Some(g.bounds);
                layer.crs = "EPSG:4326".into();
                layer.info = g.describe();
                layer.colour = "#f0b429".into();
                state.vectors.write().unwrap().insert(id.clone(), Arc::new(g));
            }

            // Put it on top, and persist, so a reopened project has it.
            {
                let mut guard = state.project.write().unwrap();
                let Some(p) = guard.as_mut() else {
                    return Response::err(400, "open a project first");
                };
                p.layers.retain(|l| l.id != id);
                p.layers.insert(0, layer.clone());
                let path = state.ws.project_path(&p.name);
                if let Err(e) = p.save(path) {
                    return Response::err(500, e.to_string());
                }
            }
            Response::json(json!({ "layer": layer }))
        }

        ("GET", ["api", "layers", id, "features"]) => match state.vector(id) {
            Ok(g) => Response::json(g.to_geojson()),
            Err(e) => Response::err(404, e.to_string()),
        },

        // A tile of an imported raster.
        //
        // The style is in the query rather than read from the project, which
        // makes the request self-contained: the URL names exactly one picture,
        // so it can be cached hard and a change of ramp is a different URL
        // rather than a stale one.
        ("GET", ["api", "layer", id, z, x, y]) => {
            let (Ok(z), Ok(tx), Ok(ty)) = (
                z.parse::<u32>(),
                x.parse::<i64>(),
                y.trim_end_matches(".png").parse::<i64>(),
            ) else {
                return Response::err(400, "bad tile");
            };
            let base = state.layer(id).map(|l| l.style).unwrap_or_default();
            let style = style_from_query(q, base);
            let r = match state.raster(id) {
                Ok(r) => r,
                Err(e) => return Response::err(404, e.to_string()),
            };
            match r.tile(z, tx, ty, &style) {
                Some(rgba) => match crate::mosaic::encode_png_rgba(&rgba, 256, 256) {
                    Ok(p) => Response::png(p, 86_400),
                    Err(e) => Response::err(500, e.to_string()),
                },
                None => Response::png(BLANK_PNG.to_vec(), 86_400),
            }
        }

        // What a raster holds under a position, for the readout.
        ("GET", ["api", "layers", id, "sample"]) => {
            let (Some(lat), Some(lon)) = (q_num::<f64>(q, "lat"), q_num::<f64>(q, "lon")) else {
                return Response::err(400, "lat and lon required");
            };
            match state.raster(id) {
                Ok(r) => Response::json(json!({
                    "value": r.sample(lat, lon),
                    "units": r.header.units,
                })),
                Err(e) => Response::err(404, e.to_string()),
            }
        }

        // The ramps the viewer can offer, with swatches drawn from the same
        // code that colours the tiles.
        ("GET", ["api", "ramps"]) => {
            let out: Vec<Value> = Ramp::ALL
                .iter()
                .map(|r| {
                    let stops: Vec<String> = (0..9)
                        .map(|i| {
                            let c = r.at(i as f32 / 8.0);
                            format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
                        })
                        .collect();
                    json!({ "name": r.name(), "stops": stops })
                })
                .collect();
            Response::json(json!({ "ramps": out }))
        }

        // ---- the search plan ----
        //
        // Only 0 to 90 degrees is ever generated, and that is the whole answer
        // rather than a shortcut: a box run at 100 degrees is the same set of
        // lines as one run at 10, turned round.
        ("GET", ["api", "plan"]) => {
            let (ps, targets) = match plan_inputs(state) {
                Ok(v) => v,
                Err(e) => return Response::err(400, e),
            };
            Response::json(json!({
                "spec": ps.spec,
                // Null until the planner has been opened: the viewer needs to
                // tell "never chosen" from "chosen nothing" to tick the boxes.
                "target_ids": ps.targets,
                "targets": targets,
                "quadrant": plan::quadrant(&ps.spec, &targets),
            }))
        }
        ("POST", ["api", "plan"]) => {
            let Ok(next) = serde_json::from_slice::<PlanState>(body) else {
                return Response::err(400, "bad plan");
            };
            let mut guard = state.project.write().unwrap();
            let Some(p) = guard.as_mut() else {
                return Response::err(400, "no project open");
            };
            p.plan = next;
            let path = state.ws.project_path(&p.name);
            if let Err(e) = p.save(&path) {
                return Response::err(500, e.to_string());
            }
            let spec = p.plan.spec;
            let ids = p.plan.targets.clone();
            drop(guard);
            let targets = PlanState { spec, targets: ids.clone() }
                .targets_from(&state.contacts.read().unwrap().items);
            Response::json(json!({
                "spec": spec,
                "target_ids": ids,
                "targets": targets,
                "quadrant": plan::quadrant(&spec, &targets),
            }))
        }
        // One azimuth, solved: the numbers for the report and the geometry for
        // the chart, which is GeoJSON so the viewer draws it with the code that
        // already draws every other vector layer.
        ("GET", ["api", "plan", "solve"]) => {
            let (ps, targets) = match plan_inputs(state) {
                Ok(v) => v,
                Err(e) => return Response::err(400, e),
            };
            let az: f64 = q_num(q, "az").unwrap_or(0.0);
            let Some(p) = plan::solve(&ps.spec, &targets, az) else {
                return Response::err(400, "nothing to search for");
            };
            let mut out = plan_json(&p, &ps.spec);
            if let Some(o) = out.as_object_mut() {
                o.insert("geojson".into(), p.to_geojson());
            }
            Response::json(out)
        }
        // Write the chosen azimuths into the project's `exports/` directory,
        // one GPX each. The files are the deliverable; importing them back as
        // layers is a separate step the viewer takes, so a plan on the chart is
        // the same kind of object as any other imported track.
        ("POST", ["api", "plan", "export"]) => {
            let Some(project) = state.project_name() else {
                return Response::err(400, "no project open");
            };
            let (ps, targets) = match plan_inputs(state) {
                Ok(v) => v,
                Err(e) => return Response::err(400, e),
            };
            if targets.is_empty() {
                return Response::err(400, "nothing to search for");
            }
            let v: Value = serde_json::from_slice(body).unwrap_or(json!({}));
            let azimuths: Vec<f64> = v
                .get("azimuths")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
                .filter(|a: &Vec<f64>| !a.is_empty())
                .unwrap_or_else(|| plan::quadrant(&ps.spec, &targets).iter().map(|r| r.azimuth_deg).collect());
            let flag = |k: &str, d: bool| v.get(k).and_then(|b| b.as_bool()).unwrap_or(d);
            let opts = GpxOptions {
                // The trace is the default deliverable: a plotter that takes
                // only one of the two is better served by the shape to follow
                // than by legs it will not be steered along.
                track: flag("track", true),
                routes: flag("routes", false),
                route_per_line: flag("route_per_line", false),
                turn_points: flag("turn_points", false),
                targets: flag("targets", true),
                line_waypoints: flag("line_waypoints", false),
                name: String::new(),
            };

            let dir = state.ws.exports_dir(&project);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return Response::err(500, e.to_string());
            }
            let mut written = Vec::new();
            let mut index = String::from("SEARCH PLAN\n===========\n\n");
            index.push_str(&format!("Targets ({}):\n", targets.len()));
            for t in &targets {
                index.push_str(&format!(
                    "  {:<12} {:.6}, {:.6}  +/- {:.0} m\n",
                    t.name, t.lat, t.lon, t.radius_m
                ));
            }
            index.push('\n');
            for az in azimuths {
                let Some(p) = plan::solve(&ps.spec, &targets, az) else { continue };
                let g = p.to_gpx(&ps.spec, &targets, &opts);
                let file = format!("{}.gpx", safe_name(&g.name));
                let path = dir.join(&file);
                if let Err(e) = crate::gpx::save(&path, &g) {
                    return Response::err(500, format!("{file}: {e}"));
                }
                index.push_str(&format!("{file}\n  {}\n", p.digest(&ps.spec)));
                index.push_str(&format!(
                    "  coverage: {:.1}% unseen, {:.0}% seen twice or more\n\n",
                    p.coverage.none * 100.0,
                    p.coverage.twice * 100.0
                ));
                written.push(json!({
                    "file": file,
                    "path": path.display().to_string(),
                    "azimuth": p.azimuth_deg,
                    "name": g.name,
                }));
            }
            index.push_str(
                "Waypoints are GPS ANTENNA positions; the fish is astern by the layback above.\n\
                 Recording starts at the A point of each line, not the S.\n",
            );
            let _ = std::fs::write(dir.join("search-plan.txt"), index);
            Response::json(json!({
                "dir": dir.display().to_string(),
                "files": written,
            }))
        }

        // ---- contacts ----
        ("GET", ["api", "contacts"]) => {
            Response::json(json!({ "contacts": state.contacts.read().unwrap().items }))
        }
        ("POST", ["api", "contacts"]) => {
            let Ok(c) = serde_json::from_slice::<Contact>(body) else {
                return Response::err(400, "bad contact");
            };
            let id = state.contacts.write().unwrap().upsert(c);
            if let Err(e) = state.save_contacts() {
                return Response::err(500, e.to_string());
            }
            let out = state.contacts.read().unwrap().get(&id).cloned();
            Response::json(json!({ "contact": out }))
        }
        ("DELETE", ["api", "contacts", id]) => {
            let ok = state.contacts.write().unwrap().remove(id);
            let _ = state.save_contacts();
            Response::json(json!({ "removed": ok }))
        }
        ("POST", ["api", "contacts", id, "snapshot"]) => {
            let Some(project) = state.project_name() else {
                return Response::err(400, "no project open");
            };
            let dir = state.ws.snaps_dir(&project);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return Response::err(500, e.to_string());
            }
            let Some(kind) = SnapKind::from_query(q) else {
                return Response::err(400, "unknown snapshot kind");
            };
            let file = format!("{id}{}.png", kind.suffix());
            if let Err(e) = std::fs::write(dir.join(&file), body) {
                return Response::err(500, e.to_string());
            }
            {
                let mut c = state.contacts.write().unwrap();
                if let Some(x) = c.items.iter_mut().find(|x| x.id == *id) {
                    *kind.slot(x) = Some(file.clone());
                }
            }
            let _ = state.save_contacts();
            Response::json(json!({ "snapshot": file }))
        }
        ("GET", ["api", "snap", id]) => {
            let Some(project) = state.project_name() else {
                return Response::not_found();
            };
            let Some(kind) = SnapKind::from_query(q) else {
                return Response::not_found();
            };
            let stem = format!("{}{}", id.trim_end_matches(".png"), kind.suffix());
            let p = state.ws.snaps_dir(&project).join(format!("{stem}.png"));
            match std::fs::read(p) {
                Ok(b) => Response::png(b, 60),
                Err(_) => Response::not_found(),
            }
        }

        // ---- coordinates ----
        ("GET", ["api", "crs"]) => {
            let lat: f64 = q_num(q, "lat").unwrap_or(52.0);
            let lon: f64 = q_num(q, "lon").unwrap_or(4.0);
            let v: Vec<crs::CrsInfo> =
                crs::suggestions(lat, lon).into_iter().map(Into::into).collect();
            Response::json(json!({ "systems": v }))
        }
        ("GET", ["api", "convert"]) => {
            let (Some(lat), Some(lon)) = (q_num::<f64>(q, "lat"), q_num::<f64>(q, "lon")) else {
                return Response::err(400, "lat and lon required");
            };
            let codes: Vec<u32> = q
                .get("epsg")
                .map(|s| s.split(',').filter_map(|c| c.trim().parse().ok()).collect())
                .unwrap_or_else(|| crs::suggestions(lat, lon).iter().map(|c| c.epsg).collect());
            let out: Vec<Value> = codes
                .iter()
                .filter_map(|&c| crs::get(c))
                .map(|c| {
                    let (x, y) = c.format(lat, lon);
                    json!({
                        "epsg": c.epsg, "name": c.name, "unit": c.unit,
                        "axes": [c.axes.0, c.axes.1], "x": x, "y": y,
                    })
                })
                .collect();
            Response::json(json!({
                "lat": lat, "lon": lon,
                "dms": [crs::to_dms(lat, true), crs::to_dms(lon, false)],
                "dm": [crs::to_dm(lat, true), crs::to_dm(lon, false)],
                "systems": out,
            }))
        }

        // ---- report ----

        // The viewer renders the map views and posts them here.
        ("POST", ["api", "report", "image", name]) => {
            let Some(project) = state.project_name() else {
                return Response::err(400, "no project open");
            };
            // The name becomes a file name, so it may not wander out of the
            // directory it is meant for.
            if name.is_empty()
                || name.len() > 120
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                || name.contains("..")
            {
                return Response::err(400, "bad image name");
            }
            let dir = state.ws.report_dir(&project);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return Response::err(500, e.to_string());
            }
            if body.is_empty() {
                let _ = std::fs::remove_file(dir.join(format!("{name}.png")));
                return Response::json(json!({ "removed": true }));
            }
            match std::fs::write(dir.join(format!("{name}.png")), body) {
                Ok(()) => Response::json(json!({ "saved": name, "bytes": body.len() })),
                Err(e) => Response::err(500, e.to_string()),
            }
        }

        ("GET", ["api", "report"]) => match report_html(state) {
            Ok(html) => Response::html(html),
            Err(e) => Response::err(400, e),
        },
        // Writes the page and says where it went. The shell turns this into an
        // open; a browser can just follow the path.
        ("POST", ["api", "report", "save"]) => match save_report(state) {
            Ok(p) => Response::json(json!({ "path": p.to_string_lossy() })),
            Err(e) => Response::err(400, e),
        },

        // ---- measurement helper ----
        ("GET", ["api", "measure"]) => {
            let (Some(a), Some(b), Some(c), Some(d)) = (
                q_num::<f64>(q, "lat1"),
                q_num::<f64>(q, "lon1"),
                q_num::<f64>(q, "lat2"),
                q_num::<f64>(q, "lon2"),
            ) else {
                return Response::err(400, "need lat1,lon1,lat2,lon2");
            };
            Response::json(json!({
                "distance_m": geo::distance_m(a, b, c, d),
                "bearing_deg": geo::initial_bearing(a, b, c, d),
            }))
        }

        _ => Response::not_found(),
    }
}

/// Base64, standard alphabet with padding.
pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}
