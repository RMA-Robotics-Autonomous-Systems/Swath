//! A small HTTP/1.1 server for localhost.
//!
//! Written out rather than pulled in: the surface actually used is a request
//! line, a handful of headers, a body and a response, and on a loopback socket
//! serving one user there is nothing a framework would buy. It also means the
//! desktop shell and the headless mode share one code path with no async
//! runtime underneath either.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use crate::api::{self, Response, State, TileCache};

/// A frontend directory on disk, if there is one to prefer over the built-in.
///
/// The frontend is compiled into the binary (see [`crate::ui`]), so `None` is
/// the ordinary answer for a shipped executable and means nothing is missing.
/// A directory is looked for anyway and wins when it is found, so that editing
/// `ui/` in a checkout still shows up on reload.
///
/// Deliberately not looked for under the workspace. `--root` names the folder
/// holding the recordings, the projects and everything derived from them; it
/// has no reason to carry a copy of the application. Searching it is what let
/// the viewer come up healthy and completely empty -- started one directory
/// off, it found a `ui/` sitting beside the source and a workspace with no
/// recordings in it, and nothing about that looks broken.
///
/// So the directory is looked for from the binary: beside it when a checkout's
/// `ui/` has been installed alongside, and up out of `target/<profile>/` when
/// run from the tree. Nowhere else -- in particular not the source tree this
/// was compiled in, which used to be the last resort and is now the wrong
/// answer twice over. A copied binary would rather serve the frontend it was
/// built with than whatever that checkout has been edited into since, and a
/// build machine's paths have no business deciding what a shipped executable
/// serves.
pub fn ui_dir() -> Option<PathBuf> {
    let mut tried: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        tried.extend(exe.ancestors().skip(1).take(5).map(|d| d.join("ui")));
    }
    tried.into_iter().find(|c| c.join("index.html").exists())
}

/// Upstream fetches allowed to be in flight at once.
///
/// Two is what the OpenStreetMap tile usage policy asks of a bulk client, and
/// it is plenty: these run in the background now, so the number sets how fast
/// a cold area fills in rather than how long anything waits.
const FETCH_WORKERS: usize = 2;

/// Tiles waiting to be fetched before the coldest are dropped.
///
/// A fast pan can name several hundred squares the operator will never look
/// at. The queue is drained newest-first and bounded, so what gets fetched is
/// where they stopped, not where they passed through.
const FETCH_BACKLOG: usize = 96;

pub struct Server {
    pub state: Arc<State>,
    /// Directory the frontend is served from, or `None` for the copy built
    /// into the binary.
    pub ui_dir: Option<std::path::PathBuf>,
    /// Base map tiles, fetched off the request path.
    fetch: Arc<Fetcher>,
}

impl Server {
    pub fn new(state: Arc<State>, ui_dir: Option<std::path::PathBuf>) -> Arc<Server> {
        let me = Arc::new(Server { state, ui_dir, fetch: Fetcher::new() });
        me.fetch.clone().start(me.state.clone());
        me
    }
}

/// One tile of one source.
type Key = (String, u32, i64, i64);

/// Background fetches of base map tiles.
///
/// The point is what it does *not* do: hold the connection. A browser gives an
/// origin six of them, and a cold OpenStreetMap tile took between one and three
/// seconds to arrive -- so six cold squares blocked the sonar underneath them,
/// which was sitting in memory a millisecond away. Now the miss is answered at
/// once and the fetch happens here; the viewer asks again and gets the picture.
pub struct Fetcher {
    /// One agent, so the connection and its TLS session are reused. Built per
    /// request, the handshake cost more than the tile did.
    agent: ureq::Agent,
    queue: Mutex<Queue>,
    wake: Condvar,
}

#[derive(Default)]
struct Queue {
    /// Newest first: a pan leaves a trail, and the end of it is where the
    /// operator is looking.
    want: VecDeque<Key>,
    /// Queued or being fetched, so the same square is never asked for twice.
    claimed: HashSet<Key>,
}

