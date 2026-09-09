//! The frontend, as bytes, compiled into the library.
//!
//! This is what makes a single file an application. There is no asset
//! directory to install beside the executable, no path for it to be wrong
//! about, and no way to end up running the code of one version against the
//! frontend of another -- which is the failure that matters, because it does
//! not look like a failure. It looks like a chart that is subtly out of date.
//!
//! The table is written by `build.rs` from the contents of `ui/`. A directory
//! on disk still wins when there is one, so editing the frontend in a checkout
//! works as it always did; see [`crate::server::ui_dir`].

include!(concat!(env!("OUT_DIR"), "/ui.rs"));

/// One file, by the path the browser asks for -- `index.html`, `app.js`.
///
/// A linear scan over about a dozen entries, which a page load does a handful
/// of times. A map would be more code than the thing it indexes.
pub fn get(rel: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|(p, _)| *p == rel).map(|(_, b)| *b)
}
