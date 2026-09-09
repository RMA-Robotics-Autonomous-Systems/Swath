//! GPX: tracks, routes and waypoints.
//!
//! A survey arrives with these from every direction -- a planned line list off
//! the bridge, a RIB's track, a set of marks someone dropped on a handheld --
//! and they are the cheapest possible check on the navigation, because they
//! were recorded by a different instrument.
//!
//! This is a scanner for the GPX vocabulary, not an XML parser. GPX is a shallow
//! schema with no mixed content, no entities worth speaking of and no
//! namespaces that change the meaning, so walking the tags directly is honest
//! and about a hundred lines. What it will not do is accept arbitrary XML: a
//! file whose structure it does not recognise comes back with no features
//! rather than with a guess.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::index::Bounds;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Point {
    pub lat: f64,
    pub lon: f64,
    /// Elevation in metres, where the file carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ele: Option<f64>,
    /// Unix seconds, where the file carries a timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Line {
    pub name: String,
    pub points: Vec<Point>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Waypoint {
    pub name: String,
    #[serde(default)]
    pub desc: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ele: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gpx {
    pub name: String,
    pub tracks: Vec<Line>,
    pub routes: Vec<Line>,
    pub waypoints: Vec<Waypoint>,
    pub bounds: Bounds,
}

impl Default for Gpx {
    fn default() -> Gpx {
        Gpx {
            name: String::new(),
            tracks: Vec::new(),
            routes: Vec::new(),
            waypoints: Vec::new(),
            // An empty bound is inverted, so the first point sets both ends.
            bounds: Bounds::EMPTY,
        }
    }
}

impl Gpx {
    pub fn points(&self) -> usize {
        self.tracks.iter().chain(&self.routes).map(|l| l.points.len()).sum::<usize>()
            + self.waypoints.len()
    }

    /// Total length of every track and route, metres.
    pub fn length_m(&self) -> f64 {
        self.tracks
            .iter()
            .chain(&self.routes)
            .flat_map(|l| l.points.windows(2))
            .map(|w| crate::geo::distance_m(w[0].lat, w[0].lon, w[1].lat, w[1].lon))
            .sum()
    }

    pub fn describe(&self) -> String {
        let km = self.length_m() / 1000.0;
        format!(
            "{} track{} · {} route{} · {} waypoint{} · {km:.2} km",
            self.tracks.len(),
            plural(self.tracks.len()),
            self.routes.len(),
            plural(self.routes.len()),
            self.waypoints.len(),
            plural(self.waypoints.len()),
        )
    }

    /// GeoJSON, so the viewer draws it with the same code that draws contacts
    /// and anything downstream can read it without a converter.
    pub fn to_geojson(&self) -> serde_json::Value {
        let mut features = Vec::new();
        let line = |l: &Line, kind: &str| {
            serde_json::json!({
                "type": "Feature",
                "geometry": {
                    "type": "LineString",
                    "coordinates": l.points.iter()
                        .map(|p| serde_json::json!([p.lon, p.lat]))
                        .collect::<Vec<_>>(),
                },
                "properties": {
                    "kind": kind, "name": l.name, "points": l.points.len(),
                }
            })
        };
        for t in &self.tracks {
            features.push(line(t, "track"));
        }
        for r in &self.routes {
            features.push(line(r, "route"));
        }
        for w in &self.waypoints {
            features.push(serde_json::json!({
                "type": "Feature",
                "geometry": { "type": "Point", "coordinates": [w.lon, w.lat] },
                "properties": {
                    "kind": "waypoint", "name": w.name, "desc": w.desc, "ele": w.ele,
                }
            }));
        }
        serde_json::json!({
            "type": "FeatureCollection",
            "properties": { "name": self.name, "bounds": self.bounds },
            "features": features,
        })
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Gpx> {
    parse(&std::fs::read_to_string(path)?)
}

/// One element as the scanner sees it.
struct Tag<'a> {
    name: &'a str,
    attrs: &'a str,
    /// `<foo/>`, which has no closing tag and therefore no text.
    self_closing: bool,
    closing: bool,
}

/// Walk the tags in document order. Text between tags is handed back with the
/// tag that opened it.
fn tags(src: &str) -> impl Iterator<Item = (Tag<'_>, &str)> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let b = src.as_bytes();
        loop {
            let open = src[at..].find('<')? + at;
            let close = src[at..].find('>').map(|i| i + at)?;
            if close < open {
                at = close + 1;
                continue;
            }
            let inner = &src[open + 1..close];
            at = close + 1;
            // skip declarations, comments and processing instructions
            if inner.starts_with('?') || inner.starts_with('!') {
                continue;
            }
            let closing = inner.starts_with('/');
            let self_closing = inner.ends_with('/');
            let body = inner.trim_start_matches('/').trim_end_matches('/');
            let (name, attrs) = match body.find(|c: char| c.is_ascii_whitespace()) {
                Some(i) => (&body[..i], &body[i..]),
                None => (body, ""),
            };
            // text runs to the next '<'
            let text_end = src[at..].find('<').map(|i| i + at).unwrap_or(b.len());
            let text = src[at..text_end].trim();
            return Some((
                Tag { name, attrs, self_closing, closing },
                text,
            ));
        }
    })
}

