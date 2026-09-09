//! Where the tests look for things.
//!
//! Two different roots, which used to be conflated and were both written out
//! by hand in three test files. They are not the same thing and they moved in
//! opposite directions when the app was promoted to the repository root.

#![allow(dead_code)]

use std::path::PathBuf;

/// The repository root: `fixtures/`, `crates/` and `ui/` sit directly under it.
///
/// Everything here is committed, so this always exists.
pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The workspace to test against: the folder holding `data/` and `out/`.
///
/// Nothing under it is committed -- the recordings are tens of gigabytes and
/// the derived products are rebuilt from them -- so every test that wants one
/// checks and skips rather than failing. Point `SWATH_WORKSPACE` at a real
/// workspace to run those; the default is the checkout itself, which is where
/// a workspace lands when the app is run from its own source tree.
pub fn workspace() -> PathBuf {
    std::env::var_os("SWATH_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(repo)
}