impl Fetcher {
    fn new() -> Arc<Fetcher> {
        Arc::new(Fetcher {
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(20))
                .user_agent("swath/1.0 (local survey tool; single user)")
                .build(),
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
        })
    }

    fn start(self: Arc<Self>, state: Arc<State>) {
        for _ in 0..FETCH_WORKERS {
            let me = self.clone();
            let state = state.clone();
            std::thread::spawn(move || me.run(state));
        }
    }

    /// Ask for a tile, unless it is already spoken for.
    fn want(&self, key: Key) {
        let mut q = self.queue.lock().unwrap();
        if q.claimed.contains(&key) {
            return;
        }
        q.claimed.insert(key.clone());
        q.want.push_front(key);
        while q.want.len() > FETCH_BACKLOG {
            if let Some(old) = q.want.pop_back() {
                q.claimed.remove(&old);
            }
        }
        drop(q);
        self.wake.notify_one();
    }

    fn run(&self, state: Arc<State>) {
        loop {
            let key = {
                let mut q = self.queue.lock().unwrap();
                loop {
                    if let Some(k) = q.want.pop_front() {
                        break k;
                    }
                    q = self.wake.wait(q).unwrap();
                }
            };
            self.fetch(&state, &key);
            self.queue.lock().unwrap().claimed.remove(&key);
        }
    }

    fn fetch(&self, state: &State, key: &Key) {
        let (layer, z, x, y) = key;
        let Some(url) = TileCache::url(layer, *z, *x, *y) else { return };
        match self.agent.get(&url).call() {
            Ok(r) => {
                let mut buf = Vec::new();
                if r.into_reader().take(4 << 20).read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    let _ = state.tiles.put(layer, *z, *x, *y, &buf);
                }
            }
            // A missing overlay tile is the normal case for OpenSeaMap, so the
            // blank is written down rather than asked for again on every pan --
            // but it carries its own age, and `TileCache::get` stops believing
            // it after a day.
            Err(ureq::Error::Status(404, _)) => {
                let _ = state.tiles.put(layer, *z, *x, *y, api::BLANK_PNG);
            }
            Err(_) => {}
        }
    }
}

