//! Survey report as HTML.
//!
//! HTML rather than a PDF library on purpose. The Python builds its report with
//! reportlab, which has no Rust equivalent worth the name -- but the viewer is
//! already a webview, and a webview can print. So the report is a page with a
//! print stylesheet, and "export PDF" is the browser's own print-to-file. That
//! removes the last dependency the port could not have satisfied, and it means
//! the report can be read in the app before it is exported.

use std::fmt::Write as _;

use crate::crs;
use crate::project::{Contact, Project};
use crate::time::iso8601;

/// A dataset's contribution to the report.
pub struct DatasetSummary {
    pub name: String,
    pub label: String,
    pub pings: usize,
    pub t0: f64,
    pub t1: f64,
    pub line_km: f64,
    pub lines: usize,
    pub subsystems: Vec<u8>,
    pub bounds: Option<crate::index::Bounds>,
    pub layback_m: f64,
    /// Speed of sound the imagery was placed with, m/s.
    pub sound_speed_m_s: f64,
    /// Median and 95th-percentile disagreement between the three layback
    /// models, metres. A floor under the positional uncertainty, not an error
    /// bar. See `nav::model_spread_m`.
    pub position_spread_m: (f64, f64),
    /// Median flying height, metres.
    pub altitude_m: f64,
    /// Inner and outer edge of the main acoustic beam on the seabed, metres
    /// either side of the track. The array is depressed 33 degrees and the
    /// vertical beam is 50 degrees wide, so it lights 32 to 82 degrees off
    /// vertical and nothing inside that -- which is where the nadir band comes
    /// from and why it is fill rather than coverage.
    pub illuminated_m: (f64, f64),
    /// The navigation this recording was solved with. Per recording, because
    /// the layback is a property of how that day was rigged -- and because it
    /// is what places the imagery, it belongs in the deliverable.
    pub nav: crate::nav::NavConfig,
    /// One chart per frequency band. A recording surveyed at two frequencies
    /// produced two different pictures of the same seabed, and stacking them
    /// into one image hides whichever lost the z-order fight.
    pub charts: Vec<Chart>,
}

/// One picture in the report, with the legend for what was drawn on it.
pub struct Chart {
    pub title: String,
    pub subtitle: String,
    /// The rendered view as a data URI, if one was drawn.
    pub image: Option<String>,
    pub legend: Vec<LegendEntry>,
}

/// One row of a map legend.
pub struct LegendEntry {
    pub label: String,
    /// `mosaic`, `track`, `raster`, `vector`.
    pub kind: String,
    /// A flat colour, for a line.
    pub colour: String,
    /// A ramp, as CSS colour stops, for a raster.
    pub ramp: Vec<String>,
    pub detail: String,
}

pub struct ReportInput<'a> {
    pub project: &'a Project,
    pub contacts: &'a [Contact],
    pub datasets: &'a [DatasetSummary],
    /// Whole-survey coverage, one chart per frequency band.
    pub overviews: &'a [Chart],
    /// Per-contact snapshots as data URIs: (contact id, one of `map`,
    /// `waterfall`, `lf`, `hf`, `wf-lf`, `wf-hf`,
    /// the image).
    pub snapshots: Vec<(String, String, String)>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A legend block: one row per layer, with the colour or ramp it was drawn in.
///
/// A chart with three greys and two blues on it is unreadable without one, and
/// a legend generated from the same stack the chart was drawn from cannot drift
/// out of step with it.
fn legend_html(entries: &[LegendEntry], caption: &str) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut h = String::new();
    let _ = write!(h,
        "<div class=\"legend\">\n<h3>Legend</h3>\n<p class=\"lede\">{}</p>\n<ul>\n",
        esc(caption));
    for e in entries {
        let swatch = if !e.ramp.is_empty() {
            format!("<span class=\"sw ramp\" style=\"background:linear-gradient(to right,{})\"></span>",
                e.ramp.iter().map(|c| esc(c)).collect::<Vec<_>>().join(","))
        } else {
            let cls = if e.kind == "track" || e.kind == "vector" { "sw line" } else { "sw" };
            format!("<span class=\"{cls}\" style=\"background:{}\"></span>", esc(&e.colour))
        };
        let _ = write!(h,
            "<li>{swatch}<span class=\"lg-name\">{}</span><span class=\"lg-kind\">{}</span><span class=\"lg-detail\">{}</span></li>\n",
            esc(&e.label), esc(&e.kind), esc(&e.detail));
    }
    h.push_str("</ul>\n</div>\n");
    h
}

