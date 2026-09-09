//! The project: which datasets are loaded, what was marked, and how a report
//! should describe it.
//!
//! Contacts round-trip as GeoJSON so the existing `marks.geojson` files keep
//! working and anything downstream can read them without a converter.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::index::Bounds;
use crate::mosaic::MosaicConfig;
use crate::nav::NavConfig;
use crate::time::iso8601;

pub const SCHEMA: &str = "swath-project/1";

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ProjectMeta {
    #[serde(default)]
    pub client: String,
    #[serde(default)]
    pub vessel: String,
    #[serde(default)]
    pub operator: String,
    #[serde(default)]
    pub job_number: String,
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub notes: String,
    /// EPSG codes the report should tabulate positions in, in order. WGS84 is
    /// always shown; these are the extra columns.
    #[serde(default)]
    pub report_crs: Vec<u32>,
}

/// One recording loaded into the project.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatasetRef {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Track colour in the viewer, `#rrggbb`.
    #[serde(default)]
    pub colour: String,
    #[serde(default)]
    pub added: String,
    /// Per-dataset navigation, because the layback is a property of how that
    /// day was rigged, not of the project.
    #[serde(default)]
    pub nav: NavConfig,
    /// Speed of sound in the water, m/s -- measured if you have it.
    ///
    /// Per recording for the same reason the layback is: it is a property of
    /// the water on the day. The default is the speed the topside was
    /// configured with, so a project that says nothing keeps the geometry it
    /// has always had rather than silently moving.
    #[serde(default = "recorded_speed")]
    pub sound_speed_m_s: f64,
    /// How this recording's imagery is painted -- everything under "Mosaic" in
    /// the settings panel.
    ///
    /// Stored as the whole `MosaicConfig` rather than as a hand-listed set of
    /// fields, because the hand-listed version is what was here before and it
    /// silently dropped every setting the panel wrote: the viewer sent `tvg`,
    /// `agc`, `nadir_blank_m` and the rest, this struct had nowhere to put
    /// them, serde discarded them, and the panel read defaults back and looked
    /// like it had reverted. Adding a painter setting must not require
    /// remembering to add it here too.
    ///
    /// Three of its fields are never read from here, because `Loaded::mosaic_cfg`
    /// fills them in per channel: `subsystem`, `nav` -- which lives above, per
    /// recording -- and `centre_freq_hz`, which is recovered from the files.
    /// `sound_speed_m_s` also lives above; the viewer sends the outer one.
    #[serde(default)]
    pub mosaic: MosaicConfig,
}

fn recorded_speed() -> f64 {
    crate::C_RECORDED
}

fn yes() -> bool {
    true
}

/// One entry in the project tree.
///
/// Recordings, their mosaics and tracks, and imported files are all in this one
/// list, and the list *is* the draw order -- first is drawn last, so first is
/// on top. It is stored flat as a pre-order flattening: a node's children
/// follow it contiguously, and `parent` says whose they are. Flat because that
/// round-trips through the project file without needing a schema for trees, and
/// because the order is then the draw order with no traversal.
///
/// A `recording` node is a container: it is not drawn itself, and the
/// navigation that places its imagery lives on the `DatasetRef` it names,
/// because the layback is a property of how that day was rigged.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layer {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// `recording`, `mosaic`, `track`, `raster` or `vector`.
    pub kind: String,
    /// Id of the containing node, empty for a top-level one.
    #[serde(default)]
    pub parent: String,
    #[serde(default = "yes")]
    pub visible: bool,
    #[serde(default = "one")]
    pub opacity: f64,
    /// Source file, for an imported layer. Absolute: an imported grid is
    /// referenced where it lives rather than copied into the project, because
    /// these files run to hundreds of megabytes and are usually shared.
    #[serde(default)]
    pub file: String,
    /// Recording and channel, for a mosaic layer.
    #[serde(default)]
    pub dataset: String,
    #[serde(default)]
    pub subsystem: Option<u8>,
    #[serde(default, deserialize_with = "bounds_compat")]
    pub bounds: Option<Bounds>,
    #[serde(default)]
    pub crs: String,
    /// Ramp, stretch and shading for a single-band raster.
    #[serde(default)]
    pub style: crate::layer::LayerStyle,
    /// Line and marker colour for a vector layer.
    #[serde(default)]
    pub colour: String,
    /// Which of the two tracks a `track` layer draws. The fish is where the
    /// imagery came from and the boat is where the GPS was, and an operator
    /// wants to see either or both -- so these are settings, and they have to
    /// survive a save. They did not, before: the viewer had them and the
    /// project file had nowhere to put them.
    #[serde(default)]
    pub show_boat: bool,
    #[serde(default = "yes")]
    pub show_fish: bool,
    /// What the reader made of the file, for the panel to show.
    #[serde(default)]
    pub info: String,
    #[serde(default)]
    pub added: String,
}

