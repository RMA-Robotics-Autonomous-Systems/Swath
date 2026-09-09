//! Carry the frontend inside the library.
//!
//! `ui/` is a dozen files with no build step, so there is nothing to compile
//! here -- only to pick up. It is walked rather than listed by hand because of
//! the one failure that would otherwise be waiting: a file that exists, loads
//! fine from a checkout, and 404s out of the shipped executable because nobody
//! remembered to add it to a table.
//!
//! What goes into the generated table is `include_bytes!` of a path, not the
//! bytes themselves. rustc then records each file as a dependency of the crate,
//! so editing the frontend rebuilds on its own; cargo is told to watch the
//! directories as well, which is what catches a file added or removed.

use std::path::{Path, PathBuf};

fn main() {
    let ui = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../ui");
    let ui = ui.canonicalize().unwrap_or_else(|e| panic!("no ui/ at {}: {e}", ui.display()));

    let mut files = Vec::new();
    collect(&ui, "", &mut files);
    files.sort();
    assert!(
        files.iter().any(|(rel, _)| rel == "index.html"),
        "no index.html in {} -- a binary built from this would serve nothing",
        ui.display()
    );

    let mut src = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (rel, path) in &files {
        // `{:?}` on a str is a Rust string literal, escaping included.
        src.push_str(&format!("    ({:?}, include_bytes!({:?})),\n", rel, path.to_string_lossy()));
    }
    src.push_str("];\n");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("ui.rs");
    std::fs::write(&out, src).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
}

/// Every file under `ui/`, as (path the browser asks for, path on disk).
///
/// `test/` is the frontend's own harness: it runs under bun from a checkout and
/// has no business inside a shipped binary. Hidden entries are skipped for the
/// same reason `copy_dir` skips them in a workspace -- an editor's leftovers are
/// not part of the application.
fn collect(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || (prefix.is_empty() && name == "test") {
            continue;
        }
        let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        if e.path().is_dir() {
            collect(&e.path(), &rel, out);
        } else {
            out.push((rel, e.path()));
        }
    }
}
