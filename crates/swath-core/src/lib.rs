//! Core of the survey viewer: read the recordings, work out where the fish
//! was, and turn that into pictures and positions.
//!
//! The layering is deliberate. Nothing in here knows about a window, an HTTP
//! request or a Tauri command; the same library backs the desktop app, the
//! command-line tools and the differential tests against the Python.
//!
//! ```text
//!   ping_space  ---- nav ---->  world
//!   (ping, sample)              (lat, lon)
//!
//!   fish   = nav.position(ping)
//!   slant  = sample * bin_size
//!   ground = sqrt(slant^2 - altitude^2)
//!   world  = fish + ground * bearing(nav.bearing(ping) +/- 90)
//! ```
//!
//! That transform, and its inverse, is the whole application. The waterfall
//! draws it one way, the mosaic draws it the other, and a contact marked in
//! either view has to land in the same place -- so both call the same code.

pub mod api;
pub mod crs;
pub mod geo;
pub mod gpx;
pub mod index;
pub mod jsf;
pub mod layer;
pub mod mosaic;
pub mod nav;
pub mod plan;
pub mod project;
pub mod report;
pub mod server;
pub mod signal;
pub mod tiff;
pub mod time;
pub mod ui;
pub mod waterfall;
pub mod xtf;

/// The speed of sound the topside was configured with when it wrote these
/// recordings, m/s.
///
/// Not a physical constant and not an estimate of the water. It is the number
/// Discover used, and therefore the number every metre-valued field in the
/// files was computed with -- the range setting, the bottom-tracked altitude,
/// the XTF range. Times are truth; metres are this.
///
/// It is 1500 because the sample counts say so. Every distinct range setting in
/// these surveys lands on a round number at 1500 m/s and on nothing at all at
/// any other speed:
///
/// ```text
///   samples  interval    two-way    @1500 m/s   @1524 m/s
///       652   10240 ns    6.68 ms      5.01 m      5.09 m
///      3896   10240 ns   39.90 ms     29.92 m     30.40 m
///      6500   10240 ns   66.56 ms     49.92 m     50.72 m
/// ```
///
/// The operator typed 5, 30 and 50. So this is what un-converts Discover's own
/// numbers, and the water's real speed -- `MosaicConfig::sound_speed_m_s`,
/// measured, 1524 m/s here -- is what converts times into distances of our own.
/// Using one where the other belongs is a 1.6% error in every across-track
/// distance, always short.
pub const C_RECORDED: f64 = 1500.0;

pub use index::{Bounds, PingIndex, PingRecord};
pub use nav::{Bearing, Fix, Nav, NavConfig, Track};
pub use project::{Contact, Contacts, Dataset, Placement, Project, Workspace};

/// Version of this build, for the report footer and the about box.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A short, stable name for a settings object.
///
/// Derived products are cached on disk and in the browser, and both caches are
/// keyed by this. Serialising to JSON first means the digest changes exactly
/// when a field the reader can see changes -- add a field with a default and
/// every existing cache entry is correctly invalidated, because the JSON is
/// different. FNV-1a is not a cryptographic hash and does not need to be: this
/// names a cache entry, it does not authenticate one.
pub fn fingerprint<T: serde::Serialize + ?Sized>(v: &T) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}