/// Read a layer's bounds in either the current form or the first schema's.
///
/// Version 1 wrote two corners as `[[lat, lon], [lat, lon]]`. One layer in one
/// old project was enough to make `Project::load` fail, which made the whole
/// project unopenable -- a nested array where an object was expected, reported
/// as a line number in a file the operator never wrote by hand.
fn bounds_compat<'de, D>(d: D) -> Result<Option<Bounds>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    if v.is_null() {
        return Ok(None);
    }
    if let Some(a) = v.as_array() {
        let pt = |i: usize| -> Option<(f64, f64)> {
            let p = a.get(i)?.as_array()?;
            Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
        };
        // Corners in either order, so take the extremes rather than trusting
        // which one was written first.
        return Ok(match (pt(0), pt(1)) {
            (Some((a_lat, a_lon)), Some((b_lat, b_lon))) => Some(Bounds {
                min_lat: a_lat.min(b_lat),
                min_lon: a_lon.min(b_lon),
                max_lat: a_lat.max(b_lat),
                max_lon: a_lon.max(b_lon),
            }),
            _ => None,
        });
    }
    serde_json::from_value(v).map(Some).map_err(serde::de::Error::custom)
}

pub const KIND_RECORDING: &str = "recording";
pub const KIND_MOSAIC: &str = "mosaic";
pub const KIND_TRACK: &str = "track";
pub const KIND_RASTER: &str = "raster";
pub const KIND_VECTOR: &str = "vector";

impl Layer {
    pub fn recording_id(dataset: &str) -> String {
        format!("rec:{dataset}")
    }
    pub fn mosaic_id(dataset: &str, subsystem: u8) -> String {
        format!("mosaic:{dataset}:{subsystem}")
    }
    pub fn track_id(dataset: &str, subsystem: u8) -> String {
        format!("track:{dataset}:{subsystem}")
    }

    pub fn is_mosaic(&self) -> bool {
        self.kind == KIND_MOSAIC
    }
}

fn one() -> f64 {
    1.0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    #[serde(default = "schema")]
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub modified: String,
    #[serde(default)]
    pub meta: ProjectMeta,
    #[serde(default)]
    pub datasets: Vec<DatasetRef>,
    #[serde(default)]
    pub layers: Vec<Layer>,
    /// What the report is made of. Separate from the layer tree on purpose: a
    /// chart in a deliverable is not "whatever happened to be ticked on screen
    /// when someone pressed the button".
    #[serde(default)]
    pub report: ReportSpec,
    /// Free-form per-dataset UI state the viewer wants to remember.
    #[serde(default)]
    pub view: BTreeMap<String, serde_json::Value>,
}

/// One layer on one chart.
///
/// `boat` and `fish` only mean anything on a `track`, where they choose which
/// of the two lines is drawn -- the overview wants the boat's line and a
/// recording's own section wants both, so the choice is per chart rather than
/// per layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChartLayer {
    pub id: String,
    /// Drawn on this chart. Layers the chart *could* show are all listed, so
    /// the dialog has something to offer and an unticked layer is remembered
    /// as unticked rather than as one nobody has heard of yet.
    #[serde(default = "yes")]
    pub on: bool,
    #[serde(default)]
    pub boat: bool,
    #[serde(default = "yes")]
    pub fish: bool,
}

/// One picture in the report, and everything drawn on it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChartSpec {
    /// Also the name of the PNG in the project's `report/` directory.
    pub id: String,
    /// `overview` (every recording, one band) or `dataset` (one of each).
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    /// Empty on an overview.
    #[serde(default)]
    pub dataset: String,
    #[serde(default)]
    pub subsystem: Option<u8>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Top of the stack first, the same order the tree shows.
    #[serde(default)]
    pub layers: Vec<ChartLayer>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportSpec {
    #[serde(default = "osm")]
    pub basemap: String,
    #[serde(default = "yes")]
    pub seamark: bool,
    #[serde(default = "yes")]
    pub contacts: bool,
    #[serde(default)]
    pub charts: Vec<ChartSpec>,
}