impl Server {
    pub fn serve(self: Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let me = self.clone();
            std::thread::spawn(move || {
                if let Err(e) = me.handle_conn(stream) {
                    // a browser closing a tile request mid-flight is routine
                    if e.kind() != std::io::ErrorKind::BrokenPipe {
                        eprintln!("connection: {e}");
                    }
                }
            });
        }
        Ok(())
    }

    fn handle_conn(&self, mut stream: TcpStream) -> std::io::Result<()> {
        stream.set_nodelay(true).ok();
        let mut reader = BufReader::new(stream.try_clone()?);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let mut parts = line.trim_end().split(' ');
            let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
                return Ok(());
            };
            let method = method.to_string();
            let target = target.to_string();

            let mut headers: HashMap<String, String> = HashMap::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h)? == 0 {
                    return Ok(());
                }
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }

            let len: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            if len > 0 {
                reader.read_exact(&mut body)?;
            }

            let (path, query) = split_query(&target);
            let resp = self.route(&method, &path, &query, &body);
            write_response(&mut stream, resp)?;

            if headers.get("connection").map(|c| c.eq_ignore_ascii_case("close")).unwrap_or(false)
            {
                return Ok(());
            }
        }
    }

    fn route(
        &self,
        method: &str,
        path: &str,
        query: &HashMap<String, String>,
        body: &[u8],
    ) -> Response {
        // Base map tiles: cache on disk, fetch once. Handled here rather than in
        // the router because the router is deliberately network-free.
        let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
        if let ("GET", ["api", "basemap", layer, z, x, y]) = (method, seg.as_slice()) {
            return self.basemap(layer, z, x, y);
        }
        // Writing the report is the router's job; opening it is not. A window
        // that has nowhere to put a new tab -- which is every desktop webview --
        // needs the page handed to whatever the desktop uses for HTML, and that
        // is a process launch, so it belongs out here with the other things the
        // router is kept clear of.
        if let ("POST", ["api", "report", "open"]) = (method, seg.as_slice()) {
            return match api::save_report(&self.state) {
                Ok(p) => {
                    let opened = open_externally(&p);
                    Response::json(serde_json::json!({
                        "path": p.to_string_lossy(), "opened": opened,
                    }))
                }
                Err(e) => Response::err(400, e),
            };
        }
        if path.starts_with("/api/") {
            return api::handle(&self.state, method, path, query, body);
        }
        self.static_file(path)
    }

    /// A base map tile: from disk if it is there, and a request to go and get
    /// it if it is not.
    ///
    /// The miss is answered immediately rather than waited on. Fetching inline
    /// held one of the browser's six connections for one to three seconds, and
    /// the local imagery -- ready in about two milliseconds -- queued behind it;
    /// a screen of fresh water took twelve seconds to draw, nearly all of it
    /// spent on somebody else's server. Now the viewer is told to come back,
    /// and it does.
    fn basemap(&self, layer: &str, z: &str, x: &str, y: &str) -> Response {
        let (Ok(z), Ok(x), Ok(y)) = (
            z.parse::<u32>(),
            x.parse::<i64>(),
            y.trim_end_matches(".png").parse::<i64>(),
        ) else {
            return Response::err(400, "bad tile");
        };
        if z > 21 || x < 0 || y < 0 || x >= 1i64 << z || y >= 1i64 << z {
            return Response::err(400, "tile out of range");
        }
        if let Some(b) = self.state.tiles.get(layer, z, x, y) {
            return Response::png(b, 86400);
        }
        if TileCache::url(layer, z, x, y).is_none() {
            return Response::err(404, "unknown layer");
        }
        self.fetch.want((layer.to_string(), z, x, y));
        // 202: nothing is wrong, the picture is on its way. Not cached, because
        // the next ask is the one that should find it.
        Response::pending()
    }

    /// One file of the frontend.
    ///
    /// From the directory when there is one, and from the copy built into the
    /// binary when there is not. A directory, once chosen, is the whole
    /// frontend: a miss inside it is a 404 rather than a quiet fall through to
    /// the built-in copy, which would answer an edit in progress with a file of
    /// a different vintage and look like the edit had no effect.
    fn static_file(&self, path: &str) -> Response {
        let rel = if path == "/" { "index.html" } else { path.trim_start_matches('/') };
        // no traversal out of the ui directory
        if rel.contains("..") {
            return Response::err(400, "bad path");
        }
        let bytes = match &self.ui_dir {
            Some(dir) => match std::fs::read(dir.join(rel)) {
                Ok(b) => b,
                Err(_) => return Response::err(404, "not found"),
            },
            None => match crate::ui::get(rel) {
                Some(b) => b.to_vec(),
                None => return Response::err(404, "not found"),
            },
        };
        Response::raw(bytes, content_type(rel), 0)
    }
}

/// What to call a file the browser asked for, by extension.
fn content_type(rel: &str) -> &'static str {
    match rel.rsplit_once('.').map(|(_, e)| e) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn split_query(target: &str) -> (String, HashMap<String, String>) {
    let mut q = HashMap::new();
    let (path, qs) = match target.split_once('?') {
        Some((p, s)) => (p, s),
        None => (target, ""),
    };
    for pair in qs.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        q.insert(urldecode(k), urldecode(v));
    }
    (urldecode(path), q)
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn write_response(stream: &mut TcpStream, resp: Response) -> std::io::Result<()> {
    let status = resp.status;
    let ct = resp.content_type();
    let cache = resp.cache_s;
    let body = resp.bytes();
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if cache > 0 {
        head.push_str(&format!("Cache-Control: max-age={cache}\r\n"));
    } else {
        head.push_str("Cache-Control: no-store\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// Hand a file to the desktop to open, and say whether that worked.
///
/// Best effort by design: a headless `survey serve` has no desktop to hand it
/// to, and that is not a failure -- the caller is told where the file is and
/// can open it however it likes.
fn open_externally(path: &std::path::Path) -> bool {
    hand_to_desktop(path.as_os_str())
}

/// Hand an address to whatever the desktop opens links with.
///
/// The same best-effort contract as [`open_externally`], and for a sharper
/// reason: `swath serve` may well be running on a machine whose browser is
/// somebody else's, over the network or through a tunnel. Failing to open one
/// locally is not a failure of the command -- the address is printed either
/// way, and that is the part that matters.
pub fn open_in_browser(url: &str) -> bool {
    hand_to_desktop(url.as_ref())
}

fn hand_to_desktop(arg: &std::ffi::OsStr) -> bool {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        // Also the shell's URL handler: `explorer https://...` opens the
        // default browser, which is why this works for both callers.
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}
