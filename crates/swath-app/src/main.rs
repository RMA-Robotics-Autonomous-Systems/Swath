//! Desktop shell.
//!
//! The window is pointed at a loopback server this process starts, rather than
//! at bundled assets talking to the backend over Tauri's IPC. Two reasons, and
//! both are about the data: tiles and waterfall frames are hundreds of
//! kilobytes at a time and IPC would serialise every one of them through JSON,
//! and pointing a plain browser at the same URL during development means the
//! shell is never the thing being debugged.
//!
//! It also means `swath serve` and this binary are the same application. There
//! is one router, in `swath_core::api`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use swath_core::api::State;
use swath_core::server::Server;
use tauri::{WebviewUrl, WebviewWindowBuilder};

fn main() {
    let root = workspace_root();
    let ui_dir = match swath_core::server::ui_dir() {
        Some(d) => d,
        None => {
            eprintln!("no frontend found beside the binary");
            std::process::exit(1);
        }
    };

    // Port 0: the OS picks a free one. Binding before the window is built so
    // the address is known by the time the webview needs it.
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind loopback: {e}");
            std::process::exit(1);
        }
    };
    let addr = listener.local_addr().expect("local addr");
    let url = format!("http://{addr}/");
    eprintln!("swath {} — {url}", swath_core::VERSION);
    eprintln!("  workspace: {}", root.display());

    let server = Server::new(Arc::new(State::new(root.clone())), ui_dir);
    std::thread::spawn(move || {
        if let Err(e) = server.serve(listener) {
            eprintln!("server stopped: {e}");
        }
    });

    tauri::Builder::default()
        .setup(move |app| {
            let win = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::External(url.parse().expect("url")),
            )
            .title("swath")
            .inner_size(1600.0, 980.0)
            .min_inner_size(1100.0, 700.0)
            .build()?;
            // The chart is dark; a white flash on open is jarring at night.
            let _ = win.set_background_color(Some(tauri::window::Color(14, 18, 22, 255)));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("run");
}

/// The workspace to open: an explicit argument, else the directory the binary
/// sits in if it looks like one, else the current directory.
fn workspace_root() -> PathBuf {
    if let Some(a) = std::env::args().nth(1) {
        return PathBuf::from(a);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("data").is_dir() {
        return cwd;
    }
    // running out of target/release, so the workspace may be a few levels up
    if let Ok(exe) = std::env::current_exe() {
        for up in exe.ancestors().skip(1).take(5) {
            if up.join("data").is_dir() {
                return up.to_path_buf();
            }
        }
    }
    cwd
}