impl Default for ReportSpec {
    fn default() -> ReportSpec {
        ReportSpec {
            basemap: osm(),
            seamark: true,
            contacts: true,
            charts: Vec::new(),
        }
    }
}

fn osm() -> String {
    "osm".to_string()
}

fn schema() -> String {
    SCHEMA.to_string()
}

impl Project {
    pub fn new(name: &str) -> Project {
        let now = iso8601(now_unix());
        Project {
            schema: SCHEMA.to_string(),
            name: name.to_string(),
            title: name.to_string(),
            created: now.clone(),
            modified: now,
            meta: ProjectMeta::default(),
            datasets: Vec::new(),
            layers: Vec::new(),
            report: ReportSpec::default(),
            view: BTreeMap::new(),
        }
    }

    pub fn load(path: impl AsRef<Path>) -> io::Result<Project> {
        let s = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&s)?)
    }

    pub fn save(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        self.modified = iso8601(now_unix());
        let path = path.as_ref();
        if let Some(d) = path.parent() {
            fs::create_dir_all(d)?;
        }
        // Write beside the target and rename, so an interrupted save cannot
        // leave a half-written project behind.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, path)
    }

    pub fn dataset(&self, name: &str) -> Option<&DatasetRef> {
        self.datasets.iter().find(|d| d.name == name)
    }
    pub fn dataset_mut(&mut self, name: &str) -> Option<&mut DatasetRef> {
        self.datasets.iter_mut().find(|d| d.name == name)
    }
    pub fn enabled(&self) -> impl Iterator<Item = &DatasetRef> {
        self.datasets.iter().filter(|d| d.enabled)
    }
}

pub fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---- contacts --------------------------------------------------------------

/// How a contact was drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    #[default]
    Point,
    Circle,
    Box,
    Line,
}

/// Which view the contact was marked in. Kept because it changes what the
/// position means: a waterfall mark is a ping and a sample, resolved through
/// the navigation, and it moves if the navigation is re-solved. A map mark is
/// a position, and does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Map,
    Waterfall,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Contact {
    // Defaulted so that a feature which carries its identity or its position
    // only in the GeoJSON proper -- `contact_id`, or the geometry -- still
    // parses, and `from_feature` can fill them in from there. Without this the
    // strictness below would refuse whole projects over files that are
    // perfectly readable.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub confidence: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub colour: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub shape: Shape,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub lat: f64,
    #[serde(default)]
    pub lon: f64,
    /// Circle radius or box half-diagonal, metres.
    #[serde(default)]
    pub radius_m: f64,
    /// Measured extent, when the operator drew one.
    #[serde(default)]
    pub length_m: Option<f64>,
    #[serde(default)]
    pub width_m: Option<f64>,
    #[serde(default)]
    pub height_m: Option<f64>,
    /// Which recording it was seen in.
    #[serde(default)]
    pub dataset: String,
    #[serde(default)]
    pub subsystem: Option<u8>,
    /// Ping time, when marked from a waterfall.
    #[serde(default)]
    pub time: Option<f64>,
    /// Water depth and fish altitude at the mark.
    #[serde(default)]
    pub depth_m: Option<f64>,
    #[serde(default)]
    pub altitude_m: Option<f64>,
    /// Across-track distance from nadir, signed, when marked from a waterfall.
    #[serde(default)]
    pub across_m: Option<f64>,
    /// Snapshot file name inside the project's `snaps/` directory: the chart,
    /// with the imagery placed on the seabed.
    #[serde(default)]
    pub snapshot: Option<String>,
    /// The same ground, drawn from one band alone.
    ///
    /// A chart crop shows whichever channels happened to be switched on, blended
    /// over each other, which is the one thing a reader cannot argue from: the
    /// two bands see a target differently, and the difference is the evidence.
    /// These are what the report sheet shows; `snapshot` stays for the chart
    /// crop the viewer draws beside them, which answers where rather than what.
    #[serde(default)]
    pub snapshot_lf: Option<String>,
    #[serde(default)]
    pub snapshot_hf: Option<String>,
    /// The same contact as the sonar saw it, cropped from the waterfall.
    ///
    /// Both, because they answer different questions. The chart says where the
    /// thing is and what is around it; the waterfall says what the return and
    /// its shadow actually look like, which is what a classification is argued
    /// from. A sheet with only one of them is half a sheet.
    ///
    /// This one is whichever channel the viewer happened to be showing, which
    /// is the same objection as the chart crop's: it is not a comparison, and
    /// which band it came from is not recorded anywhere in it. The two below
    /// replace it. Kept so that a project written before them still opens with
    /// its pictures.
    #[serde(default)]
    pub snapshot_wf: Option<String>,
    /// The waterfall crop from each band, named rather than incidental.
    ///
    /// Four pictures make a sheet: the seabed at two frequencies and the return
    /// at two frequencies, the same target in each. A target that is bright at
    /// one frequency and absent at the other is telling you what it is made of,
    /// and that argument needs the pair on both views -- the chart pair alone
    /// shows the ground without the shadow, and one sonar crop from whichever
    /// channel was on screen shows the shadow without the comparison.
    #[serde(default)]
    pub snapshot_wf_lf: Option<String>,
    #[serde(default)]
    pub snapshot_wf_hf: Option<String>,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub modified: String,
}

