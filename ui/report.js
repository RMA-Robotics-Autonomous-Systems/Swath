// What the report is made of, without the DOM.
//
// A chart in a deliverable is not "whatever happened to be ticked on screen
// when someone pressed the button". The layer tree is a working view -- an
// operator turns the DTM on to check a depth and leaves it on -- and a report
// built from it puts a 400 km2 bathymetry grid behind a 300 m survey line and
// calls it coverage. So the report keeps its own list, stored in the project,
// and the Report button asks before it draws anything.
//
// The outline is *derived* from the current tree every time and merged with
// what was stored, rather than read back verbatim: adding a recording or
// importing a file has to show up in the dialog, and a chart nobody has an
// opinion about yet should arrive with sensible defaults rather than empty.

import { MOSAIC, TRACK, RASTER, VECTOR, RECORDING } from './layers.js';

function khz(hz) {
  return hz > 0 ? Math.round(hz / 1000) : null;
}

/// The centre frequency a band actually transmits, kHz, or null.
///
/// `centre_hz` and not `f0`/`f1`: the JSF sweep field is a u16 in units of
/// 10 Hz and cannot hold anything above 655 kHz, so a 1550 kHz channel is
/// recorded as 239 kHz. The server puts the high bits back where the XTF
/// written beside it can corroborate them, and leaves this at the recorded
/// value where nothing can.
function centre(b) {
  return b ? khz(b.centre_hz || (b.f0 + b.f1) / 2) : null;
}

/// What to call a frequency band.
///
/// An operator says "the high frequency channel", not "subsystem 20". With two
/// bands the names are unambiguous and worth using; with one or three they are
/// not, so the frequency itself becomes the title.
export function bandName(bands, sub) {
  const b = bands.find((x) => x.subsystem === sub);
  const c = centre(b);
  const sweep = b ? khz(Math.abs(b.f1 - b.f0)) : null;
  const known = bands.filter((x) => centre(x));
  let title;
  if (known.length === 2 && c) {
    const top = Math.max(...known.map((x) => centre(x)));
    title = c === top ? 'High frequency' : 'Low frequency';
  } else {
    title = c ? `${c} kHz` : `Channel ss${sub}`;
  }
  const parts = [];
  if (c && title !== `${c} kHz`) parts.push(`${c} kHz`);
  if (sweep) parts.push(`${sweep} kHz sweep`);
  parts.push(`ss${sub}`);
  return { title, subtitle: parts.join(' · ') };
}

/// Must match `safe_name` on the Rust side, which is what the report looks for.
export function safeName(s) {
  return String(s).replace(/[^A-Za-z0-9_-]/g, '_');
}

export function overviewId(sub) { return safeName(`ov-${sub}`); }
export function datasetChartId(dataset, sub) { return safeName(`ds-${dataset}-${sub}`); }

/// The bands a set of recordings covers, merged, in subsystem order.
export function bandsOf(names, loaded) {
  const out = new Map();
  for (const n of names) {
    const sum = loaded.get(n);
    if (!sum) continue;
    // `bands` is new; a summary from an older server still has `subsystems`.
    const bs = sum.bands
      || (sum.subsystems || []).map((s) => ({ subsystem: s, f0: 0, f1: 0, centre_hz: 0 }));
    for (const b of bs) {
      // Prefer whichever recording could tell us the real frequency: one
      // without an XTF beside it leaves the centre at the wrapped value.
      const have = out.get(b.subsystem);
      if (!have || (!(have.centre_hz > 0) && b.centre_hz > 0)) out.set(b.subsystem, { ...b });
    }
  }
  return [...out.values()].sort((a, b) => a.subsystem - b.subsystem);
}