/// Pull one attribute out of an attribute string.
fn attr(attrs: &str, key: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some(i) = rest.find(key) {
        let after = &rest[i + key.len()..];
        let before_ok = i == 0 || rest.as_bytes()[i - 1].is_ascii_whitespace();
        let after = after.trim_start();
        if before_ok && after.starts_with('=') {
            let v = after[1..].trim_start();
            let q = v.chars().next()?;
            if q == '"' || q == '\'' {
                let end = v[1..].find(q)? + 1;
                return Some(unescape(&v[1..end]));
            }
        }
        rest = &rest[i + key.len()..];
    }
    None
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Seconds since the epoch from an ISO 8601 timestamp, which is the only form
/// GPX allows.
fn iso_to_unix(s: &str) -> Option<f64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, da): (i32, u32, u32) =
        (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let time = rest.trim_end_matches('Z');
    let time = time.split(['+', '-']).next()?;
    let mut t = time.split(':');
    let h: f64 = t.next()?.parse().ok()?;
    let mi: f64 = t.next().unwrap_or("0").parse().ok()?;
    let se: f64 = t.next().unwrap_or("0").parse().ok()?;
    Some(crate::time::days_from_civil(y, mo, da) as f64 * 86400.0 + h * 3600.0 + mi * 60.0 + se)
}

pub fn parse(src: &str) -> io::Result<Gpx> {
    let mut g = Gpx::default();

    // Where we are: which container, and which point is being filled in.
    let mut in_track = false;
    let mut in_route = false;
    let mut cur_line: Option<Line> = None;
    let mut cur_pt: Option<Point> = None;
    let mut cur_wpt: Option<Waypoint> = None;
    let mut saw_gpx = false;

    for (t, text) in tags(src) {
        if t.closing {
            match t.name {
                "trk" => {
                    if let Some(l) = cur_line.take() {
                        if !l.points.is_empty() {
                            g.tracks.push(l);
                        }
                    }
                    in_track = false;
                }
                "rte" => {
                    if let Some(l) = cur_line.take() {
                        if !l.points.is_empty() {
                            g.routes.push(l);
                        }
                    }
                    in_route = false;
                }
                "trkpt" | "rtept" => {
                    if let (Some(p), Some(l)) = (cur_pt.take(), cur_line.as_mut()) {
                        g.bounds.extend(p.lat, p.lon);
                        l.points.push(p);
                    }
                }
                "wpt" => {
                    if let Some(w) = cur_wpt.take() {
                        g.bounds.extend(w.lat, w.lon);
                        g.waypoints.push(w);
                    }
                }
                _ => {}
            }
            continue;
        }

        match t.name {
            "gpx" => saw_gpx = true,
            "trk" => {
                in_track = true;
                cur_line = Some(Line::default());
            }
            "rte" => {
                in_route = true;
                cur_line = Some(Line::default());
            }
            "trkpt" | "rtept" | "wpt" => {
                let lat = attr(t.attrs, "lat").and_then(|v| v.parse::<f64>().ok());
                let lon = attr(t.attrs, "lon").and_then(|v| v.parse::<f64>().ok());
                let (Some(lat), Some(lon)) = (lat, lon) else { continue };
                if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
                    continue;
                }
                if t.name == "wpt" {
                    cur_wpt = Some(Waypoint { lat, lon, ..Default::default() });
                } else {
                    cur_pt = Some(Point { lat, lon, ..Default::default() });
                }
                // `<trkpt lat=".." lon=".."/>` never gets a closing tag
                if t.self_closing {
                    if let (Some(p), Some(l)) = (cur_pt.take(), cur_line.as_mut()) {
                        g.bounds.extend(p.lat, p.lon);
                        l.points.push(p);
                    }
                    if let Some(w) = cur_wpt.take() {
                        g.bounds.extend(w.lat, w.lon);
                        g.waypoints.push(w);
                    }
                }
            }
            "name" if !text.is_empty() => {
                let name = unescape(text);
                if let Some(w) = cur_wpt.as_mut() {
                    w.name = name;
                } else if let Some(l) = cur_line.as_mut() {
                    if l.name.is_empty() {
                        l.name = name;
                    }
                } else if g.name.is_empty() && !in_track && !in_route {
                    g.name = name;
                }
            }
            "desc" | "cmt" if !text.is_empty() => {
                if let Some(w) = cur_wpt.as_mut() {
                    if w.desc.is_empty() {
                        w.desc = unescape(text);
                    }
                }
            }
            "ele" if !text.is_empty() => {
                let v = text.parse::<f64>().ok();
                if let Some(p) = cur_pt.as_mut() {
                    p.ele = v;
                } else if let Some(w) = cur_wpt.as_mut() {
                    w.ele = v;
                }
            }
            "time" if !text.is_empty() => {
                if let Some(p) = cur_pt.as_mut() {
                    p.time = iso_to_unix(text);
                }
            }
            _ => {}
        }
    }
    // A stray unterminated track still counts; losing the operator's line list
    // to a missing closing tag would be worse than accepting it.
    if let Some(l) = cur_line.take() {
        if !l.points.is_empty() {
            if in_route {
                g.routes.push(l);
            } else {
                g.tracks.push(l);
            }
        }
    }
    if !saw_gpx {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no <gpx> element: this does not look like a GPX file",
        ));
    }
    Ok(g)
}