impl Contact {
    /// GeoJSON feature, with every scalar mirrored into `properties` so the
    /// file is useful to anything that reads GeoJSON.
    pub fn to_feature(&self) -> serde_json::Value {
        let mut props = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(o) = props.as_object_mut() {
            o.insert("kind".into(), serde_json::json!(self.shape));
        }
        serde_json::json!({
            "type": "Feature",
            "geometry": { "type": "Point", "coordinates": [self.lon, self.lat] },
            "properties": props
        })
    }

    pub fn from_feature(v: &serde_json::Value) -> Option<Contact> {
        let props = v.get("properties")?;
        let mut c: Contact = serde_json::from_value(props.clone()).ok()?;
        // geometry wins over the mirrored properties, if they ever disagree
        if let Some(co) = v.pointer("/geometry/coordinates").and_then(|c| c.as_array()) {
            if co.len() >= 2 {
                c.lon = co[0].as_f64().unwrap_or(c.lon);
                c.lat = co[1].as_f64().unwrap_or(c.lat);
            }
        }
        if c.id.is_empty() {
            c.id = props.get("contact_id")?.as_str()?.to_string();
        }
        // A contact with no position is not a contact. Say so by failing --
        // the loader turns that into a refusal to open, not a quiet deletion.
        if !c.lat.is_finite() || !c.lon.is_finite() || (c.lat == 0.0 && c.lon == 0.0) {
            return None;
        }
        Some(c)
    }
}

/// Every contact in a project, kept in one GeoJSON FeatureCollection.
#[derive(Clone, Debug, Default)]
pub struct Contacts {
    /// Set when the file could not be read. Nothing may be written back over a
    /// file we failed to understand: an empty in-memory list saved over a file
    /// full of contacts is a silent deletion of work that cannot be redone.
    pub sealed: bool,
    pub items: Vec<Contact>,
}