/// Build the report.
pub fn render(input: &ReportInput) -> String {
    let p = input.project;
    // Always WGS84, then whatever the project asked for. If it asked for
    // nothing, offer the local UTM zone, since a report with only degrees in it
    // is not much use to anyone holding a chart.
    let mut codes = vec![4326u32];
    if p.meta.report_crs.is_empty() {
        if let Some(c) = input.contacts.first() {
            codes.push(crs::utm_epsg_for(c.lat, c.lon));
        }
    } else {
        codes.extend(p.meta.report_crs.iter().copied());
    }
    codes.dedup();
    let systems: Vec<crs::Crs> = codes.iter().filter_map(|&c| crs::get(c)).collect();

    let mut h = String::with_capacity(64 * 1024);
    let title = if p.title.is_empty() { &p.name } else { &p.title };

    let _ = write!(h, r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>{} — survey report</title>
<style>{}</style>
</head><body>
"#, esc(title), STYLE);

    // ---- cover -------------------------------------------------------------
    let _ = write!(h, r#"<header class="cover">
  <div class="eyebrow">Sidescan survey report</div>
  <h1>{}</h1>
  <dl class="meta">
"#, esc(title));
    for (k, v) in [
        ("Client", &p.meta.client),
        ("Vessel", &p.meta.vessel),
        ("Operator", &p.meta.operator),
        ("Job number", &p.meta.job_number),
        ("Area", &p.meta.area),
    ] {
        if !v.is_empty() {
            let _ = write!(h, "    <dt>{}</dt><dd>{}</dd>\n", k, esc(v));
        }
    }
    let _ = write!(
        h,
        "    <dt>Issued</dt><dd>{}</dd>\n  </dl>\n",
        esc(&iso8601(crate::project::now_unix()))
    );
    if !p.meta.notes.is_empty() {
        let _ = write!(h, "  <p class=\"notes\">{}</p>\n", esc(&p.meta.notes));
    }
    h.push_str("</header>\n");

    // ---- positioning statement --------------------------------------------
    // Every report of this survey has to carry it. The imagery is placed by a
    // constant layback with no reference grid behind it, and a reader who takes
    // a contact position as metre-accurate will send a ROV to the wrong patch
    // of seabed.
    let lay = input.datasets.first().map(|d| d.layback_m).unwrap_or(0.0);
    let _ = write!(h, r#"<section class="callout">
  <h2>Positioning</h2>
  <p>Contact positions are derived from the vessel GNSS antenna, offset astern
  by a constant layback of {lay:.1} m to the towfish, along the smoothed course
  over ground. The cable bounds that offset's <em>length</em> to about a metre.
  Its <em>bearing</em> is not observed by any sensor in this recording.</p>
  <p>On straight run lines, treat positions as accurate to <strong>5&ndash;10 m</strong>.
  <strong>In turns they are worse, and by an unbounded amount.</strong> A constant
  offset astern places the fish wide of the arc the vessel actually sailed;
  a towed body on a cable does the opposite and cuts inside it. Measured on
  these recordings, simply choosing between those two placements moves a
  contact by 9&ndash;15 m at the median while the vessel is turning, and by up
  to 60 m in the tightest turns. Contacts found on a turn should be treated as
  indicative until re-run on a straight line.</p>
  <p>The gap closes only when the fish has settled, and on a survey flown as
  short lines with frequent turns it rarely does: a towed body needs roughly
  three cable lengths of straight running to forget the last turn, and on these
  recordings 92% of pings are inside that. The steady state a constant offset
  assumes is close to never reached.</p>
  <p>Overlap between adjacent lines is not a check on any of this. Sonar
  brightness depends on the direction it was looked at from &mdash; the
  incidence angle on the local slope, and which side of an object its shadow
  falls &mdash; so two passes over the same ground on reciprocal headings do not
  produce comparable pictures. Correlated directly, imagery from one line
  against itself matches at r&nbsp;=&nbsp;0.70 and falls off over a metre or
  two; the same test across headings returns r&nbsp;=&nbsp;0.04 to 0.07 with no
  peak at all. The mosaic therefore cannot be verified against itself, and where
  two passes cover the same seabed the chart shows the better-angled look rather
  than an average of two that agree.</p>
  <p>None of this is improved by re-processing. Closing it needs either a
  reference grid to register the imagery against, or a sensor on the fish that
  measures its bearing from the tow point.</p>
</section>
"#);

    // The measured floor under that statement, per recording. Three defensible
    // models of the same cable, the same vessel track, and this much daylight
    // between them.
    if input.datasets.iter().any(|d| d.position_spread_m.0.is_finite()) {
        h.push_str("<section><h3>Measured model disagreement</h3>\n");
        h.push_str("<p>How far apart the constant-astern, wake-following and taut-cable \
placements put the fish, over each recording. Not an error bar &mdash; nothing here observes \
where the fish was &mdash; but a floor under one.</p>\n");
        h.push_str("<table class=\"grid\">\n<thead><tr><th>Recording</th>\
<th>median</th><th>95th percentile</th><th>speed of sound</th></tr></thead>\n<tbody>\n");
        for d in input.datasets {
            if !d.position_spread_m.0.is_finite() {
                continue;
            }
            let _ = write!(
                h,
                "<tr><td>{}</td><td>{:.1} m</td><td>{:.1} m</td><td>{:.0} m/s</td></tr>\n",
                esc(&d.label), d.position_spread_m.0, d.position_spread_m.1, d.sound_speed_m_s
            );
        }
        h.push_str("</tbody>\n</table>\n</section>\n");
    }

    // ---- what the instrument measures --------------------------------------
    //
    // Worth a paragraph in every report, because the picture looks like a map
    // of the seabed and is not one. A reader who takes it for bathymetry will
    // read depth off a quantity that contains none.
    let swath = input
        .datasets
        .iter()
        .find(|d| d.illuminated_m.0.is_finite())
        .map(|d| format!(
            "On this survey the fish flew at about {:.0} m, so the beam reached the seabed \
from roughly {:.0} m to {:.0} m either side of the track.",
            d.altitude_m, d.illuminated_m.0, d.illuminated_m.1))
        .unwrap_or_default();
    let _ = write!(h, r#"<section class="callout">
  <h2>What the imagery is</h2>
  <p>A side scan sonar records the <em>strength of the echo</em> returning at each
  instant after the transmit pulse. Every sample is an amplitude against a
  travel time; distance is that time multiplied by the speed of sound. Nothing
  in it is a depth. A bright pixel means a strong return &mdash; hard, rough, or
  angled towards the sonar &mdash; and a dark one means a weak return or an
  acoustic shadow. Two pixels of equal brightness can be at very different
  depths.</p>
  <p>Heights quoted for contacts are derived from shadow length and the flying
  height, not measured. Depth beneath the towfish comes from its pressure sensor
  and its own bottom tracking, along the track line only.</p>
  <p>The transducers look sideways and downwards, not straight down: the array
  is depressed 33&deg; with a 50&deg; vertical beam, so it lights the seabed
  between 32&deg; and 82&deg; off vertical. {swath} The band inside that inner
  figure &mdash; directly beneath the fish &mdash; is filled in at the lowest
  priority so the chart has no hole down the middle of every pass, but it is the
  edge of the beam looking at a specular return, and it should be read as fill
  rather than as coverage.</p>
</section>
"#);

    // ---- summary -----------------------------------------------------------
    //
    // What the survey covered, in one table, before any of the detail. A reader
    // who only wants the size of the job should not have to add up a table of
    // recordings to get it.
    let total_km: f64 = input.datasets.iter().map(|d| d.line_km).sum();
    let total_lines: usize = input.datasets.iter().map(|d| d.lines).sum();
    let total_pings: usize = input.datasets.iter().map(|d| d.pings).sum();
    let span = input.datasets.iter().fold(None::<(f64, f64)>, |acc, d| {
        Some(match acc {
            None => (d.t0, d.t1),
            Some((a, b)) => (a.min(d.t0), b.max(d.t1)),
        })
    });
    let mut area = crate::index::Bounds::EMPTY;
    for b in input.datasets.iter().filter_map(|d| d.bounds) {
        area.extend(b.min_lat, b.min_lon);
        area.extend(b.max_lat, b.max_lon);
    }

    h.push_str("<section><h2>Summary</h2>\n<table class=\"grid summary\">\n<tbody>\n");
    {
        let mut srow = |k: &str, v: String| {
            let _ = write!(h, "<tr><th>{k}</th><td>{v}</td></tr>\n");
        };
        srow("Recordings", format!("{}", input.datasets.len()));
        if let Some((t0, t1)) = span {
            srow("Surveyed", format!("{} to {}", esc(&iso8601(t0)), esc(&iso8601(t1))));
            srow("Elapsed", crate::time::hms(t1 - t0));
        }
        srow("Survey lines", format!("{total_lines}"));
        srow("Line length", format!("{total_km:.2} km"));
        srow("Pings", format!("{total_pings}"));
        if !area.is_empty() {
            srow("Extent", format!(
                "{:.5}&nbsp;N {:.5}&nbsp;E to {:.5}&nbsp;N {:.5}&nbsp;E",
                area.min_lat, area.min_lon, area.max_lat, area.max_lon));
        }
        srow("Contacts", format!("{}", input.contacts.len()));
        let imported = input.project.layers.iter()
            .filter(|l| l.kind == crate::project::KIND_RASTER
                     || l.kind == crate::project::KIND_VECTOR).count();
        srow("Imported layers", format!("{imported}"));
        if !input.overviews.is_empty() {
            // The sweep, not the section heading: "553-608 kHz chirp" says
            // something about the survey, "Coverage - High frequency" does not.
            srow("Frequency bands", input.overviews.iter()
                .map(|c| esc(if c.subtitle.is_empty() { &c.title } else { &c.subtitle }))
                .collect::<Vec<_>>().join(", "));
        }
        srow("Coordinate systems", systems.iter()
            .map(|c| format!("{} (EPSG:{})", esc(c.name), c.epsg))
            .collect::<Vec<_>>().join(", "));
    }
    h.push_str("</tbody></table>\n</section>\n");

    // ---- a note when there is nothing to look at ---------------------------
    if input.overviews.is_empty() && input.datasets.iter().all(|d| d.charts.is_empty()) {
        h.push_str("<section class=\"callout\">\n<h2>No charts</h2>\n<p>This report has \
            no map views. The viewer draws them, and it asks first: open the project and \
            press <b>Report</b> to choose what each chart should show.</p>\n</section>\n");
    }

    // ---- coverage, one chart per frequency band -----------------------------
    //
    // One picture per band rather than one picture per survey: the two
    // frequencies are two different answers about the same seabed, and painting
    // them into the same image only shows whichever won the z-order.
    for c in input.overviews {
        h.push_str("<section class=\"page\">\n");
        let _ = write!(h, "<h2>{}</h2>\n", esc(&c.title));
        if !c.subtitle.is_empty() {
            let _ = write!(h, "<p class=\"lede\">{}</p>\n", esc(&c.subtitle));
        }
        match &c.image {
            Some(img) => {
                let _ = write!(h,
                    "<img class=\"overview\" src=\"{img}\" alt=\"{}\">\n", esc(&c.title));
            }
            None => h.push_str("<p class=\"lede\">No view was rendered for this band.</p>\n"),
        }
        h.push_str(&legend_html(&c.legend,
            "Everything drawn on the chart above, top of the stack first."));
        h.push_str("</section>\n");
    }

    // ---- one section per recording -----------------------------------------
    for d in input.datasets {
        let label = if d.label.is_empty() { &d.name } else { &d.label };
        let _ = write!(h, "<section class=\"page\">\n<h2>{}</h2>\n", esc(label));
        if !d.label.is_empty() && d.label != d.name {
            let _ = write!(h, "<p class=\"lede\">Recording <code>{}</code></p>\n", esc(&d.name));
        }
        h.push_str("<div class=\"two-up\">\n<table class=\"grid summary\"><tbody>\n");
        {
            let mut drow = |k: &str, v: String| {
                let _ = write!(h, "<tr><th>{k}</th><td>{v}</td></tr>\n");
            };
            drow("Start (UTC)", esc(&iso8601(d.t0)));
            drow("Duration", crate::time::hms(d.t1 - d.t0));
            drow("Pings", format!("{}", d.pings));
            drow("Survey lines", format!("{}", d.lines));
            drow("Line length", format!("{:.2} km", d.line_km));
            drow("Channels", d.subsystems.iter()
                .map(|s| format!("ss{s}")).collect::<Vec<_>>().join(", "));
            if let Some(b) = d.bounds {
                drow("Extent", format!(
                    "{:.5}&nbsp;N {:.5}&nbsp;E to {:.5}&nbsp;N {:.5}&nbsp;E",
                    b.min_lat, b.min_lon, b.max_lat, b.max_lon));
            }
        }
        h.push_str("</tbody></table>\n");

        // The navigation in full. This is the recording's own answer to "where
        // do you say the imagery is", and it belongs beside the picture it
        // produced rather than in a footnote.
        h.push_str("<table class=\"grid summary\"><tbody>\n");
        {
            let mut nrow = |k: &str, v: String| {
                let _ = write!(h, "<tr><th>{k}</th><td>{v}</td></tr>\n");
            };
            nrow("Layback", format!("{:.1} m", d.nav.layback_m));
            nrow("GPS to tow point", format!("{:.1} m", d.nav.gps_to_towpoint_m));
            nrow("Layback model", match d.nav.model {
                crate::nav::LaybackModel::Astern => "constant, astern",
                crate::nav::LaybackModel::Wake => "follows the wake",
                crate::nav::LaybackModel::Tractrix => "tractrix (taut cable)",
            }.to_string());
            nrow("Swath bearing", match d.nav.bearing {
                crate::nav::Bearing::Cog => "course over ground",
                crate::nav::Bearing::Compass => "fish compass",
            }.to_string());
            nrow("Course baseline", format!("{:.0} s", d.nav.cog_baseline_s));
            nrow("Roll flag", format!("{:.1}\u{b0}", d.nav.roll_flag_deg));
        }
        h.push_str("</tbody></table>\n</div>\n");

        // The numbers first, then what each frequency actually saw.
        for c in &d.charts {
            h.push_str("<div class=\"band\">\n");
            let _ = write!(h, "<h3>{}</h3>\n", esc(&c.title));
            if !c.subtitle.is_empty() {
                let _ = write!(h, "<p class=\"lede\">{}</p>\n", esc(&c.subtitle));
            }
            if let Some(img) = &c.image {
                let _ = write!(h, "<img class=\"overview\" src=\"{img}\" alt=\"{} \u{2014} {}\">\n",
                    esc(label), esc(&c.title));
            }
            h.push_str(&legend_html(&c.legend, "Layers shown on this chart."));
            h.push_str("</div>\n");
        }
        let here: Vec<&Contact> = input.contacts.iter().filter(|c| c.dataset == d.name).collect();
        if !here.is_empty() {
            let _ = write!(h,
                "<p class=\"lede\">{} contact{} marked on this recording: {}</p>\n",
                here.len(), if here.len() == 1 { "" } else { "s" },
                here.iter().map(|c| esc(&c.id)).collect::<Vec<_>>().join(", "));
        }
        h.push_str("</section>\n");
    }

    // ---- contact register --------------------------------------------------
    // Not every deliverable is about the contacts -- a coverage report is a
    // real thing -- so the register is a section the report can be asked for
    // rather than one it always carries.
    if !input.project.report.contacts {
        let _ = write!(
            h,
            "<footer>Generated by survey {} \u{b7} {}</footer>\n</body></html>\n",
            crate::VERSION,
            esc(&iso8601(crate::project::now_unix()))
        );
        return h;
    }
    let _ = write!(
        h,
        "<section><h2>Contacts</h2>\n<p class=\"lede\">{} contact{} recorded.</p>\n",
        input.contacts.len(),
        if input.contacts.len() == 1 { "" } else { "s" }
    );
    // The table below prints coordinates to the metre in four projections, so
    // it has to say in the same breath how well they are known.
    let worst = input
        .datasets
        .iter()
        .map(|d| d.position_spread_m.1)
        .filter(|v| v.is_finite())
        .fold(f64::NAN, f64::max);
    if worst.is_finite() {
        let _ = write!(h, "<p class=\"lede warn\">Positions below are quoted to the metre \
because the projections are exact. They are not known to the metre: the layback models \
disagree by up to {worst:.0} m over these recordings, and that is a floor under the \
uncertainty rather than a measurement of it. See <em>Positioning</em>.</p>\n");
    }
    if !input.contacts.is_empty() {
        h.push_str("<table class=\"grid contacts\">\n<thead><tr><th>ID</th><th>Name</th><th>Class</th><th>Conf.</th>");
        for c in &systems {
            let _ = write!(
                h,
                "<th colspan=\"2\" class=\"crs\">{}<span class=\"epsg\">EPSG:{}</span></th>",
                esc(c.name),
                c.epsg
            );
        }
        h.push_str("<th>Size</th></tr>\n<tr class=\"sub\"><th></th><th></th><th></th><th></th>");
        for c in &systems {
            let _ = write!(h, "<th>{}</th><th>{}</th>", c.axes.0, c.axes.1);
        }
        h.push_str("<th></th></tr></thead>\n<tbody>\n");
        for c in input.contacts {
            let _ = write!(
                h,
                "<tr><td class=\"id\">{}</td><td>{}</td><td>{}</td><td>{}</td>",
                esc(&c.id),
                esc(&c.name),
                esc(&c.class),
                esc(&c.confidence)
            );
            for s in &systems {
                let (x, y) = s.format(c.lat, c.lon);
                let _ = write!(h, "<td class=\"num\">{x}</td><td class=\"num\">{y}</td>");
            }
            let size = match (c.length_m, c.width_m) {
                (Some(l), Some(w)) => format!("{l:.1} × {w:.1} m"),
                (Some(l), None) => format!("{l:.1} m"),
                _ if c.radius_m > 0.0 => format!("r {:.1} m", c.radius_m),
                _ => String::from("—"),
            };
            let _ = write!(h, "<td class=\"num\">{size}</td></tr>\n");
        }
        h.push_str("</tbody></table>\n");
    }
    h.push_str("</section>\n");

    // ---- contact sheets ----------------------------------------------------
    for c in input.contacts {
        let find = |k: &str| {
            input.snapshots.iter()
                .find(|(id, kind, _)| id == &c.id && kind == k)
                .map(|(_, _, u)| u.as_str())
        };
        let _ = write!(h, "<section class=\"page sheet\">\n<h2>{} — {}</h2>\n", esc(&c.id), esc(&c.name));
        // Four pictures: the seabed at each band, and the return at each band.
        // Every one is drawn from a single channel rather than from whatever
        // the chart had switched on, because the same target at two frequencies
        // is the comparison a classification is argued from and blending them
        // into one picture throws away the only part that carries the argument.
        //
        // Chart first, then sonar, so the pairs read across: the two chart
        // crops beside each other, the two sonar crops beside each other.
        //
        // Nothing is offered as a stand-in for a missing one -- not the
        // combined chart crop, not the old single sonar crop, both of which are
        // still on the contact. A sheet is regenerated whenever it is read, so
        // an empty space means that contact has not been through the viewer
        // since these existed, and saying so is more use than filling it with a
        // different picture under the same heading.
        let panes: Vec<(&str, &str)> = [
            ("Chart — low frequency", find("lf")),
            ("Chart — high frequency", find("hf")),
            ("Sonar — low frequency", find("wf-lf")),
            ("Sonar — high frequency", find("wf-hf")),
        ]
        .into_iter()
        .filter_map(|(caption, u)| Some((caption, u?)))
        .collect();
        if !panes.is_empty() {
            h.push_str("<div class=\"snaps\">\n");
            for (caption, u) in panes {
                let _ = write!(h,
                    "<figure><img class=\"snap\" src=\"{u}\" alt=\"Contact {} — {caption}\">\
                     <figcaption>{caption}</figcaption></figure>\n", esc(&c.id));
            }
            h.push_str("</div>\n");
        }
        h.push_str("<dl class=\"detail\">\n");
        let mut row = |k: &str, v: String| {
            if !v.is_empty() && v != "—" {
                let _ = write!(h, "<dt>{k}</dt><dd>{}</dd>\n", esc(&v));
            }
        };
        row("Class", c.class.clone());
        row("Confidence", c.confidence.clone());
        row("Status", c.status.clone());
        row("Recording", c.dataset.clone());
        if let Some(t) = c.time {
            row("Time (UTC)", iso8601(t));
        }
        row("Latitude", crs::to_dm(c.lat, true));
        row("Longitude", crs::to_dm(c.lon, false));
        for s in systems.iter().filter(|s| !s.is_geographic()) {
            let (x, y) = s.format(c.lat, c.lon);
            row(s.name, format!("{} {}, {} {} ({})", s.axes.0, x, s.axes.1, y, s.unit));
        }
        if let Some(d) = c.depth_m {
            row("Water depth", format!("{d:.1} m"));
        }
        if let Some(a) = c.altitude_m {
            row("Fish altitude", format!("{a:.1} m"));
        }
        if let Some(a) = c.across_m {
            row("Across-track", format!("{a:.1} m {}", if a < 0.0 { "port" } else { "starboard" }));
        }
        match (c.length_m, c.width_m, c.height_m) {
            (None, None, None) => {}
            (l, w, ht) => row(
                "Dimensions",
                [l.map(|v| format!("L {v:.1} m")), w.map(|v| format!("W {v:.1} m")), ht.map(|v| format!("H {v:.1} m"))]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · "),
            ),
        }
        h.push_str("</dl>\n");
        if !c.note.is_empty() {
            let _ = write!(h, "<p class=\"note\">{}</p>\n", esc(&c.note));
        }
        h.push_str("</section>\n");
    }

    let _ = write!(
        h,
        "<footer>Generated by survey {} · {}</footer>\n</body></html>\n",
        crate::VERSION,
        esc(&iso8601(crate::project::now_unix()))
    );
    h
}

const STYLE: &str = r#"
:root {
  --ink: #14181d; --muted: #5b6570; --rule: #d8dde3; --paper: #ffffff;
  --accent: #0f6d8c; --flag: #b4471f; --band: #f4f6f8;
}
* { box-sizing: border-box; }
body {
  margin: 0; padding: 0 0 4rem; background: var(--paper); color: var(--ink);
  font: 10.5pt/1.55 "Source Serif 4", Charter, Georgia, serif;
  -webkit-print-color-adjust: exact; print-color-adjust: exact;
}
header, section, footer { max-width: 190mm; margin: 0 auto; padding: 0 12mm; }
h1 { font: 600 26pt/1.15 "Archivo", Inter, system-ui, sans-serif; margin: .2em 0 .6em; letter-spacing: -.01em; }
h2 { font: 600 13pt/1.3 "Archivo", Inter, system-ui, sans-serif; margin: 2.2em 0 .7em;
     padding-bottom: .35em; border-bottom: 1.5px solid var(--ink); letter-spacing: -.005em; }
.eyebrow { font: 600 8.5pt/1 "IBM Plex Mono", ui-monospace, monospace;
           letter-spacing: .14em; text-transform: uppercase; color: var(--accent); margin-top: 18mm; }
.cover { border-bottom: 3px solid var(--ink); padding-bottom: 1.5em; margin-bottom: 1em; }
dl.meta { display: grid; grid-template-columns: max-content 1fr; gap: .25em 1.5em; margin: 0; }
dl.meta dt { font: 600 8.5pt/1.6 "IBM Plex Mono", ui-monospace, monospace;
             text-transform: uppercase; letter-spacing: .06em; color: var(--muted); }
dl.meta dd { margin: 0; }
.notes { color: var(--muted); margin-top: 1em; }
.lede { color: var(--muted); margin-top: -.3em; }

.callout { background: var(--band); border-left: 3px solid var(--flag);
           padding: .9em 1.2em; margin: 1.6em auto; max-width: 190mm; }
.callout h2 { margin: 0 0 .4em; border: 0; padding: 0; font-size: 10.5pt;
              text-transform: uppercase; letter-spacing: .08em; color: var(--flag); }
.callout p { margin: 0; }

table.grid { width: 100%; border-collapse: collapse; font-size: 9pt; margin-top: .4em; }
table.grid th, table.grid td { padding: .38em .55em; text-align: left;
                               border-bottom: .5px solid var(--rule); vertical-align: top; }
table.grid thead th { font: 600 8pt/1.3 "Archivo", Inter, system-ui, sans-serif;
                      text-transform: uppercase; letter-spacing: .05em;
                      border-bottom: 1.2px solid var(--ink); white-space: nowrap; }
table.grid tr.sub th { font-weight: 500; text-transform: none; letter-spacing: 0;
                       border-bottom: 1.2px solid var(--ink); color: var(--muted); }
th.crs { text-align: center; border-left: .5px solid var(--rule); }
th.crs .epsg { display: block; font-weight: 400; text-transform: none;
               letter-spacing: 0; color: var(--muted); font-size: 7.5pt; }
td.num, th.num { text-align: right; font-family: "IBM Plex Mono", ui-monospace, monospace;
                 font-size: 8.5pt; white-space: nowrap; font-variant-numeric: tabular-nums; }
td.id { font-family: "IBM Plex Mono", ui-monospace, monospace; font-weight: 600; }
tbody tr:nth-child(even) { background: #fafbfc; }

.overview, .snap { width: 100%; border: .5px solid var(--rule); margin: .6em 0; }
/* Two columns, so the pair of chart crops sits above the pair of sonar crops
   and each band lines up under the other. Four across a page would be too
   small to argue a classification from, which is what these are for. */
.snaps { display: grid; grid-template-columns: 1fr 1fr; gap: 4px 12px;
         align-items: start; break-inside: avoid; }
.snaps figure { min-width: 0; margin: 0; }
.snaps figcaption {
  font-size: 10px; letter-spacing: .08em; text-transform: uppercase;
  color: #667; margin-top: -.3em;
}
.sheet { break-before: page; }
dl.detail { display: grid; grid-template-columns: max-content 1fr; gap: .2em 1.4em;
            font-size: 9.5pt; margin: .8em 0; }
dl.detail dt { font: 600 8pt/1.7 "IBM Plex Mono", ui-monospace, monospace;
               text-transform: uppercase; letter-spacing: .06em; color: var(--muted); }
dl.detail dd { margin: 0; font-variant-numeric: tabular-nums; }
.note { background: var(--band); padding: .7em .9em; margin: .8em 0 0; font-size: 9.5pt; }
footer { margin-top: 3em; padding-top: .8em; border-top: .5px solid var(--rule);
         font: 8pt/1.4 "IBM Plex Mono", ui-monospace, monospace; color: var(--muted); }

@media print {
  @page { size: A4; margin: 14mm 0; }
  body { font-size: 9.5pt; }
  header, section, footer { max-width: none; }
  .page { break-inside: avoid; }
  thead { display: table-header-group; }
  tr { break-inside: avoid; }
}

/* ---- summary and legend ---- */
table.summary { max-width: 100%; }
table.summary th {
  width: 34%; text-align: left; font-weight: 500; color: #566;
  background: #f7f9fa;
}
table.summary td { font-variant-numeric: tabular-nums; }
.two-up { display: flex; gap: 16px; align-items: flex-start; margin: 12px 0; }
.two-up > table { flex: 1; min-width: 0; }
/* One frequency band's chart, kept whole on the page: a legend that prints
   away from the picture it describes is worse than no legend. */
.band { margin: 18px 0 0; break-inside: avoid; }
.band h3 {
  font-size: 13px; margin: 0 0 1px; padding-top: 8px;
  border-top: .5px solid var(--rule);
}
.band > .lede { margin: 0 0 6px; }
.legend { margin: 14px 0 4px; break-inside: avoid; }
.legend h3 {
  font-size: 11px; letter-spacing: .08em; text-transform: uppercase;
  color: #667; margin: 0 0 2px;
}
.legend ul { list-style: none; margin: 6px 0 0; padding: 0; }
.legend li {
  display: flex; align-items: center; gap: 10px;
  padding: 3px 0; border-bottom: 1px solid #eef1f3; font-size: 11.5px;
}
.legend .sw {
  width: 26px; height: 12px; border-radius: 2px; flex: none;
  border: 1px solid #ccd3d8; background: #888;
}
.legend .sw.ramp { width: 46px; }
.legend .sw.line { height: 4px; border: none; border-radius: 2px; }
.legend .lg-name { flex: 0 0 auto; font-weight: 500; }
.legend .lg-kind {
  font-family: ui-monospace, monospace; font-size: 10px; color: #889;
  border: 1px solid #dde3e7; border-radius: 3px; padding: 0 4px;
}
.legend .lg-detail { color: #778; flex: 1; text-align: right; }
"#;