/// The layers a chart could draw, in tree order so the legend and the picture
/// agree, each with what it should default to.
///
/// `imported` files default to off: they are the reason this dialog exists.
/// A bathymetry grid imported to check depths is not what a coverage chart is
/// about, and it is the layer most likely to bury the survey it sits under.
function availableFor(layers, kind, dataset, sub, names) {
  const out = [];
  for (const l of layers) {
    if (l.kind === RECORDING) continue;
    if (l.kind === MOSAIC || l.kind === TRACK) {
      // A mosaic belongs to a band; a track does not. There is one fish and one
      // boat per recording, and every band was towed by them, so a track goes
      // into the frame of whichever band the frame is about.
      if (l.kind === MOSAIC && l.subsystem !== sub) continue;
      if (kind === 'dataset' ? l.dataset !== dataset : !names.includes(l.dataset)) continue;
      // A track layer *is* one line now, so which line it draws is the layer's
      // to say, not the chart's. What the chart still chooses is whether to
      // draw it: the overview answers "where did we go", and the boat's line is
      // the one with a GPS behind it; a recording's own section is about the
      // imagery, and there the fish's line is what the imagery was shot from.
      const boat = l.kind === TRACK ? !!l.show_boat : true;
      const fish = l.kind === TRACK ? l.show_fish !== false : kind === 'dataset';
      out.push({
        id: l.id,
        on: l.kind !== TRACK || (kind === 'dataset' ? fish : boat),
        boat,
        fish,
      });
    } else if (l.kind === RASTER || l.kind === VECTOR) {
      out.push({ id: l.id, on: false, boat: false, fish: true });
    }
  }
  return out;
}

/// Derive the report outline from the project as it is now, keeping every
/// choice already made about a chart or layer that still exists.
export function buildReportSpec(project, loaded, layers) {
  const stored = project.report || {};
  const prev = new Map((stored.charts || []).map((c) => [c.id, c]));

  // A recording has to be in the project and actually loaded: the viewer draws
  // these charts, and it cannot draw a recording it has not read.
  const names = (project.datasets || [])
    .filter((d) => d.enabled !== false && loaded.has(d.name))
    .map((d) => d.name);
  const bands = bandsOf(names, loaded);

  const charts = [];
  const add = (spec) => {
    const old = prev.get(spec.id);
    const was = new Map((old?.layers || []).map((l) => [l.id, l]));
    charts.push({
      ...spec,
      enabled: old ? old.enabled !== false : true,
      layers: spec.layers.map((a) => {
        const o = was.get(a.id);
        return o
          ? { id: a.id, on: o.on !== false, boat: !!o.boat, fish: o.fish !== false }
          : a;
      }),
    });
  };

  for (const b of bands) {
    const n = bandName(bands, b.subsystem);
    add({
      id: overviewId(b.subsystem),
      kind: 'overview',
      title: `Coverage — ${n.title}`,
      subtitle: n.subtitle,
      dataset: '',
      subsystem: b.subsystem,
      layers: availableFor(layers, 'overview', '', b.subsystem, names),
    });
  }

  for (const name of names) {
    const own = bandsOf([name], loaded);
    for (const b of own) {
      // Named from the project's bands, not the recording's, so a recording
      // that only ran one channel still says "High frequency" rather than
      // renaming it because it is alone.
      const n = bandName(bands.length ? bands : own, b.subsystem);
      add({
        id: datasetChartId(name, b.subsystem),
        kind: 'dataset',
        title: n.title,
        subtitle: n.subtitle,
        dataset: name,
        subsystem: b.subsystem,
        layers: availableFor(layers, 'dataset', name, b.subsystem, names),
      });
    }
  }

  return {
    basemap: stored.basemap || 'osm',
    seamark: stored.seamark !== false,
    contacts: stored.contacts !== false,
    charts,
  };
}

/// The frame a chart should be drawn into.
///
/// Shaped like its own data. A fixed landscape frame letterboxes anything that
/// is not 3:2 -- a north-south run of line drew two grey bands and a ribbon --
/// and the wasted axis is wasted on the page too. Clamped, because a single
/// straight line has an aspect ratio of about a hundred.
export function chartSize(bounds, long = 1600) {
  let a = 1;
  if (bounds) {
    const y = (lat) => Math.log(Math.tan(Math.PI / 4 + (lat * Math.PI) / 360));
    const dx = ((bounds.max_lon - bounds.min_lon) * Math.PI) / 180;
    const dy = y(bounds.max_lat) - y(bounds.min_lat);
    if (dx > 0 && dy > 0) a = dx / dy;
  }
  a = Math.min(Math.max(a, 0.5), 2);
  return a >= 1
    ? [long, Math.max(1, Math.round(long / a))]
    : [Math.max(1, Math.round(long * a)), long];
}