impl Contacts {
    pub fn load(path: impl AsRef<Path>) -> io::Result<Contacts> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Contacts::default());
        }
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(path)?)?;
        let empty = Vec::new();
        let feats = v.get("features").and_then(|f| f.as_array()).unwrap_or(&empty);
        let items: Vec<Contact> = feats.iter().filter_map(Contact::from_feature).collect();
        // A contact is a person's observation of the seabed and cannot be
        // recreated from anything else in the workspace. Dropping one because
        // it failed to parse, and then writing the file back without it, is a
        // silent deletion -- so refuse to load at all rather than load a
        // subset that the next save would make permanent.
        if items.len() != feats.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} of {} contacts in {} could not be read; refusing to open the \
                     project rather than overwrite them",
                    feats.len() - items.len(), feats.len(), path.display()
                ),
            ));
        }
        Ok(Contacts { items, sealed: false })
    }

    /// A `Contacts` that refuses to be written back, for when the file on disk
    /// could not be read.
    pub fn sealed() -> Contacts {
        Contacts { items: Vec::new(), sealed: true }
    }

    pub fn save(&self, path: impl AsRef<Path>, project: &str) -> io::Result<()> {
        if self.sealed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the contacts file could not be read, so it will not be overwritten",
            ));
        }
        let path = path.as_ref();
        if let Some(d) = path.parent() {
            fs::create_dir_all(d)?;
        }
        let fc = serde_json::json!({
            "type": "FeatureCollection",
            "properties": { "project": project, "exported": iso8601(now_unix()) },
            "features": self.items.iter().map(|c| c.to_feature()).collect::<Vec<_>>(),
        });
        // Keep the last version. These files are small and the data in them is
        // a day on the water that cannot be repeated; one generation of backup
        // costs nothing and has to exist before it is needed.
        if path.exists() {
            let _ = fs::copy(path, path.with_extension("geojson.bak"));
        }
        let tmp = path.with_extension("geojson.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&fc)?)?;
        fs::rename(&tmp, path)
    }

    /// The next free `C###` identifier.
    pub fn next_id(&self) -> String {
        let mut n = 0u32;
        for c in &self.items {
            if let Some(d) = c.id.strip_prefix('C') {
                if let Ok(v) = d.parse::<u32>() {
                    n = n.max(v);
                }
            }
        }
        format!("C{:03}", n + 1)
    }

    pub fn get(&self, id: &str) -> Option<&Contact> {
        self.items.iter().find(|c| c.id == id)
    }

    pub fn upsert(&mut self, mut c: Contact) -> String {
        if c.id.is_empty() {
            c.id = self.next_id();
        }
        c.modified = iso8601(now_unix());
        if c.created.is_empty() {
            c.created = c.modified.clone();
        }
        match self.items.iter_mut().find(|x| x.id == c.id) {
            Some(x) => *x = c.clone(),
            None => self.items.push(c.clone()),
        }
        c.id
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let n = self.items.len();
        self.items.retain(|c| c.id != id);
        self.items.len() != n
    }
}

// ---- workspace layout ------------------------------------------------------

/// Where everything lives. One recording per folder under `data/`, everything
/// derived from it under a matching folder in `out/`, and projects -- which
/// combine recordings -- under `projects/`.
#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Workspace {
        Workspace { root: root.into() }
    }
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }
    pub fn out_dir(&self, dataset: &str) -> PathBuf {
        self.root.join("out").join(dataset)
    }
    pub fn index_path(&self, dataset: &str) -> PathBuf {
        prefer_existing(self.out_dir(dataset).join("ping_index.swi"), "wpi")
    }
    /// Where a mosaic built with a particular set of settings lives.
    ///
    /// The settings digest is in the file name on purpose. A mosaic is a
    /// picture of a *decision* -- this layback, this priority table -- and two
    /// decisions are two pictures. Keying by subsystem alone meant changing the
    /// navigation left the old raster on disk and the viewer went on showing
    /// imagery that no longer matched the track drawn over it.
    pub fn mosaic_path(&self, dataset: &str, subsystem: u8, key: &str) -> PathBuf {
        prefer_existing(
            self.out_dir(dataset)
                .join(format!("mosaic_{subsystem}_{key}.swm")),
            "wpm",
        )
    }

    /// Every mosaic on disk for a subsystem, oldest first.
    pub fn mosaics_for(&self, dataset: &str, subsystem: u8) -> Vec<PathBuf> {
        let prefix = format!("mosaic_{subsystem}_");
        let Ok(rd) = fs::read_dir(self.out_dir(dataset)) else { return Vec::new() };
        let mut v: Vec<(std::time::SystemTime, PathBuf)> = rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with(&prefix)
                    && (has_ext(&e.path(), "swm") || has_ext(&e.path(), "wpm"))
            })
            .map(|e| {
                let t = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (t, e.path())
            })
            .collect();
        v.sort();
        v.into_iter().map(|(_, p)| p).collect()
    }

    /// Keep the most recent `keep` mosaics for a subsystem and delete the rest.
    ///
    /// Trying two laybacks should not cost a gigabyte per attempt, but throwing
    /// the previous one away immediately would make going back to it a rebuild.
    pub fn prune_mosaics(&self, dataset: &str, subsystem: u8, keep: usize) {
        let all = self.mosaics_for(dataset, subsystem);
        let drop = all.len().saturating_sub(keep);
        for p in all.into_iter().take(drop) {
            let _ = fs::remove_file(p);
        }
    }
    pub fn project_dir(&self, project: &str) -> PathBuf {
        self.root.join("projects").join(project)
    }
    pub fn project_path(&self, project: &str) -> PathBuf {
        self.project_dir(project).join("project.json")
    }
    pub fn contacts_path(&self, project: &str) -> PathBuf {
        self.project_dir(project).join("marks.geojson")
    }
    /// Where imported layers are resampled to.
    ///
    /// Beside the other derived products rather than inside the project: two
    /// projects over the same ground share one copy of a 300 MB grid, and
    /// deleting a project does not throw away half an hour of resampling.
    pub fn layers_dir(&self) -> PathBuf {
        self.root.join("out").join("layers")
    }

    pub fn layer_cache_path(&self, key: &str) -> PathBuf {
        prefer_existing(self.layers_dir().join(format!("{key}.swl")), "wpl")
    }

    pub fn snaps_dir(&self, project: &str) -> PathBuf {
        self.project_dir(project).join("snaps")
    }
    /// Map views rendered for the report.
    ///
    /// The viewer draws them and posts them here, so the report shows the chart
    /// the operator was actually looking at -- the same projection, the same
    /// layer stack, the same colours -- rather than a second renderer's idea of
    /// it. They are kept so that `survey report` run later still has them.
    pub fn report_dir(&self, project: &str) -> PathBuf {
        self.project_dir(project).join("report")
    }

    pub fn exports_dir(&self, project: &str) -> PathBuf {
        self.project_dir(project).join("exports")
    }

    /// Where a project keeps the recordings it holds itself.
    ///
    /// A recording added from somewhere else on the disk lands here, as a copy,
    /// a move or a symlink -- the operator's choice, because the three answer
    /// three different questions: "give me my own", "this belongs to the job
    /// now", and "leave it where the survey team put it".
    pub fn project_data_dir(&self, project: &str) -> PathBuf {
        self.project_dir(project).join("data")
    }

    /// Datasets on disk: one folder per recording, or a flat `data/` holding
    /// the files loose, which is the older layout and still works.
    pub fn datasets(&self) -> Vec<Dataset> {
        self.datasets_for(None)
    }

    /// Every recording a project can see: the ones it holds, then the shared
    /// pool under `data/`.
    ///
    /// The project's own come first and win a name clash, so a recording moved
    /// into a job is the one that job means by that name. The pool stays
    /// visible because most of these surveys are shared between projects and
    /// copying twenty gigabytes per job to say so would be absurd.
    pub fn datasets_for(&self, project: Option<&str>) -> Vec<Dataset> {
        let mut out: Vec<Dataset> = Vec::new();
        if let Some(p) = project {
            out.extend(scan_datasets(&self.project_data_dir(p), true));
        }
        for d in scan_datasets(&self.data_dir(), false) {
            if !out.iter().any(|x| x.name == d.name) {
                out.push(d);
            }
        }
        // The older layout: recordings loose in `data/` with no folder each.
        if out.is_empty() && has_sonar(&self.data_dir()) {
            out.push(Dataset { name: "_flat".into(), dir: self.data_dir(), owned: false });
        }
        out
    }

    /// Is this name already spoken for anywhere in the workspace?
    ///
    /// Derived products live in `out/<name>`, keyed by the name alone, so two
    /// different recordings called the same thing would silently share an index
    /// and a mosaic. The name is the identity: this is what keeps it one.
    pub fn dataset_name_taken(&self, name: &str) -> bool {
        if self.data_dir().join(name).exists() || self.out_dir(name).exists() {
            return true;
        }
        self.projects().iter().any(|p| self.project_data_dir(p).join(name).exists())
    }

    /// `want` if it is free, else `want-2`, `want-3` and so on.
    pub fn free_dataset_name(&self, want: &str) -> String {
        let base = sanitise_name(want);
        if !self.dataset_name_taken(&base) {
            return base;
        }
        (2..1000)
            .map(|n| format!("{base}-{n}"))
            .find(|c| !self.dataset_name_taken(c))
            .unwrap_or_else(|| format!("{base}-{}", now_unix() as u64))
    }

    /// Bring a folder of recordings into a project.
    ///
    /// Returns where it landed. The name has to be free workspace-wide -- see
    /// `dataset_name_taken` -- and the source has to hold sonar files directly,
    /// which is the same thing `datasets` means by a recording.
    pub fn import_dataset(
        &self,
        project: &str,
        src: &Path,
        name: &str,
        how: Placement,
    ) -> io::Result<PathBuf> {
        let src = fs::canonicalize(src)?;
        if !src.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a folder"));
        }
        if !has_sonar(&src) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} holds no .jsf or .xtf files", src.display()),
            ));
        }
        if name != sanitise_name(name) || name.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a recording name is one plain folder name",
            ));
        }
        if self.dataset_name_taken(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a recording called {name} is already in this workspace"),
            ));
        }
        let root = fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        if how == Placement::Move && (root.starts_with(&src) || src == root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "that folder contains the workspace; moving it would take the workspace with it",
            ));
        }
        let dir = self.project_data_dir(project);
        fs::create_dir_all(&dir)?;
        let dest = dir.join(name);
        if dest.exists() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "already there"));
        }
        match how {
            Placement::Link => symlink_dir(&src, &dest)?,
            Placement::Copy => copy_dir(&src, &dest)?,
            // Across filesystems a rename cannot work, and a survey drive is a
            // different filesystem more often than not.
            Placement::Move => {
                if fs::rename(&src, &dest).is_err() {
                    copy_dir(&src, &dest)?;
                    fs::remove_dir_all(&src)?;
                }
            }
        }
        Ok(dest)
    }

    /// Take a recording back out of a project's own folder.
    ///
    /// Only the project's own: one in the shared pool under `data/` is not this
    /// project's to delete. A link is unlinked and the recording it pointed at
    /// is left alone; a copied or moved one goes for good, which is why the
    /// caller has to have asked.
    pub fn remove_dataset(&self, project: &str, name: &str) -> io::Result<()> {
        if name != sanitise_name(name) || name.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a recording name"));
        }
        let dir = self.project_data_dir(project).join(name);
        let Ok(meta) = fs::symlink_metadata(&dir) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{project} does not hold {name}; a recording in data/ is shared, \
                         and unticking it is the whole of removing it here"),
            ));
        };
        if meta.is_symlink() {
            fs::remove_file(&dir)
        } else {
            fs::remove_dir_all(&dir)
        }
    }

    /// Rename a project, folder and all, keeping the name inside the file in
    /// step with the folder it lives in.
    pub fn rename_project(&self, from: &str, to: &str) -> io::Result<()> {
        let (from_dir, to_dir) = self.project_move_paths(from, to)?;
        fs::rename(&from_dir, &to_dir)?;
        let path = to_dir.join("project.json");
        let mut p = Project::load(&path)?;
        p.name = to.to_string();
        p.save(&path)
    }

    /// Copy a project so a second pass at the same survey can start from it.
    ///
    /// The recordings are linked rather than copied: a duplicate is a second
    /// opinion about the same day at sea, and it should cost a kilobyte.
    pub fn duplicate_project(&self, from: &str, to: &str) -> io::Result<()> {
        let (from_dir, to_dir) = self.project_move_paths(from, to)?;
        fs::create_dir_all(&to_dir)?;
        let mut p = Project::load(from_dir.join("project.json"))?;
        p.name = to.to_string();
        p.created = iso8601(now_unix());
        p.save(to_dir.join("project.json"))?;
        let marks = from_dir.join("marks.geojson");
        if marks.exists() {
            fs::copy(&marks, to_dir.join("marks.geojson"))?;
        }
        for d in scan_datasets(&self.project_data_dir(from), true) {
            let src = fs::canonicalize(&d.dir).unwrap_or(d.dir);
            let into = self.project_data_dir(to);
            fs::create_dir_all(&into)?;
            symlink_dir(&src, &into.join(&d.name))?;
        }
        Ok(())
    }

    /// Recordings this project holds outright -- copied or moved in, not
    /// linked. Deleting the project deletes these, and nothing else would have
    /// them.
    pub fn project_holds(&self, project: &str) -> Vec<String> {
        scan_datasets(&self.project_data_dir(project), true)
            .into_iter()
            .filter(|d| !fs::symlink_metadata(&d.dir).map(|m| m.is_symlink()).unwrap_or(false))
            .map(|d| d.name)
            .collect()
    }

    /// Delete a project and everything in its folder.
    ///
    /// Refuses while the project holds a recording of its own unless that is
    /// said out loud, because a linked recording costs nothing to lose and a
    /// moved one is the only copy there is.
    pub fn delete_project(&self, name: &str, with_data: bool) -> io::Result<()> {
        if name != sanitise_name(name) || name.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a project name"));
        }
        let dir = self.project_dir(name);
        if !dir.join("project.json").exists() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("no project {name}")));
        }
        let holds = self.project_holds(name);
        if !holds.is_empty() && !with_data {
            return Err(io::Error::other(format!(
                "{name} holds {}, which nothing else has",
                holds.join(", ")
            )));
        }
        // `remove_dir_all` unlinks a symlink rather than walking through it, so
        // a linked recording keeps its files.
        fs::remove_dir_all(dir)
    }

    /// Check a project rename or copy before anything moves.
    fn project_move_paths(&self, from: &str, to: &str) -> io::Result<(PathBuf, PathBuf)> {
        for n in [from, to] {
            if n != sanitise_name(n) || n.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a project name"));
            }
        }
        let from_dir = self.project_dir(from);
        let to_dir = self.project_dir(to);
        if !from_dir.join("project.json").exists() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("no project {from}")));
        }
        if to_dir.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a project called {to} already exists"),
            ));
        }
        Ok((from_dir, to_dir))
    }

    pub fn projects(&self) -> Vec<String> {
        let Ok(rd) = fs::read_dir(self.root.join("projects")) else { return Vec::new() };
        let mut v: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("project.json").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// Sonar files inside a dataset folder, sorted.
    ///
    /// JSF wins when a folder holds both. Discover writes the XTF as an export
    /// of the same pings, so indexing both would double every ping -- and the
    /// JSF is the one with the full header, so it is the one to keep.
    pub fn sonar_files(&self, dir: &Path) -> Vec<PathBuf> {
        let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
        let mut v: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_sonar(p))
            .collect();
        if v.iter().any(|p| has_ext(p, "jsf")) {
            v.retain(|p| has_ext(p, "jsf"));
        }
        v.sort();
        v
    }
}

/// A recording the workspace can see.
#[derive(Clone, Debug)]
pub struct Dataset {
    pub name: String,
    pub dir: PathBuf,
    /// Held by the project rather than by the shared pool under `data/`.
    pub owned: bool,
}

/// How a recording added from elsewhere gets into the project.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    /// Duplicate the files. The project is then self-contained and the original
    /// is untouched.
    Copy,
    /// Take the files. Nothing else has them afterwards.
    Move,
    /// Point at them where they are. Costs nothing and breaks if the drive the
    /// survey came on is unplugged.
    Link,
}

/// Recording folders directly under `dir`.
fn scan_datasets(dir: &Path, owned: bool) -> Vec<Dataset> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<Dataset> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter(|e| has_sonar(&e.path()))
        .map(|e| Dataset {
            name: e.file_name().to_string_lossy().into_owned(),
            dir: e.path(),
            owned,
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// One plain folder name: no separators, no `..`, nothing hidden.
///
/// These names are pasted into paths under the workspace, so this is the only
/// thing standing between a typed name and a write outside it.
pub fn sanitise_name(s: &str) -> String {
    let s: String = s
        .trim()
        .chars()
        .map(|c| if std::path::is_separator(c) || c == '\0' { '_' } else { c })
        .collect();
    let s = s.trim_matches('.').trim().to_string();
    if s.is_empty() { String::new() } else { s }
}

#[cfg(unix)]
fn symlink_dir(src: &Path, dest: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(src, dest)
}

#[cfg(not(unix))]
fn symlink_dir(src: &Path, dest: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dest)
}

/// Copy a folder, skipping hidden entries.
fn copy_dir(src: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    for e in fs::read_dir(src)?.flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let from = e.path();
        let to = dest.join(&name);
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// The same path under its pre-rename extension, when that is what is on disk.
///
/// `.swi`, `.swm` and `.swl` were `.wpi`, `.wpm` and `.wpl` before the
/// application was renamed. Every one of them is derived and could be rebuilt,
/// but rebuilding means re-reading tens of gigabytes of sonar, so an existing
/// workspace keeps working: a file already written under the old name is read
/// where it lies, and anything new is written under the new one. Nothing
/// converts and nothing is deleted.
fn prefer_existing(new: PathBuf, old_ext: &str) -> PathBuf {
    if new.exists() {
        return new;
    }
    let old = new.with_extension(old_ext);
    if old.exists() {
        return old;
    }
    new
}

pub fn has_ext(p: &Path, ext: &str) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case(ext))
        .unwrap_or(false)
}

fn is_sonar(p: &Path) -> bool {
    p.is_file()
        && p.extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                e == "jsf" || e == "xtf"
            })
            .unwrap_or(false)
}

fn has_sonar(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).any(|e| is_sonar(&e.path())))
        .unwrap_or(false)
}
