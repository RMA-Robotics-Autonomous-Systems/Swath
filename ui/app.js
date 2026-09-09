// Wiring: what the two views show, what the panels list, and what a click does.

import { api, tileUrl } from './api.js';
import { MapView } from './map.js';
import { WaterfallView, sliderRow, verticalSliderRunsDown } from './waterfall.js';
import { installSelects } from './select.js';
import { Planner } from './plan.js';
import { haversine, offset, formatDistance, parseCoord, ddm } from './geo.js';
import {
  RECORDING, MOSAIC, TRACK, RASTER, VECTOR,
  buildTree, moveWithinSiblings, dropIndex, removeNode, drawOrder, isReady,
  isVisible, shouldAdopt, recordingId, mosaicId, trackId, normalise, migrate,
} from './layers.js';
import { buildReportSpec, chartSize, safeName } from './report.js';

const $ = (id) => document.getElementById(id);

/// Bind a handler, and say so loudly rather than throwing if the element has
/// gone.
///
/// A missing id used to take the whole module down at load time, which shows up
/// as an application that draws its static HTML and does nothing else -- the
/// hardest kind of failure to read from a screenshot. A panel that has been
/// rearranged should cost one control, not all of them.
function on(id, event, fn) {
  const el = $(id);
  if (!el) { console.warn(`no element #${id} to bind ${event} to`); return null; }
  el.addEventListener(event, fn);
  return el;
}

// Track colours, assigned in load order. Chosen to stay distinguishable on the
// dark chart and against the amber of a contact pin.
const PALETTE = ['#35b8a6', '#e8792f', '#8f7ae0', '#4fa3e8', '#d94f70', '#7cc45a'];

const S = {
  // The planner replaces the waterfall in the right-hand pane rather than
  // opening a window of its own; see plan.js.
  planning: false,
  project: null,
  datasets: [],          // from disk
  loaded: new Map(),     // name -> summary
  contacts: [],
  selected: null,
  crsList: [],
  tool: 'pan',
  measureFrom: null,
  pendingMark: null,
  /// Chart and waterfall follow each other. Which one is driving is tracked so
  /// the two do not chase each other round in a circle.
  link: true,
  linkFrom: null,
  /// The project tree, flat and in draw order, first on top.
  layers: [],
  /// GeoJSON for vector layers, by layer id.
  features: new Map(),
  /// Fetched track geometry, by layer id.
  tracks: new Map(),
  ramps: [],
  collapsed: new Set(),
  /// Mosaic layer ids with a build running, so the row can say so.
  building: new Set(),
};

/// Image rows per server render.
///
/// Half a second of work at 1024 px wide, which is about four screens at true
/// scale. Small enough that the first one arrives quickly, large enough that a
/// steady read crosses a boundary about once a minute -- and by then the next
/// block has long since been fetched.
const BLOCK_ROWS = 2048;

let map;

/// The waterfall panes, left to right, and the one everything single-valued
/// follows. `wf` is that pane's view: the scrollbar, the chart link, the scale
/// readout and the contact overlay all speak for one channel, and this is it.
let panes = [];
let wf = null;

/// The channel everything single-valued means: which recording the chart link
/// follows, and what a contact marked from the chart belongs to. Derived from
/// the panes rather than stored, because the panes are derived from the tree
/// and a second copy of "which channel" is exactly what went stale before.
function leadPane() {
  return panes.find(p => p.view === wf) || panes[0] || null;
}

/// Panes past this are narrower than a swath is worth. Ticking a sixth mosaic
/// still paints it on the chart; it just does not get a column here.
const MAX_PANES = 4;

// ---- status ----------------------------------------------------------------

let msgTimer = null;
function msg(text, kind = '') {
  const el = $('msg');
  el.textContent = text;
  el.className = kind;
  clearTimeout(msgTimer);
  if (text) msgTimer = setTimeout(() => { el.textContent = ''; }, 6000);
}
function busy(text) { msg(text, 'warn'); }

// ---- boot ------------------------------------------------------------------

async function boot() {
  map = new MapView($('map'), {
    onhover: onMapHover,
    onclick: onMapClick,
    onmove: onMapMove,
  });
  installSelects();

  new ResizeObserver(() => map.resize()).observe($('map-wrap'));
  // Resizing changes how many rows fit, so more blocks may be needed -- but
  // the ones already held are still valid, which is the point of rendering
  // fixed blocks rather than exactly one paneful.
  let wfResizeTimer = null;
  new ResizeObserver(() => {
    for (const p of panes) p.view.resize();
    syncScrollbar();
    clearTimeout(wfResizeTimer);
    wfResizeTimer = setTimeout(ensureBlocks, 120);
  }).observe($('wf-wrap'));

  bindChrome();
  bindSplitter();
  bindKeys();

  await refresh();
  await restore();
  msg('ready');
}

/// Reopen whatever was open last time.
///
/// The server holds no session across restarts, so the last project name lives
/// in the browser's own storage and the datasets come from the project file.
/// Opening the app should put the work back on screen, not an empty chart.
async function restore() {
  try {
    if (!S.project) {
      const want = localStorage.getItem('swath.project');
      const known = [...$('project').options].some(o => o.value === want);
      if (want && known) {
        S.project = await api.openProject(want);
        $('project').value = want;
        adoptLayers();
        await loadContacts();
      }
    }
    const wanted = (S.project?.datasets || []).map(d => d.name);
    for (const n of wanted) {
      if (S.datasets.some(d => d.name === n)) {
        await loadDataset(n, { fit: false, waterfall: false });
      }
    }
    await loadWaterfall(true);
    await syncLayers();
    if (S.loaded.size) {
      const bs = [...S.loaded.values()].map(s => s.bounds).filter(Boolean);
      if (bs.length) map.fit(bs.reduce((a, x) => ({
        min_lat: Math.min(a.min_lat, x.min_lat), min_lon: Math.min(a.min_lon, x.min_lon),
        max_lat: Math.max(a.max_lat, x.max_lat), max_lon: Math.max(a.max_lon, x.max_lon),
      })));
    }
  } catch (e) {
    msg(`could not restore the last session: ${e.message}`, 'warn');
  }
}

/// Take the open project's layer stack as the working one.
///
/// Mosaic entries whose recording is not loaded are kept, not dropped: the
/// recordings load a moment later and the stack has to come back in the order
/// it was left, not in load order.
function adoptLayers() {
  S.layers = migrate(S.project?.layers || []);
  S.features.clear();
  S.tracks.clear();
}

async function refresh() {
  const st = await api.state();
  S.datasets = st.datasets;
  const sel = $('project');
  sel.innerHTML = '<option value="">— no project —</option>' +
    st.projects.map(p => `<option value="${esc(p)}">${esc(p)}</option>`).join('');
  if (st.project) {
    sel.value = st.project.name;
    localStorage.setItem('swath.project', st.project.name);
    // Only take the server's copy when it is a *different* project. For the
    // one already open the browser is the authority: it holds edits that have
    // not been written yet, and adopting over them discards them.
    if (shouldAdopt(S.project, st.project)) {
      S.project = st.project;
      adoptLayers();
    }
    await loadContacts();
  }
  if (!S.ramps.length) {
    try { S.ramps = (await api.ramps()).ramps; } catch { S.ramps = []; }
  }
  // A dataset the server already has loaded should show as loaded, whether it
  // was this window that asked for it or not.
  for (const sum of st.loaded || []) S.loaded.set(sum.name, sum);
  // Not just the tree: `adoptLayers` replaces the stack the chart is drawing
  // from, and leaving the old one in `map.layers` meant a switched-away project
  // went on asking for tiles of a recording that had just been unloaded.
  await syncLayers();
}

/// Recordings on disk that this project does not already cover.
function availableDatasets() {
  const have = new Set((S.project?.datasets || []).map(d => d.name));
  return S.datasets.filter(d => !have.has(d.name));
}

// ---- the project tree ------------------------------------------------------
//
// One list holds recordings, their mosaics and tracks, and imported files. The
// order is the draw order and the tree is the panel; the model itself lives in
// layers.js, where it can be tested without a window.

/// Everything the chart draws, from the tree.
/// A tree node with whatever the chart needs to draw it: the digest its tiles
/// are keyed on, the track geometry, the parsed features. Null when that has
/// not arrived yet.
function fillLayer(l) {
  if (l.kind === MOSAIC) {
    const sum = S.loaded.get(l.dataset);
    if (!sum) return null;
    return { ...l, rev: sum.mosaic_rev?.[String(l.subsystem)] || '' };
  }
  if (l.kind === TRACK) {
    // Keyed by recording, not by layer: the fish line and the boat line are two
    // readings of one solve, so they are fetched once.
    const st = S.tracks.get(l.dataset);
    return st ? { ...l, track: st } : null;
  }
  if (l.kind === VECTOR) return { ...l, features: S.features.get(l.id) || null };
  return { ...l };
}

async function syncLayers() {
  const { tiles, vectors } = drawOrder(S.layers);
  map.layers = tiles.map(fillLayer).filter(Boolean);
  map.vectors = vectors.map(fillLayer).filter(Boolean);
  map.draw();
  renderTree();
  // The waterfall is a view of the same layer list, so it comes along: this is
  // the one call that makes ticking a mosaic change what the sonar pane shows.
  syncPanes();
  await fetchMissing();
}

/// Is this layer shown? Ancestors included -- unticking a recording takes its
/// mosaics and tracks with it, so nothing should be fetched, bounded, repainted
/// or sampled for a layer inside a hidden one.
function shown(l) {
  return isVisible(S.layers, l);
}

/// Fetch what the visible layers need but do not have yet.
async function fetchMissing() {
  for (const l of S.layers) {
    if (!shown(l)) continue;
    if (l.kind === VECTOR && !S.features.has(l.id)) {
      S.features.set(l.id, null);                 // claim it, so one fetch only
      try {
        S.features.set(l.id, await api.layerFeatures(l.id));
        await syncLayers();
      } catch (e) {
        msg(`${l.label}: ${e.message}`, 'error');
      }
    }
    if (l.kind === TRACK && !S.tracks.has(l.dataset) && S.loaded.has(l.dataset)) {
      S.tracks.set(l.dataset, null);
      try {
        S.tracks.set(l.dataset, (await api.track(l.dataset)).track);
        await syncLayers();
      } catch (e) {
        msg(`${l.label}: ${e.message}`, 'error');
      }
    }
  }
}

/// Build the tree nodes a recording contributes, if they are not there already.
function ensureRecordingNodes(name, sum) {
  const rid = recordingId(name);
  const d = datasetRef(name);
  if (!S.layers.some(l => l.id === rid)) {
    S.layers.unshift({
      id: rid, kind: RECORDING, parent: '', dataset: name,
      label: d.label || name, visible: true,
      colour: d.colour || PALETTE[countRecordings() % PALETTE.length],
    });
  }
  const rec = S.layers.find(l => l.id === rid);
  if (!rec.colour) rec.colour = PALETTE[countRecordings() % PALETTE.length];
  d.colour = rec.colour;

  // One mosaic per channel, off by default: painting one is a minute of work
  // and should be asked for.
  const after = [];
  for (const sub of sum.subsystems) {
    const mid = mosaicId(name, sub);
    if (!S.layers.some(l => l.id === mid)) {
      after.push({
        id: mid, kind: MOSAIC, parent: rid, dataset: name, subsystem: sub,
        label: `ss${sub} mosaic`, visible: false, opacity: 1,
        style: { ramp: 'grey', reverse: false, lo: 0, hi: 255 },
      });
    }
  }
  // One fish and one boat, because that is how many there were. Every band was
  // towed by the same fish and every mosaic is painted from its fixes, so a
  // track per channel was the same line drawn twice.
  for (const which of ['fish', 'boat']) {
    const tid = trackId(name, which);
    if (S.layers.some(l => l.id === tid)) continue;
    after.push({
      id: tid, kind: TRACK, parent: rid, dataset: name, subsystem: null,
      label: `${which} track`, visible: which === 'fish', opacity: 1,
      colour: rec.colour, show_boat: which === 'boat', show_fish: which === 'fish',
    });
  }
  if (after.length) {
    const at = S.layers.findIndex(l => l.id === rid);
    S.layers.splice(at + 1, 0, ...after);
  }
  // An older project can have this recording's mosaics sitting at the top
  // level; the migration adopts them, and this puts them back beside their
  // siblings.
  S.layers = normalise(S.layers);
}

function countRecordings() {
  return S.layers.filter(l => l.kind === RECORDING).length;
}

/// The project's record of a dataset, created if this is the first sight of it.
function datasetRef(name) {
  if (!S.project) return { name, nav: defaultNav() };
  S.project.datasets = S.project.datasets || [];
  let d = S.project.datasets.find(x => x.name === name);
  if (!d) {
    d = { name, label: name, enabled: true, colour: '',
          added: new Date().toISOString(), nav: defaultNav(),
          sound_speed_m_s: C_RECORDED };
    S.project.datasets.push(d);
  }
  if (!d.nav) d.nav = defaultNav();
  if (!d.mosaic) d.mosaic = {};
  if (!(d.sound_speed_m_s > 0)) d.sound_speed_m_s = C_RECORDED;
  return d;
}

/// The speed of sound the topside was configured with when it wrote these
/// recordings. Not a guess at the water -- see `C_RECORDED` on the Rust side.
export const C_RECORDED = 1500;

/// The mosaic settings this recording carries, as a partial the server
/// completes with its own defaults.
///
/// Unlike the water-column setting this replaced, the speed of sound is not a
/// choice about how to draw: it is a property of the water that day, it was
/// measured, and getting it wrong scales every across-track distance.
function mosaicOpts(d) {
  // Everything the Mosaic panel writes lives under `d.mosaic`, which is a
  // partial MosaicConfig and round-trips through the project file as one. It
  // used to be spread flat over the dataset entry, where the server's
  // DatasetRef had nowhere to put it and serde dropped the lot on every save:
  // the panel would apply a setting, the project would come back without it,
  // and the control would spring back to its default.
  const m = d.mosaic || {};
  const o = { sound_speed_m_s: d.sound_speed_m_s || C_RECORDED };
  for (const k of ['tvg', 'agc', 'angular_gain', 'despeckle', 'gamma',
                   'exponent', 'table', 'axis', 'base_zoom', 'across',
                   'clip_lo', 'clip_hi', 'drop_flagged']) {
    if (m[k] !== undefined) o[k] = m[k];
  }
  if (m.db !== undefined) o.db = !!m.db;
  if (m.max_range_m) o.max_range_m = m.max_range_m;
  if (m.nadir_blank_m) o.nadir_blank_m = m.nadir_blank_m;
  return o;
}

function defaultNav() {
  return {
    gps_to_towpoint_m: 4.0, layback_m: 44.0, model: 'astern', bearing: 'cog',
    compass_smooth_s: 2.0, heading_offset_deg: 0.0,
    cog_baseline_s: 12.0, roll_flag_deg: 11.0,
  };
}

/// Write the tree and the per-recording navigation back into the project.
let saveViewTimer = null;
async function writeView() {
  if (!S.project) return;
  S.project.layers = S.layers;
  // A recording in the tree is a recording in the project, and therefore in
  // the report. Unticking it in the project dialog is how you keep it on the
  // chart but out of the deliverable.
  for (const l of S.layers) {
    if (l.kind !== RECORDING) continue;
    const d = datasetRef(l.dataset);
    d.label = l.label;
    d.colour = l.colour;
  }
  try {
    const saved = await api.saveProject(S.project);
    saved.layers = S.layers;         // keep our own object identity
    saved.datasets = S.project.datasets;
    S.project = saved;
  } catch { /* not fatal */ }
}

function saveView() {
  if (!S.project) return;
  clearTimeout(saveViewTimer);
  saveViewTimer = setTimeout(writeView, 600);
}

/// Write the pending save out now and wait for it.
///
/// Anything that re-reads the project from the server has to come through here
/// first. The debounce means the browser holds the only copy of a change for up
/// to six hundred milliseconds, and a refresh in that window replaces it with
/// the server's older copy -- which is how a new project lost the recordings
/// that had just been ticked into it.
async function flushView() {
  if (!saveViewTimer) return;
  clearTimeout(saveViewTimer);
  saveViewTimer = null;
  await writeView();
}

// ---- rendering the tree ----------------------------------------------------

const KIND_BADGE = {
  [RECORDING]: '', [MOSAIC]: 'sonar', [TRACK]: 'track',
  [RASTER]: 'grid', [VECTOR]: 'gpx',
};

function renderTree() {
  const ul = $('tree');
  ul.innerHTML = '';
  $('tree-hint').style.display = S.layers.length > 1 ? '' : 'none';
  if (!S.layers.length) {
    ul.innerHTML = '<li class="none">Nothing yet. Add a recording, or import a grid or a GPX.</li>';
    return;
  }
  for (const node of buildTree(S.layers)) {
    ul.appendChild(treeRow(node.layer, false));
    if (S.collapsed.has(node.layer.id)) continue;
    for (const c of node.children) ul.appendChild(treeRow(c, true));
  }
  bindTreeDrag(ul);
}

function treeRow(l, isChild) {
  const li = document.createElement('li');
  li.className = `t-row${isChild ? ' t-child' : ''} t-${l.kind}`;
  li.dataset.id = l.id;
  const ready = isReady(l, S.loaded);
  if (!ready) li.classList.add('waiting');
  // Its own box is still ticked -- that is its own setting, and unticking the
  // parent should not silently rewrite it -- but nothing here is on screen.
  if (l.visible !== false && !shown(l)) li.classList.add('t-hidden');

  const twist = l.kind === RECORDING
    ? `<span class="t-twist">${S.collapsed.has(l.id) ? '▸' : '▾'}</span>`
    : '<span class="t-twist"></span>';
  const swatch = l.colour
    ? `<span class="t-swatch" style="background:${esc(l.colour)}"></span>`
    : '';
  const label = S.building.has(l.id) ? 'building…'
    : !ready ? 'not loaded'
    : KIND_BADGE[l.kind];
  const badge = label ? `<span class="t-kind">${label}</span>` : '';
  if (S.building.has(l.id)) li.classList.add('waiting');
  li.innerHTML = `
    <span class="t-grip" title="Drag to reorder">⠿</span>
    ${twist}
    <input type="checkbox" ${l.visible !== false ? 'checked' : ''} title="Show or hide">
    ${swatch}
    <span class="t-name" title="${esc(l.info || l.file || l.dataset || '')}">${esc(l.label || l.id)}</span>
    ${badge}
    <button class="t-gear mini" title="Settings">⚙</button>`;

  li.querySelector('input').addEventListener('change', async (e) => {
    e.stopPropagation();
    await setVisible(l, e.target.checked);
  });
  li.querySelector('.t-gear').addEventListener('click', (e) => {
    e.stopPropagation();
    openLayerDialog(l.id);
  });
  if (l.kind === RECORDING) {
    li.querySelector('.t-twist').addEventListener('click', (e) => {
      e.stopPropagation();
      S.collapsed.has(l.id) ? S.collapsed.delete(l.id) : S.collapsed.add(l.id);
      renderTree();
    });
  }
  return li;
}

/// The checkbox means one thing: show it, or do not.
///
/// It used to also mean "and block for a minute while the mosaic is built, then
/// untick yourself if that failed", which makes a checkbox that argues with the
/// person clicking it. Painting a mosaic still has to happen, so it happens
/// behind the tick: the box goes on and stays on, the row says `building…`, and
/// a failure is reported rather than silently undone.
async function setVisible(l, on) {
  l.visible = on;
  await syncLayers();
  saveView();
  if (on && l.kind === MOSAIC && !S.building.has(l.id)) buildMosaic(l);
}

/// Paint a mosaic, without holding anything up.
async function buildMosaic(l) {
  S.building.add(l.id);
  renderTree();
  try {
    const m = await api.buildMosaic(l.dataset, l.subsystem);
    msg(`${l.label}: ${m.size[0]}×${m.size[1]} px from ${fmtInt(m.pings)} pings`);
    // The recording's summary carries the digest the tile URLs are keyed on,
    // and building it is exactly what changes that.
    try {
      const st = await api.state();
      const sum = (st.loaded || []).find(x => x.name === l.dataset);
      if (sum) S.loaded.set(l.dataset, sum);
    } catch { /* the tiles will still come, just from the old URL */ }
  } catch (e) {
    msg(`${l.label}: ${e.message}`, 'error');
  } finally {
    S.building.delete(l.id);
    await syncLayers();
  }
}

// ---- reordering ------------------------------------------------------------

/// The rows a drag may land between: a node's own siblings, as drawn.
function siblingRows(ul, id) {
  const me = S.layers.find(l => l.id === id);
  if (!me) return [];
  const parent = me.parent || '';
  return [...ul.querySelectorAll('li.t-row')].filter(li => {
    const l = S.layers.find(x => x.id === li.dataset.id);
    return l && (l.parent || '') === parent;
  });
}

/// Drag to reorder, tracked on `window`.
///
/// Pointer capture on the row is not reliable in the shell's webview -- the
/// drag stops receiving moves the moment the pointer leaves the element -- so
/// the gesture listens globally for its duration. A drag only starts once the
/// pointer has moved far enough that it cannot have been meant as a click.
function bindTreeDrag(ul) {
  const THRESHOLD = 4;
  ul.querySelectorAll('li.t-row').forEach(li => {
    li.addEventListener('pointerdown', (e) => {
      if (e.button !== 0) return;
      if (e.target.tagName === 'INPUT' || e.target.tagName === 'BUTTON') return;
      if (e.target.classList.contains('t-twist')) return;
      const startY = e.clientY;
      const id = li.dataset.id;
      let mids = null, rows = null, at = null, dragging = false;

      const clear = () => ul.querySelectorAll('li').forEach(x =>
        x.classList.remove('dragging', 'over-top', 'over-bottom'));

      const onMove = (ev) => {
        if (!dragging) {
          if (Math.abs(ev.clientY - startY) < THRESHOLD) return;
          dragging = true;
          li.classList.add('dragging');
          rows = siblingRows(ul, id);
          mids = rows.map(r => {
            const b = r.getBoundingClientRect();
            return b.top + b.height / 2;
          });
        }
        at = dropIndex(mids, ev.clientY);
        rows.forEach((r, i) => {
          r.classList.toggle('over-top', i === at);
          r.classList.toggle('over-bottom', at === rows.length && i === rows.length - 1);
        });
      };
      const onUp = () => {
        for (const ev of ['pointermove', 'mousemove']) window.removeEventListener(ev, onMove);
        for (const ev of ['pointerup', 'mouseup', 'pointercancel', 'blur']) {
          window.removeEventListener(ev, onUp);
        }
        clear();
        if (!dragging || at == null) return;
        const r = moveWithinSiblings(S.layers, id, at);
        if (!r.moved) return;
        S.layers = r.list;
        syncLayers();
        saveView();
      };
      window.addEventListener('pointermove', onMove);
      window.addEventListener('mousemove', onMove);
      for (const ev of ['pointerup', 'mouseup', 'pointercancel', 'blur']) {
        window.addEventListener(ev, onUp);
      }
    });
  });
}

// ---- recordings ------------------------------------------------------------

async function loadDataset(name, opts = {}) {
  busy(`loading ${name}…`);
  try {
    const d = datasetRef(name);
    const sum = await api.loadDataset(name, d.nav, mosaicOpts(d));
    S.loaded.set(name, sum);
    ensureRecordingNodes(name, sum);
    // The fixes moved, so anything derived from them is stale.
    S.tracks.delete(name);
    await syncLayers();
    saveView();
    if (opts.fit !== false && S.loaded.size === 1 && sum.bounds) map.fit(sum.bounds);
    if (opts.waterfall !== false) await loadWaterfall(true);
    msg(`${name}: ${fmtInt(sum.pings)} pings`);
    return sum;
  } catch (e) {
    msg(`${name}: ${e.message}`, 'error');
    renderTree();
    return null;
  }
}

async function unloadDataset(name) {
  await api.unloadDataset(name);
  S.loaded.delete(name);
  S.layers = removeNode(S.layers, recordingId(name));
  S.tracks.delete(name);
  if (S.project) {
    S.project.datasets = (S.project.datasets || []).filter(d => d.name !== name);
  }
  if (panes.some(p => p.dataset === name)) await loadWaterfall(true);
  await syncLayers();
  saveView();
}

/// Re-solve one recording and bring everything derived from it along.
///
/// The layback moves the fish, and the fish is where the imagery is painted, so
/// a change of navigation invalidates the track, the waterfall's row positions
/// and the mosaic alike. The mosaic is the one that used to be missed.
async function applyNav(name) {
  const sum = await loadDataset(name, { fit: false, waterfall: false });
  if (!sum) return;
  const mosaics = S.layers.filter(l =>
    l.kind === MOSAIC && l.dataset === name && shown(l));
  for (let i = 0; i < mosaics.length; i++) {
    busy(`repainting ${mosaics[i].label} (${i + 1}/${mosaics.length})…`);
    try {
      await api.buildMosaic(name, mosaics[i].subsystem);
    } catch (e) {
      msg(`${mosaics[i].label}: ${e.message}`, 'error');
    }
  }
  await syncLayers();
  map.tiles.clear();                 // drop imagery painted with the old fixes
  map.draw();
  if (panes.some(p => p.dataset === name)) await loadWaterfall(false);
  msg(mosaics.length
    ? `${name}: reprocessed, ${mosaics.length} mosaic${mosaics.length > 1 ? 's' : ''} repainted`
    : `${name}: reprocessed`);
}

// ---- importing -------------------------------------------------------------

let browseAt = null;
let chosen = null;

async function openImport() {
  if (!S.project) { msg('open or create a project first', 'error'); return; }
  chosen = null;
  $('im-import').disabled = true;
  $('im-chosen').textContent = '';
  await browseTo(localStorage.getItem('swath.browse') || '');
  $('import-dialog').returnValue = '';
  $('import-dialog').showModal();
}

async function browseTo(path) {
  const list = $('im-list');
  list.innerHTML = '<li class="none">reading…</li>';
  try {
    const r = await api.browse(path);
    browseAt = r.path;
    $('im-path').value = r.path;
    localStorage.setItem('swath.browse', r.path);
    list.innerHTML = '';
    if (r.parent) {
      list.appendChild(browseRow('↑ ' + r.parent, 'dir', () => browseTo(r.parent)));
    }
    for (const d of r.dirs) {
      list.appendChild(browseRow(d.name + '/', 'dir', () => browseTo(d.path)));
    }
    for (const f of r.files) {
      const el = browseRow(`${f.name}`, f.kind, () => {
        chosen = f;
        $('im-chosen').textContent = `${f.path} · ${fmtBytes(f.size)}`;
        $('im-import').disabled = false;
        list.querySelectorAll('li').forEach(x => x.classList.remove('sel'));
        el.classList.add('sel');
      }, fmtBytes(f.size));
      list.appendChild(el);
    }
    if (!r.dirs.length && !r.files.length) {
      list.innerHTML = '<li class="none">Nothing importable here.</li>';
    }
  } catch (e) {
    list.innerHTML = `<li class="none">${esc(e.message)}</li>`;
  }
}

function browseRow(text, kind, onclick, right = '') {
  const li = document.createElement('li');
  li.className = `br br-${kind}`;
  li.innerHTML = `<span class="br-name">${esc(text)}</span><span class="br-right">${esc(right)}</span>`;
  li.addEventListener('click', onclick);
  return li;
}

on('import-dialog', 'close', async () => {
  if ($('import-dialog').returnValue !== 'import' || !chosen) return;
  const f = chosen;
  busy(`importing ${f.name}… a large grid takes a few seconds`);
  try {
    const { layer } = await api.importLayer(f.path, '');
    layer.parent = '';
    S.layers = S.layers.filter(l => l.id !== layer.id);
    S.layers.unshift(layer);
    S.features.delete(layer.id);
    await syncLayers();
    saveView();
    if (layer.bounds) map.fit(layer.bounds);
    msg(`${layer.label}: ${layer.info}`);
  } catch (e) {
    msg(`import failed: ${e.message}`, 'error');
  }
});

// ---- waterfall -------------------------------------------------------------
//
// One pane per visible mosaic layer, in the order the chart draws them. The
// waterfall used to be a channel picked from a dropdown of its own, which meant
// the chart and the waterfall could be looking at different channels of the
// same recording with nothing on screen saying so -- and switching the mosaic
// you were reading did not switch the sonar you were reading it from. The tree
// is the control now: tick a mosaic and its channel is here, in the colours
// that mosaic is painted in.

/// Set while `loadWaterfall` is rebuilding, so `syncPanes` leaves the fetching
/// to it.
let restarting = false;

/// The mosaic layers a pane should exist for, left to right in draw order.
function wantedChannels() {
  return S.layers
    .filter(l => l.kind === MOSAIC && shown(l) && S.loaded.has(l.dataset))
    .slice(0, MAX_PANES);
}

/// The colour scheme as the block image's query.
///
/// Empty for plain grey, which is what the server already has encoded, so the
/// common case costs nothing and hits the browser cache under the same URL the
/// block was first fetched with.
function rampQuery(st) {
  const ramp = st?.ramp || 'grey';
  const lo = st?.lo ?? 0, hi = st?.hi ?? 255;
  if (ramp === 'grey' && !st?.reverse && lo === 0 && hi === 255) return '';
  return `?ramp=${encodeURIComponent(ramp)}&reverse=${st.reverse ? 1 : 0}&lo=${lo}&hi=${hi}`;
}

/// Build a pane, or return the one already showing this layer.
///
/// Reuse matters: ticking a second mosaic must not throw away the blocks the
/// first one has already fetched, and toggling one off and on again should not
/// refetch the recording.
function makePane(layer) {
  const root = document.createElement('div');
  root.className = 'wf-pane';
  const head = document.createElement('div');
  head.className = 'wf-pane-head';
  head.innerHTML = '<span class="ramp"></span><span class="nm"></span><span class="who"></span>';
  const canvas = document.createElement('canvas');
  root.append(head, canvas);

  const pane = {
    id: layer.id,
    layer,
    dataset: layer.dataset,
    subsystem: layer.subsystem,
    root, canvas,
    ramp: rampQuery(layer.style),
    inFlight: new Set(),
    view: null,
  };
  pane.view = new WaterfallView(canvas, {
    onhover: (h) => onWfHover(h, pane),
    onclick: (h, e) => onWfClick(h, e, pane),
    onscroll: () => onWfScroll(pane),
  });
  // A pane that appears mid-session takes the settings the others are already
  // on, not the class defaults, or it would be drawn at a different scale from
  // the pane beside it.
  pane.view.stride = parseInt($('wf-stride').value, 10) || 1;
  pane.view.aspectMode = $('wf-aspect').value;
  pane.view.tool = S.tool;
  pane.view.contacts = S.contacts;
  pane.view.totalPings = S.loaded.get(pane.dataset)?.pings || 0;
  // Whichever pane is being used is the one the chart link and the readouts
  // speak for. Capture, so it lands before the view's own drag handling.
  root.addEventListener('pointerdown', () => setLead(pane), true);
  return pane;
}

function setLead(pane) {
  if (wf === pane.view) return;
  wf = pane.view;
  for (const p of panes) p.root.classList.toggle('lead', p === pane);
  updateScaleReadout();
  syncScrollbar();
}

/// Bring the panes into line with the tree. Called from `syncLayers`, so every
/// tick, drag and colour change goes through it.
function syncPanes() {
  const want = wantedChannels();
  const by = new Map(panes.map(p => [p.id, p]));
  const next = [];
  for (const l of want) {
    const p = by.get(l.id) || makePane(l);
    by.delete(l.id);
    p.layer = l;
    next.push(p);
  }
  for (const gone of by.values()) gone.root.remove();

  const box = $('wf-panes');
  const changed = next.length !== panes.length || next.some((p, i) => p !== panes[i]);
  panes = next;
  if (changed) {
    for (const p of panes) box.appendChild(p.root);   // appending reorders
  }

  // The lead pane has to be one that still exists.
  if (!panes.some(p => p.view === wf)) wf = panes[0]?.view || null;
  for (const p of panes) p.root.classList.toggle('lead', p.view === wf);

  for (const p of panes) {
    p.view.contacts = S.contacts;
    p.view.totalPings = p.view.totalPings || S.loaded.get(p.dataset)?.pings || 0;
    describePane(p);
    // A recoloured mosaic means recoloured sonar: the pixels are the same, but
    // the images the browser holds are not, so they have to come again.
    const ramp = rampQuery(p.layer.style);
    if (ramp !== p.ramp) {
      p.ramp = ramp;
      p.view.clear();
      p.inFlight.clear();
      p.view.totalPings = S.loaded.get(p.dataset)?.pings || 0;
    }
    p.view.resize();
  }
  if (changed) alignPanes();
  lockPaneScales();

  // Ticking a fifth mosaic still paints it on the chart, so say why it is not
  // here rather than letting it look like it failed.
  const over = S.layers.filter(l => l.kind === MOSAIC && shown(l) && S.loaded.has(l.dataset))
    .length - panes.length;
  $('wf-more').textContent = over > 0 ? `+${over} not shown` : '';

  const loaded = S.loaded.size > 0;
  $('wf-empty').textContent = !loaded
    ? 'Load a recording to see the waterfall.'
    : 'No mosaic layer is shown. Tick one in the tree to read its channel here.';
  $('wf-empty').style.display = panes.length ? 'none' : 'grid';
  // Not while restarting: the caller is about to clear every pane, and fetching
  // blocks it is going to throw away is the one thing worth avoiding here.
  if (!restarting) ensureBlocks();
  syncScrollbar();
  updateScaleReadout();
}

/// Say what a pane is, and in what colours.
function describePane(p) {
  const rec = S.layers.find(l => l.id === p.layer.parent);
  p.root.querySelector('.nm').textContent = p.layer.label || `ss${p.subsystem}`;
  p.root.querySelector('.who').textContent =
    panes.some(x => x.dataset !== p.dataset) ? (rec?.label || p.dataset) : '';
  const r = S.ramps.find(x => x.name === (p.layer.style?.ramp || 'grey'));
  const stops = r ? (p.layer.style?.reverse ? [...r.stops].reverse() : r.stops) : null;
  p.root.querySelector('.ramp').style.background =
    stops ? `linear-gradient(to right, ${stops.join(',')})` : '';
}

/// Panes over one recording draw at one across-track scale: the widest band's.
///
/// At true scale the vertical scale follows the across-track one, so bands with
/// different swath widths -- 50 m and 25 m here -- put different numbers of
/// rows on screen and slide apart the moment either is scrolled. Sharing the
/// scale locks the rows together; the narrower band is then drawn narrower,
/// which is what it is.
function lockPaneScales() {
  const widest = new Map();
  for (const p of panes) {
    const m = p.view.metresPerPxAcross();
    if (m) widest.set(p.dataset, Math.max(widest.get(p.dataset) || 0, m));
  }
  let changed = false;
  for (const p of panes) {
    const ref = widest.get(p.dataset) || null;
    if (p.view.acrossRef === ref) continue;
    p.view.acrossRef = ref;
    changed = true;
  }
  if (!changed) return false;
  // The row height just moved, so what was on screen has to be clamped again
  // and the panes brought back level.
  for (const p of panes) p.view.top = p.view.clampTop(p.view.top);
  alignPanes();
  for (const p of panes) p.view.draw();
  return true;
}

/// Put every pane where the newly-scrolled one is.
///
/// Every band of a recording is on the recording's own rows -- row N is the
/// same transmit cycle in all of them, by construction, see
/// `waterfall::pair_bands` -- so within a recording the row itself is the
/// answer. Two recordings share no row space at all, so those are matched
/// through time, taken from row metadata already in hand rather than from the
/// server.
function alignPanes(from = null) {
  const lead = from || panes.find(p => p.view === wf);
  if (!lead) return;
  for (const p of panes) {
    if (p === lead) continue;
    let top = lead.view.top;
    if (p.dataset !== lead.dataset || p.view.stride !== lead.view.stride) {
      const t = lead.view.timeAt(lead.view.top);
      const r = t == null ? null : p.view.rowAtTime(t);
      if (r != null) top = r;
    }
    p.view.scrollTo(top, { silent: true });
  }
}

/// The request for one block, so the key the server computes is stable.
function blockRequest(pane, start) {
  return {
    dataset: pane.dataset,
    subsystem: pane.subsystem,
    start,
    count: BLOCK_ROWS * pane.view.stride,
    width: 1024,
    stride: pane.view.stride,
    axis: $('wf-axis').value,
    tvg: 0.7, gamma: 0.8, clip_lo: 1, clip_hi: 99,
    max_range_m: null,
  };
}

/// Fetch one block and hand it to its pane.
///
/// The metadata and the image come separately: the JSON is parsed on the main
/// thread and the PNG is decoded off it, which is the whole reason the server
/// stopped inlining the image as a data URI.
async function fetchBlock(pane, start) {
  if (pane.inFlight.has(start) || pane.view.hasBlock(start)) return;
  pane.inFlight.add(start);
  const stamp = () => `${pane.id}|${pane.view.stride}|${$('wf-axis').value}|${pane.ramp}`;
  const want = stamp();
  try {
    const meta = await api.waterfall(blockRequest(pane, start));
    const img = new Image();
    img.src = meta.png + pane.ramp;
    await img.decode();
    // The operator may have changed scale or colours while this was in the air.
    // Dropping it is cheaper than showing the wrong picture.
    if (stamp() !== want || !panes.includes(pane)) return;
    meta.image = img;
    pane.view.addBlock(meta);
    // The swath width is only known once a block has landed, so the scale the
    // panes share can only be settled here.
    lockPaneScales();
    if (pane.view === wf) {
      syncScrollbar();
      updateScaleReadout();
      // The span is drawn from row metadata, so it can only be right once the
      // rows are here. Without this, jumping the chart somewhere new left the
      // highlight off until the next scroll.
      showViewSpan();
    }
  } catch (e) {
    if (!/past the end/.test(e.message)) msg(`waterfall: ${e.message}`, 'error');
  } finally {
    pane.inFlight.delete(start);
  }
}

/// Keep blocks loaded around the visible window, and drop the far ones.
///
/// This is the buffer. The window is padded by a screen either side before
/// rounding out to whole blocks, so a steady scroll always has the next block
/// in hand before it is needed and a reversal does not have to refetch what
/// was just read.
function ensureBlocks() {
  // Shared across the panes, not per pane: six connections is what the browser
  // gives an origin, and four panes each asking for three would leave the one
  // being looked at queued behind the ones that are not.
  let budget = 4 - panes.reduce((n, p) => n + p.inFlight.size, 0);
  // The lead pane first, so what is under the cursor arrives before the rest.
  const order = [...panes].sort((a, b) => (b.view === wf) - (a.view === wf));
  for (const p of order) budget = ensurePaneBlocks(p, budget);
}

function ensurePaneBlocks(pane, budget) {
  const view = pane.view;
  const step = BLOCK_ROWS * view.stride;
  const vis = view.visibleRows();
  const pad = Math.max(vis, BLOCK_ROWS / 2);
  const lo = Math.max(0, view.top - pad);
  const hi = view.top + vis + pad;
  const total = view.totalPings || Infinity;

  const first = Math.floor(lo / BLOCK_ROWS);
  const last = Math.floor(hi / BLOCK_ROWS);
  // Nearest to the middle of the view first, so a jump renders what is being
  // looked at before it renders the margin around it.
  const centre = view.centreRow() / BLOCK_ROWS;
  const wanted = [];
  for (let i = first; i <= last; i++) {
    if (i * step >= total) continue;
    wanted.push(i);
  }
  wanted.sort((a, b) => Math.abs(a + 0.5 - centre) - Math.abs(b + 0.5 - centre));

  view.dropBlocksOutside((first - 1) * step, (last + 2) * step);
  for (const i of wanted) {
    if (budget <= 0) break;
    if (pane.inFlight.has(i * step) || view.hasBlock(i * step)) continue;
    budget--;
    fetchBlock(pane, i * step);
  }
  return budget;
}

/// True when this engine draws a vertical slider's minimum at the top, which
/// is where a scrollbar's belongs. Measured once; see `verticalSliderRunsDown`.
const SLIDER_DOWN = verticalSliderRunsDown();
if (!SLIDER_DOWN) document.documentElement.classList.add('slider-up');

function syncScrollbar() {
  const sl = $('wf-scroll');
  if (!wf) { sl.disabled = true; return; }
  const max = Math.max(1, Math.round(Math.max(0, wf.totalRows() - wf.visibleRows())));
  sl.max = max;
  sl.value = sliderRow(Math.round(wf.top), max, SLIDER_DOWN);
  sl.disabled = !wf.ready;
}

function updateScaleReadout() {
  const along = wf?.metresPerRow(), across = wf?.metresPerCanvasPx();
  $('wf-scale').textContent = along && across
    ? `${across.toFixed(2)} m/px across · ${along.toFixed(2)} m/ping along`
    : '';
}

/// Point the chart at whatever the waterfall is showing.
///
/// Scrolling any pane scrolls them all: they are the same stretch of seabed
/// read on different channels, and letting them drift apart would make the pair
/// two pictures of roughly the same place rather than one comparison.
function onWfScroll(pane) {
  if (pane) {
    setLead(pane);
    alignPanes(pane);
  }
  syncScrollbar();
  ensureBlocks();
  lockPaneScales();
  updateScaleReadout();
  showViewSpan();
  if (!S.link || S.linkFrom === 'map' || !wf) return;
  const r = wf.rowAt(Math.floor(wf.centreRow()));
  if (!r) return;
  S.linkFrom = 'waterfall';
  map.flyTo(r.fish_lat, r.fish_lon);
  S.linkFrom = null;
}

/// Highlight the stretch of track the waterfall is currently showing, so the
/// chart says where on the line you are reading.
function showViewSpan() {
  if (!wf || !wf.ready) { map.viewSpan = null; map.schedule(); return; }
  const vis = wf.visibleRows();
  const pts = [];
  const stepRows = Math.max(1, Math.floor(vis / 120));
  for (let g = Math.floor(wf.top); g < wf.top + vis; g += stepRows) {
    const r = wf.rowAt(g);
    if (r) pts.push([r.fish_lat, r.fish_lon]);
  }
  map.viewSpan = pts.length > 1 ? pts : null;
  map.schedule();
}

/// Start (or restart) every pane: a new axis, stride or recording means the
/// blocks in hand were drawn from something that no longer applies.
async function loadWaterfall(reset = false) {
  const keepRow = reset ? 0 : (wf ? wf.centreRow() : 0);
  restarting = true;
  try { syncPanes(); } finally { restarting = false; }
  if (!panes.length) { syncScrollbar(); return; }
  for (const p of panes) {
    p.view.stride = parseInt($('wf-stride').value, 10) || 1;
    p.view.aspectMode = $('wf-aspect').value;
    p.view.contacts = S.contacts;
    p.view.clear();
    p.inFlight.clear();
    // Seed the row count from the summary so the scrollbar has a range before
    // the first block lands. It is refined to the channel's own count when it
    // does; a channel is a subset of the recording, not all of it.
    p.view.totalPings = S.loaded.get(p.dataset)?.pings || 0;
    p.view.top = p.view.clampTop(keepRow - p.view.visibleRows() / 2);
  }
  ensureBlocks();
  syncScrollbar();
}

function onWfHover(h, pane) {
  if (!h) { map.cursor = null; map.schedule(); $('wf-pos').textContent = ''; return; }
  const r = h.info;
  const across = h.across_m ?? 0;
  const time = `${new Date(r.time * 1000).toISOString().slice(11, 19)}Z`;

  // In slant range the band either side of nadir narrower than the altitude is
  // water, not seabed. It has no position, so the chart cursor comes off rather
  // than being pinned to the fish and read as a place on the ground.
  if (h.water) {
    map.cursor = null;
    map.schedule();
    $('wf-pos').textContent =
      `water column · ${Math.abs(h.range_m ?? 0).toFixed(1)} m slant · ` +
      `alt ${r.altitude.toFixed(1)} m · ${time}`;
    return;
  }

  const ll = offset(r.fish_lat, r.fish_lon, r.bearing + 90, across);
  map.cursor = ll;
  map.schedule();
  const slant = pane.view.axis === 'slant'
    ? ` (${Math.abs(h.range_m ?? 0).toFixed(1)} m slant)` : '';
  $('wf-pos').textContent =
    `${across >= 0 ? 'STB' : 'PRT'} ${Math.abs(across).toFixed(1)} m${slant} · ` +
    `alt ${r.altitude.toFixed(1)} m · ${time}`;
  showReadout(ll[0], ll[1], r);
}

async function onWfClick(h, ev, pane) {
  if (S.tool !== 'mark') return;
  if (h.water) { msg('that is the water column — there is no seabed there to mark', 'error'); return; }
  const r = h.info;
  const ll = offset(r.fish_lat, r.fish_lon, r.bearing + 90, h.across_m ?? 0);
  openContact({
    lat: ll[0], lon: ll[1],
    source: 'waterfall',
    // The pane that was clicked, not whichever one is leading: a contact
    // belongs to the channel it was seen on.
    dataset: pane.dataset,
    subsystem: pane.subsystem,
    time: r.time,
    depth_m: r.depth,
    altitude_m: r.altitude,
    across_m: h.across_m,
  });
  // The clicked pixel is deliberately not passed on. It is a pixel in whichever
  // band happened to be on screen, and the dialog draws both bands from blocks
  // it fetches itself, finding the pixel each time with the same `locateWorld`
  // the click came through. Handing it one of the four would make that pane the
  // odd one out.
}

// ---- map interaction -------------------------------------------------------

/// Point the waterfall at whatever the chart is showing.
///
/// Debounced, because a pan fires on every frame and the answer only matters
/// once the view settles. If the chart is nowhere near the recording the
/// waterfall is left where it is: dragging away to look at something else is
/// not a request to lose your place.
let mapLinkTimer = null;
function onMapMove() {
  showReadoutForCentre();
  const lead = leadPane();
  if (!S.link || S.linkFrom === 'waterfall' || !lead) return;
  clearTimeout(mapLinkTimer);
  mapLinkTimer = setTimeout(async () => {
    try {
      const r = await api.nearest(lead.dataset, lead.subsystem, map.centre[0], map.centre[1]);
      // Half the chart's width: near enough that the pings are what is on
      // screen, rather than the closest pass on the far side of the survey.
      const reach = Math.max(200, map.metresPerPixel() * map.w * 0.5);
      if (r.distance_m > reach) return;
      S.linkFrom = 'map';
      lead.view.centreOn(r.row / lead.view.stride);
      alignPanes(lead);
      S.linkFrom = null;
      ensureBlocks();
      syncScrollbar();
      showViewSpan();
    } catch { /* the link is a convenience; a failed lookup should not shout */ }
  }, 160);
}

/// Keep the grid readout live while panning, not only while hovering.
let centreReadoutTimer = null;
function showReadoutForCentre() {
  clearTimeout(centreReadoutTimer);
  centreReadoutTimer = setTimeout(() => showReadout(map.centre[0], map.centre[1]), 250);
}

/// The pointer moves at whatever rate the device reports -- a hundred and
/// twenty times a second on this hardware -- and every one of those used to
/// become a request for the grid readout. Cheap requests, but they were queued
/// on the same six connections the tiles need, and a readout from sixty
/// positions ago is of no interest to anyone. The line under the cursor is
/// drawn immediately; only the lookup waits.
let hoverTimer = null;
let hoverAt = null;
function onMapHover(ll) {
  hoverAt = ll;
  if (!hoverTimer) {
    hoverTimer = setTimeout(() => {
      hoverTimer = null;
      if (hoverAt) showReadout(hoverAt[0], hoverAt[1]);
    }, 60);
  }
  if (S.tool === 'measure' && S.measureFrom) {
    map.measure = { from: S.measureFrom, to: ll };
    map.schedule();
  }
}

async function onMapClick(ll, ev) {
  const hit = map.hit(ll[0], ll[1]);
  if (hit && S.tool !== 'measure') {
    selectContact(hit.id);
    if (ev.detail === 2 || ev.altKey) openContact(hit);
    return;
  }
  if (S.tool === 'measure') {
    if (!S.measureFrom) {
      S.measureFrom = ll;
      map.measure = { from: ll, to: ll };
    } else {
      const d = haversine(S.measureFrom, ll);
      msg(`${formatDistance(d)} on ${bearingOf(S.measureFrom, ll).toFixed(1)}°`);
      S.measureFrom = null;
    }
    map.draw();
    return;
  }
  if (S.tool === 'mark') {
    openContact({ lat: ll[0], lon: ll[1], source: 'map',
                  dataset: leadPane()?.dataset || '' }, { kind: 'map' });
  }
}

// ---- readout ---------------------------------------------------------------

let readoutSeq = 0;
async function showReadout(lat, lon, row) {
  $('readout').textContent =
    `${fmtDM(lat, true)}  ${fmtDM(lon, false)}` +
    (row ? `   depth ${(row.depth + row.altitude).toFixed(1)} m` : '');
  const seq = ++readoutSeq;
  try {
    const r = await api.convert(lat, lon);
    if (seq !== readoutSeq) return;   // a newer hover already answered
    S.crsList = r.systems;
    const grid = r.systems.find(s => s.unit === 'm');
    $('crs-readout').textContent = grid
      ? `${grid.name}  ${grid.x}  ${grid.y}`
      : '';
  } catch { /* readout is cosmetic; a failed convert should not shout */ }
  sampleLayer(lat, lon, seq);
}

/// What the topmost visible grid says is under the cursor.
///
/// This is why an imported grid is stored as values rather than as a picture:
/// a multibeam DTM under the pointer can answer "how deep is it there", and the
/// answer is the file's own number, not a colour read backwards off a ramp.
let sampleTimer = null;
function sampleLayer(lat, lon, seq) {
  const l = S.layers.find(x => x.kind === RASTER && shown(x));
  const el = $('layer-readout');
  if (!l) { el.textContent = ''; return; }
  clearTimeout(sampleTimer);
  sampleTimer = setTimeout(async () => {
    try {
      const r = await api.layerSample(l.id, lat, lon);
      if (seq !== readoutSeq) return;
      el.textContent = r.value == null
        ? `${l.label}  —`
        : `${l.label}  ${r.value.toFixed(2)}`;
    } catch { el.textContent = ''; }
  }, 90);
}

// ---- contacts --------------------------------------------------------------

async function loadContacts() {
  const r = await api.contacts();
  S.contacts = r.contacts || [];
  map.contacts = S.contacts;
  renderContacts();
  map.draw();
  for (const p of panes) { p.view.contacts = S.contacts; p.view.draw(); }
  // The plan's box is built from the datums, so a datum added, moved or
  // deleted is a different plan.
  planContactsChanged();
}

function renderContacts() {
  $('contact-count').textContent = S.contacts.length;
  const ul = $('contacts');
  ul.innerHTML = '';
  for (const c of S.contacts) {
    const li = document.createElement('li');
    if (S.selected === c.id) li.classList.add('sel');
    li.innerHTML = `<span class="c-id">${esc(c.id)}</span>
      <span class="c-name">${esc(c.name || c.class || '—')}</span>
      <span class="c-dot" style="background:${esc(c.colour || '#f0b429')}"></span>`;
    li.addEventListener('click', () => { selectContact(c.id); map.flyTo(c.lat, c.lon); });
    li.addEventListener('dblclick', () => openContact(c));
    ul.appendChild(li);
  }
}

/// Paint a stored snapshot into a canvas. False when there is not one.
async function drawStoredSnap(canvas, existing, kind) {
  if (!existing?.id || !existing[SNAP_FIELD[kind]]) return false;
  const q = kind ? `kind=${encodeURIComponent(kind)}&` : '';
  return new Promise((res) => {
    const img = new Image();
    img.onload = () => {
      canvas.getContext('2d').drawImage(img, 0, 0, canvas.width, canvas.height);
      canvas.parentElement.classList.remove('empty');
      res(true);
    };
    img.onerror = () => res(false);
    img.src = `/api/snap/${encodeURIComponent(existing.id)}?${q}t=${Date.now()}`;
  });
}

function selectContact(id) {
  S.selected = id;
  map.selected = id;
  renderContacts();
  map.draw();
}

/// Say what was understood, before it is saved. A transposed digit in a datum
/// is a day at sea in the wrong place.
function checkContactPos() {
  const lat = parseCoord($('cd-lat').value, true);
  const lon = parseCoord($('cd-lon').value, false);
  const el = $('cd-parsed');
  const ok = Number.isFinite(lat) && Number.isFinite(lon);
  el.classList.toggle('bad', !ok);
  if (!ok) {
    el.textContent = (!$('cd-lat').value && !$('cd-lon').value)
      ? 'Degrees and decimal minutes as the plotter shows them, or decimal degrees.'
      : `Cannot read the ${!Number.isFinite(lat) && !Number.isFinite(lon) ? 'position'
          : !Number.isFinite(lat) ? 'latitude' : 'longitude'}.`
        + ' Try N5125.2300 and E00309.3400, or 51.4205 and 3.1556.';
    return;
  }
  el.textContent = `${lat.toFixed(6)}, ${lon.toFixed(6)}`;
}


async function openContact(seed) {
  if (!S.project) { msg('open or create a project first', 'error'); return; }
  const dlg = $('contact-dialog');
  const existing = seed.id ? S.contacts.find(c => c.id === seed.id) : null;
  const c = existing ? { ...existing } : {
    id: '', name: '', class: '', confidence: 'medium', status: 'new',
    colour: '#f0b429', shape: 'point', ...seed,
  };
  S.pendingMark = c;

  $('cd-title').textContent = existing ? `Contact ${c.id}` : 'New contact';
  $('cd-name').value = c.name || '';
  $('cd-class').value = c.class || '';
  $('cd-confidence').value = c.confidence || 'medium';
  $('cd-status').value = c.status || 'new';
  $('cd-length').value = c.length_m ?? '';
  $('cd-width').value = c.width_m ?? '';
  $('cd-note').value = c.note || '';
  $('cd-delete').style.display = existing ? '' : 'none';

  // Coordinates in every system the area suggests: this is the whole point of
  // the report requirement, and seeing it at mark time catches a wrong zone
  // before it reaches a deliverable.
  // A datum is typed, not clicked, so the position is editable here. Anything
  // marked on the imagery keeps its fields too -- a mark placed one screen-pixel
  // out is easier to nudge as a number than to re-click.
  $('cd-lat').value = ddm(c.lat, 'NS');
  $('cd-lon').value = ddm(c.lon, 'EW');
  $('cd-radius').value = c.radius_m ?? '';
  $('cd-datum').checked = (c.source || 'map') === 'datum';
  checkContactPos();

  $('cd-coords').innerHTML = 'resolving…';
  api.convert(c.lat, c.lon).then(r => {
    // The uncertainty belongs beside the coordinate, not in a footnote. Six
    // decimal places of latitude next to nothing at all reads as a survey
    // position; it is a picture placed by a modelled layback.
    const sum = S.loaded.get(leadPane()?.dataset) || [...S.loaded.values()][0];
    const sp = sum && sum.position_spread_m;
    $('cd-coords').innerHTML = [
      `<b>WGS 84</b> ${r.dm[0]}, ${r.dm[1]}`,
      ...r.systems.filter(s => s.unit === 'm')
        .map(s => `<b>${esc(s.name)}</b> ${s.x}, ${s.y}`),
      sp && sp[1] > 0
        ? `<span class="warn">± ${sp[0].toFixed(0)} m typical, ${sp[1].toFixed(0)} m `
          + 'in turns — the layback models disagree by this much</span>'
        : '',
    ].filter(Boolean).join('<br>');
  }).catch(() => { $('cd-coords').textContent = ''; });

  // Four crops: the seabed at each band and the return at each band. Stored
  // ones are shown as they are; anything missing is drawn now, which is what
  // makes a brand new mark carry a full sheet the moment it is saved.
  //
  // The dialog opens first and the pictures land in it, rather than the other
  // way round: two of these need a round trip to the server for a waterfall
  // block, and a dialog that waits for all four before appearing reads as a
  // click that did nothing.
  S.snapTaken = {};
  for (const [, el] of SNAP_PANES) {
    const cv = $(el);
    cv.getContext('2d').clearRect(0, 0, cv.width, cv.height);
    cv.parentElement.classList.remove('empty');
  }
  dlg.returnValue = '';
  dlg.showModal();

  for (const [kind, el] of SNAP_PANES) {
    const cv = $(el);
    // The operator may have closed it, or opened another contact, while a
    // block was in the air.
    if (S.pendingMark !== c) return;
    let drawn = await drawStoredSnap(cv, existing, kind);
    if (!drawn) {
      try {
        drawn = await drawBandSnap(cv, kind, c);
      } catch {
        drawn = false;
      }
    }
    if (S.pendingMark !== c) return;
    S.snapTaken[kind] = drawn;
    cv.parentElement.classList.toggle('empty', !drawn);
  }
}


/// All four crops for one contact, drawn off screen and stored.
///
/// Off screen because these are not what the operator is looking at: each is
/// its own channel alone, with nothing blended over it and no sea underneath,
/// which is the picture the report argues from.
async function putBandSnaps(id, c) {
  const took = [];
  for (const [kind] of SNAP_PANES) {
    const cv = document.createElement('canvas');
    cv.width = cv.height = 360;
    try {
      if (!(await drawBandSnap(cv, kind, c))) continue;
      const blob = await new Promise(res => cv.toBlob(res, 'image/png'));
      if (blob) {
        await api.putSnapshot(id, new Uint8Array(await blob.arrayBuffer()), kind);
        took.push(kind);
      }
    } catch { /* a band that cannot be drawn is simply not in the sheet */ }
  }
  return took;
}

/// All four crops for every contact, taken again as the report is built.
///
/// `putBandSnaps` runs when a contact is saved from its dialog, which leaves
/// the report two ways short. A contact marked before the band crops existed
/// has none at all. One whose mosaic has been repainted since -- a different
/// gain, a different axis -- has a pair that no longer matches the imagery the
/// rest of the report is drawn from. The sheets are the part a classification
/// is argued from, so they are taken here rather than trusted.
///
/// Both bands have to be painted before either can be cropped, and the report
/// otherwise builds only the mosaics its own charts asked for -- so a channel
/// that is in no chart would silently have no crop.
async function refreshContactBands() {
  if (!S.contacts.length) return;
  const wanted = new Map();
  for (const c of S.contacts) {
    if (!c.dataset || wanted.has(c.dataset)) continue;
    wanted.set(c.dataset, S.loaded.has(c.dataset) ? bandLayers(c.dataset) : []);
  }
  let painted = false;
  for (const [ds, bands] of wanted) {
    for (const b of bands) {
      busy(`painting ${ds} ${b.kind === 'lf' ? 'low' : 'high'} frequency…`);
      try {
        await api.buildMosaic(ds, b.layer.subsystem);
        painted = true;
      } catch (e) {
        msg(`${ds} ${b.kind}: ${e.message}`, 'error');
      }
    }
  }
  // Painting changes the digest the tiles are keyed on, so a crop taken against
  // the old state comes off the old raster.
  if (painted) {
    try {
      const st = await api.state();
      for (const sum of st.loaded || []) {
        if (S.loaded.has(sum.name)) S.loaded.set(sum.name, sum);
      }
    } catch { /* the old URLs still resolve */ }
  }

  const short = [];
  for (let i = 0; i < S.contacts.length; i++) {
    const c = S.contacts[i];
    busy(`contact sheets (${i + 1}/${S.contacts.length})…`);
    const took = await putBandSnaps(c.id, c);
    if (took.length < SNAP_PANES.length) {
      short.push(`${c.id} (${took.join(', ') || 'none'})`);
    }
  }
  if (short.length) {
    msg(`incomplete contact sheet for ${short.join('; ')} — that recording needs `
        + 'both channels loaded and mosaicked', 'error');
  }
}

/// The recording's two channels, lowest first.
///
/// Named by frequency rather than by subsystem number, because "ss20" means
/// nothing to whoever reads the report and "low frequency" means exactly the
/// thing the picture is showing. Empty when the recording has only one band:
/// there is no comparison to draw.
function bands(dataset) {
  const sum = S.loaded.get(dataset);
  if (!sum) return [];
  const centre = (sub) => sum.bands?.find(b => b.subsystem === sub)?.centre_hz ?? sub;
  const subs = [...(sum.subsystems || [])].sort((a, b) => centre(a) - centre(b));
  if (subs.length < 2) return [];
  return [{ kind: 'lf', subsystem: subs[0] },
          { kind: 'hf', subsystem: subs[subs.length - 1] }];
}

/// Those channels with the mosaic layer that draws each, for the chart crops.
///
/// A band whose mosaic is not in the tree has no chart crop. The sonar crops do
/// not come through here: they are read from the recording itself, so they work
/// on a band whose mosaic has never been painted.
function bandLayers(dataset) {
  return bands(dataset)
    .map((b) => {
      const l = S.layers.find(x => x.kind === MOSAIC &&
                                   x.dataset === dataset && x.subsystem === b.subsystem);
      const layer = l && fillLayer(l);
      return layer ? { ...b, layer } : null;
    })
    .filter(Boolean);
}

/// The four crops a contact carries, and where each is drawn in the dialog.
const SNAP_PANES = [
  ['lf', 'cd-canvas-lf'],
  ['hf', 'cd-canvas-hf'],
  ['wf-lf', 'cd-canvas-wf-lf'],
  ['wf-hf', 'cd-canvas-wf-hf'],
];

/// Which field on the contact holds each kind.
const SNAP_FIELD = {
  '': 'snapshot', map: 'snapshot', waterfall: 'snapshot_wf',
  lf: 'snapshot_lf', hf: 'snapshot_hf',
  'wf-lf': 'snapshot_wf_lf', 'wf-hf': 'snapshot_wf_hf',
};

/// One channel's waterfall around a contact, rendered off screen.
///
/// The view on screen holds one subsystem, so a crop of the other band cannot
/// come from it. This asks the server for a block around the ping the contact
/// was seen on and drives a detached `WaterfallView` over it -- detached rather
/// than open-coded, so the arithmetic that finds the pixel is the same
/// arithmetic the on-screen view uses and the two cannot drift apart.
///
/// `time` is what says which ping, and it matters more than it looks. A
/// contact sits up to a swath off the track, so every row for tens of metres
/// either side is nearly the same distance from it and a search on position
/// alone picks among them at random -- and on a recording that loops over
/// itself it can land on a different pass entirely, which is a crop of the
/// right seabed from the wrong look. The two subsystems ping together, so the
/// time that was recorded when the mark was made resolves the same instant in
/// either band.
async function bandWaterfall(dataset, subsystem, lat, lon, time) {
  const near = await api.nearest(dataset, subsystem, lat, lon, time);
  const row = near?.row;
  if (!(row >= 0)) return null;
  const count = 512;
  const meta = await api.waterfall({
    dataset, subsystem,
    start: Math.max(0, Math.round(row) - count / 2),
    count, width: 1024, stride: 1,
    axis: $('wf-axis').value,
    tvg: 0.7, gamma: 0.8, clip_lo: 1, clip_hi: 99, max_range_m: null,
  });
  const img = new Image();
  // The crop wears the colours this channel is read in, so the report shows
  // what the operator was looking at rather than a second opinion in grey.
  const layer = S.layers.find(l =>
    l.kind === MOSAIC && l.dataset === dataset && l.subsystem === subsystem);
  img.src = meta.png + rampQuery(layer?.style);
  await img.decode();
  meta.image = img;
  const view = new WaterfallView(document.createElement('canvas'), {});
  view.stride = 1;
  view.addBlock(meta);
  return view;
}

/// Draw one of the four crops into a canvas, from scratch.
///
/// Returns false when this band cannot be drawn at all -- no mosaic painted for
/// a chart crop, no ping with the contact abeam for a sonar one -- so the
/// caller can leave the pane marked empty rather than storing a black square.
async function drawBandSnap(canvas, kind, c) {
  const band = kind.replace('wf-', '');
  if (kind.startsWith('wf-')) {
    const b = bands(c.dataset).find(x => x.kind === band);
    if (!b) return false;
    const view = await bandWaterfall(c.dataset, b.subsystem, c.lat, c.lon, c.time);
    const at = view && view.locateWorld(c.lat, c.lon);
    if (!at) return false;
    view.crop(canvas, at[0], at[1], 260);
    return true;
  }
  const b = bandLayers(c.dataset).find(x => x.kind === band);
  if (!b) return false;
  // One channel alone, with nothing blended over it and no sea underneath:
  // the picture the report argues from.
  await map.snapshot(canvas, c.lat, c.lon, 60, { layers: [b.layer], basemap: false });
  return true;
}

on('contact-dialog', 'close', async (e) => {
  const dlg = $('contact-dialog');
  const c = S.pendingMark;
  S.pendingMark = null;
  if (!c) return;
  if (dlg.returnValue === 'delete') {
    if (c.id) { await api.deleteContact(c.id); await loadContacts(); msg(`${c.id} deleted`); }
    return;
  }
  if (dlg.returnValue !== 'save') return;

  c.name = $('cd-name').value.trim();
  c.class = $('cd-class').value.trim();
  c.confidence = $('cd-confidence').value;
  c.status = $('cd-status').value;
  c.note = $('cd-note').value;
  const L = parseFloat($('cd-length').value), W = parseFloat($('cd-width').value);
  c.length_m = Number.isFinite(L) ? L : null;
  c.width_m = Number.isFinite(W) ? W : null;
  const lat = parseCoord($('cd-lat').value, true), lon = parseCoord($('cd-lon').value, false);
  if (Number.isFinite(lat) && Number.isFinite(lon)) { c.lat = lat; c.lon = lon; }
  const R = parseFloat($('cd-radius').value);
  c.radius_m = Number.isFinite(R) ? R : 0;
  // A datum is a position given to us rather than found, and saying so is what
  // keeps a re-solved navigation from moving it -- and what puts it in front of
  // the planner.
  c.source = $('cd-datum').checked ? 'datum'
           : (c.source === 'datum' ? 'map' : (c.source || 'map'));
  if (c.source === 'datum' && c.shape === 'point' && c.radius_m > 0) c.shape = 'circle';

  try {
    const r = await api.saveContact(c);
    const id = r.contact.id;
    // Whatever is on screen, which is what the operator just looked at and
    // approved. A pane that could not be drawn stores nothing, so an earlier
    // crop of it survives rather than being replaced by a black square.
    for (const [kind, el] of SNAP_PANES) {
      if (!S.snapTaken?.[kind]) continue;
      const blob = await new Promise(res => $(el).toBlob(res, 'image/png'));
      if (blob) await api.putSnapshot(id, new Uint8Array(await blob.arrayBuffer()), kind);
    }
    await loadContacts();
    selectContact(id);
    msg(`${id} saved`);
  } catch (err) {
    msg(`save failed: ${err.message}`, 'error');
  }
});

// ---- chrome ----------------------------------------------------------------

function bindChrome() {
  on('refresh', 'click', refresh);

  on('project', 'change', (e) => switchProject(e.target.value));

  on('new-project', 'click', () => openProjectDialog({ create: true }));
  on('manage-projects', 'click', openProjects);

  document.querySelectorAll('#tools .tool').forEach(b => {
    b.addEventListener('click', () => setTool(b.dataset.tool));
  });

  on('basemap', 'change', (e) => { map.basemap = e.target.value; map.draw(); });
  on('seamark', 'change', (e) => { map.seamark = e.target.checked; map.draw(); });

  // Axis and stride change the pixels, so the blocks have to be re-rendered.
  // Aspect only changes how they are drawn, so it does not.
  on('wf-axis', 'change', () => loadWaterfall());
  on('wf-stride', 'change', () => loadWaterfall());
  on('wf-aspect', 'change', () => {
    for (const p of panes) {
      p.view.aspectMode = $('wf-aspect').value;
      p.view.top = p.view.clampTop(p.view.top);
      p.view.draw();
    }
    syncScrollbar();
    ensureBlocks();
    updateScaleReadout();
  });

  on('wf-scroll', 'input', (e) => {
    if (!wf) return;
    const max = parseInt(e.target.max, 10) || 1;
    wf.scrollTo(sliderRow(parseInt(e.target.value, 10), max, SLIDER_DOWN));
  });

  on('link-views', 'change', (e) => {
    S.link = e.target.checked;
    if (S.link) onWfScroll();
  });

  on('add-layer', 'click', openImport);
  on('add-recording', 'click', openRecordingImport);
  on('im-go', 'click', () => browseTo($('im-path').value.trim()));
  on('pm-new', 'click', () => { $('projects-dialog').close(); openProjectDialog({ create: true }); });
  on('rec-go', 'click', () => recBrowseTo($('rec-path').value.trim()));
  on('rec-add', 'click', recAddChosen);
  on('rec-name', 'keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); recAddChosen(); }
  });
  on('rec-path', 'keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); recBrowseTo($('rec-path').value.trim()); }
  });
  on('im-path', 'keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); browseTo($('im-path').value.trim()); }
  });

  on('project-settings', 'click', () => openProjectDialog({}));

  on('open-report', 'click', openReport);
  on('open-plan', 'click', () => setPlanMode(!S.planning));
  on('pl-settings', 'click', () => {
    const d = $('plan-dialog');
    d.returnValue = '';
    d.showModal();
  });
  // A datum has no position until one is typed, so the dialog opens on the
  // middle of the chart and the operator overwrites it.
  on('new-datum', 'click', () => {
    const c = map.centre;
    openContact({
      lat: c[0], lon: c[1], source: 'datum', shape: 'circle', radius_m: 50,
      status: 'for search', class: 'datum',
    });
  });
  for (const id of ['cd-lat', 'cd-lon']) on(id, 'input', checkContactPos);
}

// ---- planning --------------------------------------------------------------

let planner = null;

/// Swap the right-hand pane between the waterfall and the planner.
///
/// They are alternatives rather than neighbours: while planning there is no
/// recording under examination, and the chart -- with the previous survey's
/// mosaic and the seamarks on it -- is exactly what the lines have to be judged
/// against, so it keeps its size.
async function setPlanMode(on) {
  S.planning = on;
  $('open-plan').classList.toggle('on', on);
  $('plan-pane').hidden = !on;
  for (const el of [document.querySelector('.wf-head'), document.querySelector('.wf-readout'),
                    $('wf-wrap')]) {
    if (el) el.hidden = on;
  }
  if (!on) {
    map.plan = null;
    map.draw();
    return;
  }
  if (!planner) {
    planner = new Planner({
      onplan: (gj) => { map.plan = gj; map.draw(); },
      onmsg: (t, kind) => msg(t, kind),
      onbounds: (b) => map.fit(b),
      onchanged: () => loadContacts(),
      onlayers: (f) => { map.planLayers = f; map.draw(); },
      // The written GPX comes back in as an ordinary vector layer, so a plan
      // on the chart is the same kind of object as an imported track and needs
      // no drawing code of its own.
      onimport: async (f) => {
        try {
          await api.importLayer(f.path, f.name);
          await refresh();
          msg(`${f.name} added to the project`);
        } catch (e) {
          msg(`could not add the plan as a layer: ${e.message}`, 'error');
        }
      },
    });
  }
  try {
    busy('solving the plan…');
    planner.contacts = S.contacts;
    await planner.load();
    const n = planner.targets.length;
    msg(n ? `searching for ${n} of ${S.contacts.length} contacts`
          : S.contacts.length ? 'Tick the contacts to search for.'
          : 'No contacts yet — “Datum…” beside Contacts takes a typed position.');
  } catch (e) {
    msg(`plan: ${e.message}`, 'error');
  }
}

/// Contacts changed under the planner: the box is built from them.
function planContactsChanged() {
  if (!planner) return;
  planner.contacts = S.contacts;
  if (S.planning) planner.load().catch(() => {});
}

// ---- the report ------------------------------------------------------------

/// Draw the report's chart views, hand them to the server, and open it.
///
/// The viewer draws them rather than the server, so the report shows the chart
/// the operator was looking at -- the same projection, the same stack, the same
/// colours. They are kept in the project, so `swath report` run later still
/// has them.
async function openReport() {
  if (!S.project) { msg('open a project first', 'error'); return; }
  openReportDialog();
}

/// Choose what the report draws, before it draws anything.
///
/// The outline is rebuilt from the project every time this opens and merged
/// with what was stored, so importing a file or adding a recording turns up
/// here, and a chart nobody has an opinion about arrives with defaults rather
/// than empty.
function openReportDialog() {
  let spec = buildReportSpec(S.project, S.loaded, S.layers);
  const dlg = $('report-dialog');
  const body = $('rd-body');
  const open = new Set();
  const byId = new Map(S.layers.map(l => [l.id, l]));

  $('rd-basemap').value = spec.basemap || 'osm';
  $('rd-seamark').checked = spec.seamark !== false;
  $('rd-contacts').checked = spec.contacts !== false;

  const chartRow = (c) => {
    const wrap = document.createElement('div');
    wrap.className = 'rd-chart' + (c.enabled === false ? ' off' : '');
    const head = document.createElement('div');
    head.className = 'rd-head';
    const twist = document.createElement('span');
    twist.className = 't-twist';
    twist.textContent = open.has(c.id) ? '▾' : '▸';
    twist.addEventListener('click', () => {
      open.has(c.id) ? open.delete(c.id) : open.add(c.id);
      draw();
    });
    head.appendChild(twist);
    head.appendChild(checkbox('', c.enabled !== false, (v) => { c.enabled = v; draw(); }));
    const name = document.createElement('span');
    name.className = 'rd-name';
    name.textContent = c.title;
    head.appendChild(name);
    const sub = document.createElement('span');
    sub.className = 'rd-sub';
    sub.textContent = c.subtitle || '';
    head.appendChild(sub);
    const n = c.layers.filter(l => l.on !== false).length;
    const count = document.createElement('span');
    count.className = 't-kind';
    count.textContent = `${n} layer${n === 1 ? '' : 's'}`;
    head.appendChild(count);
    wrap.appendChild(head);

    if (open.has(c.id)) {
      const list = document.createElement('div');
      list.className = 'rd-layers';
      if (!c.layers.length) {
        list.innerHTML = '<span class="none">nothing to draw on this one</span>';
      }
      for (const cl of c.layers) {
        const l = byId.get(cl.id);
        const row = document.createElement('div');
        row.className = 'rd-layer';
        row.appendChild(checkbox(l?.label || cl.id, cl.on !== false, (v) => {
          cl.on = v; draw();
        }));
        // A track layer used to carry both lines and a pair of checkboxes to
        // pick between them. It is one line now -- the fish's or the boat's --
        // so the tick beside its name is the whole of the choice.
        const kind = document.createElement('span');
        kind.className = 't-kind';
        kind.textContent = KIND_BADGE[l?.kind] || '?';
        row.appendChild(kind);
        list.appendChild(row);
      }
      wrap.appendChild(list);
    }
    return wrap;
  };

  const draw = () => {
    body.innerHTML = '';
    if (!spec.charts.length) {
      body.innerHTML = '<span class="none">No recordings are loaded, so there is ' +
        'nothing to chart. Load one and try again.</span>';
      return;
    }
    const section = (title, charts) => {
      if (!charts.length) return;
      const h = document.createElement('h4');
      h.textContent = title;
      body.appendChild(h);
      for (const c of charts) body.appendChild(chartRow(c));
    };
    section('Coverage', spec.charts.filter(c => c.kind === 'overview'));
    const names = [...new Set(spec.charts.filter(c => c.kind === 'dataset').map(c => c.dataset))];
    for (const n of names) {
      section(datasetRef(n).label || n, spec.charts.filter(c => c.dataset === n));
    }
  };
  draw();

  const reset = $('rd-reset');
  reset.onclick = () => {
    // Drop every stored choice, then derive again from scratch.
    spec = buildReportSpec({ ...S.project, report: null }, S.loaded, S.layers);
    $('rd-basemap').value = spec.basemap;
    $('rd-seamark').checked = spec.seamark;
    $('rd-contacts').checked = spec.contacts;
    draw();
  };

  dlg.returnValue = '';
  dlg.onclose = () => {
    if (dlg.returnValue !== 'render') return;
    spec.basemap = $('rd-basemap').value;
    spec.seamark = $('rd-seamark').checked;
    spec.contacts = $('rd-contacts').checked;
    renderReport(spec);
  };
  dlg.showModal();
}

/// Draw every chart the spec asks for, then open the report.
async function renderReport(spec) {
  const btn = $('open-report');
  btn.disabled = true;
  try {
    S.project.report = spec;
    await api.saveProject(S.project);

    const wanted = spec.charts.filter(c => c.enabled !== false);
    const byId = new Map(S.layers.map(l => [l.id, l]));

    // What the report asks for, which is *not* what the tree happens to show.
    // A mosaic switched off on screen can still be wanted on the paper, and it
    // has none of what it needs: no painted raster, no fetched geometry. Get
    // all of it first, so a chart is never quietly drawn short.
    const need = new Set();
    for (const c of wanted) {
      for (const cl of c.layers) if (cl.on !== false) need.add(cl.id);
    }
    const missing = [...need].map(id => byId.get(id)).filter(Boolean);
    for (let i = 0; i < missing.length; i++) {
      const l = missing[i];
      const step = `preparing layers (${i + 1}/${missing.length})`;
      try {
        if (l.kind === MOSAIC && S.loaded.has(l.dataset)) {
          busy(`${step}: ${l.dataset} ${l.label}…`);
          await api.buildMosaic(l.dataset, l.subsystem);
        } else if (l.kind === TRACK && !S.tracks.get(l.dataset) && S.loaded.has(l.dataset)) {
          busy(`${step}: ${l.label}…`);
          S.tracks.set(l.dataset, (await api.track(l.dataset)).track);
        } else if (l.kind === VECTOR && !S.features.get(l.id)) {
          busy(`${step}: ${l.label}…`);
          S.features.set(l.id, await api.layerFeatures(l.id));
        }
      } catch (e) {
        msg(`${l.label}: ${e.message}`, 'error');
      }
    }
    // Building a mosaic changes the digest its tiles are keyed on.
    try {
      const st = await api.state();
      for (const sum of st.loaded || []) {
        if (S.loaded.has(sum.name)) S.loaded.set(sum.name, sum);
      }
    } catch { /* the old URLs still resolve */ }

    await refreshContactBands();

    const overview = datasetBounds();
    for (let i = 0; i < wanted.length; i++) {
      const c = wanted[i];
      const bounds = c.kind === 'overview' ? overview : S.loaded.get(c.dataset)?.bounds;
      if (!bounds) continue;
      busy(`drawing ${c.title}${c.dataset ? ` — ${c.dataset}` : ''} (${i + 1}/${wanted.length})…`);

      const on = new Map(c.layers.filter(l => l.on !== false).map(l => [l.id, l]));
      // The tree's order, the report's selection.
      const { tiles, vectors } = drawOrder(S.layers, l => on.has(l.id));
      const prep = (list) => list
        .map(fillLayer)
        .filter(Boolean)
        .map(l => l.kind === TRACK
          ? { ...l, show_boat: !!on.get(l.id).boat, show_fish: on.get(l.id).fish !== false }
          : l)
        .filter(l => l.kind !== TRACK || l.show_boat || l.show_fish);

      const [w, h] = chartSize(bounds);
      const canvas = document.createElement('canvas');
      canvas.width = w;
      canvas.height = h;
      await map.renderView(canvas, bounds, {
        layers: prep(tiles),
        vectors: prep(vectors),
        contacts: c.kind === 'overview'
          ? S.contacts
          : S.contacts.filter(x => x.dataset === c.dataset),
        basemap: spec.basemap || 'osm',
        seamark: spec.seamark !== false,
      });
      await postImage(safeName(c.id), canvas);
    }
    // Written to disk, then opened. `window.open` alone worked in a browser
    // and did nothing whatsoever in the desktop shell -- a webview has no tabs,
    // so it returns null and the export looked like a button that did not work.
    // A file is also closer to what "export" ought to mean.
    const out = await api.openReport();
    msg(`report written to ${out.path}`);
    if (!out.opened) window.open('/api/report', '_blank');
  } catch (e) {
    msg(`could not build the report: ${e.message}`, 'error');
  } finally {
    btn.disabled = false;
    await syncLayers();
  }
}

async function postImage(name, canvas) {
  const blob = await new Promise(res => canvas.toBlob(res, 'image/png'));
  if (!blob) return;
  await api.reportImage(name, new Uint8Array(await blob.arrayBuffer()));
}

function union(bs) {
  const v = bs.filter(Boolean);
  if (!v.length) return null;
  return v.reduce((a, x) => ({
    min_lat: Math.min(a.min_lat, x.min_lat), min_lon: Math.min(a.min_lon, x.min_lon),
    max_lat: Math.max(a.max_lat, x.max_lat), max_lon: Math.max(a.max_lon, x.max_lon),
  }));
}

/// What the survey covers: the recordings, and nothing else.
///
/// Not the same question as "what is on screen". An imported bathymetry grid
/// can be a hundred times the size of the survey, and letting it into the
/// answer is how a coverage chart ends up showing a survey the size of a
/// thumbnail in the middle of somebody else's dataset.
function datasetBounds() {
  const names = (S.project?.datasets || [])
    .filter(d => d.enabled !== false)
    .map(d => d.name);
  return union(names.map(n => S.loaded.get(n)?.bounds));
}

/// Everything on screen, for the fit-all button -- where including an imported
/// layer is the whole point.
function allBounds() {
  const bs = [...S.loaded.values()].map(s => s.bounds);
  for (const l of S.layers) if (shown(l) && l.bounds) bs.push(l.bounds);
  return union(bs);
}

function setTool(t) {
  S.tool = t;
  for (const p of panes) p.view.tool = t;
  map.tool = t;
  S.measureFrom = null;
  map.measure = null;
  document.querySelectorAll('#tools .tool').forEach(b => b.classList.toggle('on', b.dataset.tool === t));
  $('map-wrap').classList.toggle('pan', t === 'pan');
  const hint = $('map-hint');
  const text = { mark: 'Click the chart or the waterfall to place a contact',
                 measure: 'Click two points to measure' }[t];
  hint.textContent = text || '';
  hint.classList.toggle('show', !!text);
  map.draw();
}

/// Width of the right-hand pane, from where the splitter was let go.
///
/// The floor wins over the ceiling rather than the other way round. Written as
/// `min(max(w, 260), innerWidth - 500)` the two crossed over as soon as the
/// window was narrower than 760: the ceiling came out below the floor, `min`
/// took it, and the pane was handed a negative width that took the whole
/// layout with it.
export function paneWidth(innerWidth, clientX) {
  const floor = 260;
  const ceiling = Math.max(floor, innerWidth - 500);
  return Math.min(Math.max(innerWidth - clientX, floor), ceiling);
}

function bindSplitter() {
  const sp = $('splitter');
  let dragging = false;
  // The same latch the chart had, and the same three ways out of it: nothing
  // held down, the pointer cancelled, the capture lost. Without them a release
  // the splitter never saw left it resizing the panes on every mouse move for
  // the rest of the session -- which reads, from the other side of the screen,
  // as a chart that will not stop moving.
  const stop = () => { dragging = false; };
  sp.addEventListener('pointerdown', (e) => {
    if (e.button !== 0) return;
    sp.setPointerCapture(e.pointerId);
    dragging = true;
  });
  sp.addEventListener('pointermove', (e) => {
    if (!dragging) return;
    if (e.buttons === 0) return stop();
    document.querySelector('main').style
      .setProperty('--right', `${paneWidth(window.innerWidth, e.clientX)}px`);
    map.resize();
    for (const p of panes) p.view.resize();
  });
  sp.addEventListener('pointerup', stop);
  sp.addEventListener('pointercancel', stop);
  sp.addEventListener('lostpointercapture', stop);
}

function bindKeys() {
  window.addEventListener('keydown', (e) => {
    if (e.target.matches('input, textarea, select')) return;
    if (e.key === 'v') setTool('pan');
    if (e.key === 'm') setTool('mark');
    if (e.key === 'r') setTool('measure');
    if (e.key === 'Escape') { setTool('pan'); }
    if (e.key === 'p') setPlanMode(!S.planning);
    if (e.key === 'f' && S.loaded.size) {
      const b = [...S.loaded.values()].map(s => s.bounds).filter(Boolean);
      if (b.length) map.fit(b.reduce((a, x) => ({
        min_lat: Math.min(a.min_lat, x.min_lat), min_lon: Math.min(a.min_lon, x.min_lon),
        max_lat: Math.max(a.max_lat, x.max_lat), max_lon: Math.max(a.max_lon, x.max_lon),
      })));
    }
  });
}

// ---- settings dialogs ------------------------------------------------------
//
// Every row in the tree opens the same dialog; what it contains depends on what
// the row is. Keeping the controls out of the panel is what lets the panel stay
// a tree: a mosaic has a colour scheme and a contrast stretch, a recording has
// a whole navigation solution, and neither fits in a sidebar row.

let dialogLayer = null;

function field(label, control, hint) {
  const d = document.createElement('div');
  d.className = 'ld-field';
  const s = document.createElement('span');
  s.textContent = label;
  d.appendChild(s);
  const wrap = document.createElement('div');
  wrap.className = 'ld-control';
  wrap.appendChild(control);
  if (hint) {
    const h = document.createElement('p');
    h.className = 'hint';
    h.textContent = hint;
    wrap.appendChild(h);
  }
  d.appendChild(wrap);
  return d;
}

function input(type, value, onchange, attrs = {}) {
  const i = document.createElement('input');
  i.type = type;
  if (value != null) i.value = value;
  for (const [k, v] of Object.entries(attrs)) i.setAttribute(k, v);
  i.addEventListener('change', () => onchange(type === 'number' ? parseFloat(i.value) : i.value));
  if (type === 'color') i.addEventListener('input', () => onchange(i.value));
  return i;
}

function select(options, value, onchange) {
  const sel = document.createElement('select');
  for (const [v, label] of options) {
    const o = document.createElement('option');
    o.value = v;
    o.textContent = label;
    if (v === value) o.selected = true;
    sel.appendChild(o);
  }
  sel.addEventListener('change', () => onchange(sel.value));
  return sel;
}

function slider(min, max, step, value, onchange, fmt) {
  const wrap = document.createElement('div');
  wrap.className = 'ld-slider';
  const i = document.createElement('input');
  i.type = 'range';
  i.min = min; i.max = max; i.step = step; i.value = value;
  const out = document.createElement('em');
  const show = () => { out.textContent = fmt ? fmt(+i.value) : `${Math.round(i.value * 100)}%`; };
  show();
  // `input` for the live number, `change` for the work: dragging a slider fires
  // forty times a second and each one is a tile refetch.
  i.addEventListener('input', show);
  i.addEventListener('change', () => onchange(+i.value));
  wrap.appendChild(i);
  wrap.appendChild(out);
  return wrap;
}

function rampPicker(current, onchange) {
  const wrap = document.createElement('div');
  wrap.className = 'ld-ramp';
  const sel = document.createElement('select');
  for (const r of S.ramps) {
    const o = document.createElement('option');
    o.value = r.name;
    o.textContent = r.name;
    if (r.name === current) o.selected = true;
    sel.appendChild(o);
  }
  const sw = document.createElement('span');
  sw.className = 'ld-swatch';
  const paint = () => {
    const r = S.ramps.find(x => x.name === sel.value);
    sw.style.background = r ? `linear-gradient(to right, ${r.stops.join(',')})` : '';
  };
  paint();
  sel.addEventListener('change', () => { paint(); onchange(sel.value); });
  wrap.appendChild(sel);
  wrap.appendChild(sw);
  return wrap;
}

function checkbox(label, value, onchange) {
  const l = document.createElement('label');
  l.className = 'check';
  const i = document.createElement('input');
  i.type = 'checkbox';
  i.checked = !!value;
  i.addEventListener('change', () => onchange(i.checked));
  l.appendChild(i);
  l.appendChild(document.createTextNode(' ' + label));
  return l;
}

const MODEL_NOTE = {
  astern: 'Widest of the three in a turn: the fish is placed off the arc the vessel sailed.',
  wake: 'The fish follows exactly where the vessel went. Between the other two.',
  tractrix: 'A taut cable, so the fish cuts inside the turn on radius √(R²−L²). A towed body does at least this much; real cable drag makes it lag further, so the truth sits between the wake and here.',
};

function openLayerDialog(id) {
  const l = S.layers.find(x => x.id === id);
  if (!l) return;
  dialogLayer = l;
  const dlg = $('layer-dialog');
  const body = $('ld-body');
  body.innerHTML = '';
  $('ld-title').textContent = l.label || l.id;
  $('ld-info').textContent = l.info || '';
  $('ld-remove').style.display = l.kind === TRACK || l.kind === MOSAIC ? 'none' : '';

  body.appendChild(field('Name', input('text', l.label || '', (v) => { l.label = v; })));

  if (l.kind !== RECORDING) {
    body.appendChild(field('Opacity',
      slider(0, 1, 0.05, l.opacity ?? 1, (v) => { l.opacity = v; live(); })));
  }

  if (l.kind === RECORDING) buildRecordingFields(body, l);
  if (l.kind === MOSAIC) buildMosaicFields(body, l);
  if (l.kind === RASTER) buildRasterFields(body, l);
  if (l.kind === TRACK) buildTrackFields(body, l);
  if (l.kind === VECTOR) {
    body.appendChild(field('Colour',
      input('color', l.colour || '#f0b429', (v) => { l.colour = v; live(); })));
  }

  dlg.returnValue = '';
  dlg.showModal();
}

/// Apply what is in the dialog without closing it, so a ramp or a slider can
/// be judged against the chart rather than from its name.
function live() {
  syncLayers();
}

function buildRecordingFields(body, l) {
  const d = datasetRef(l.dataset);
  const nav = d.nav;
  body.appendChild(field('Track colour',
    input('color', l.colour || '#35b8a6', (v) => {
      l.colour = v;
      for (const c of S.layers) if (c.parent === l.id && c.kind === TRACK) c.colour = v;
      live();
    })));

  const wh = document.createElement('h4');
  wh.textContent = 'Water';
  body.appendChild(wh);
  const ssNote = document.createElement('p');
  ssNote.className = 'hint';
  const sayTemp = (v) => {
    // Mackenzie at S=35, 20 m, inverted roughly -- just enough to sanity-check
    // a typed number against the season.
    const t = (v - 1448.96 - 0.33) / 4.3;
    ssNote.textContent = v === C_RECORDED
      ? 'The default is what the topside was set to, not a measurement. Every '
        + 'across-track distance is a travel time times this number, so 1500 '
        + 'against a real 1524 draws the whole swath 1.6% narrow — 0.75 m at '
        + 'the edge of a 47 m swath. Repaints the mosaic.'
      : `About ${t.toFixed(0)} °C at ordinary salinity — check that against the `
        + 'season. Repaints the mosaic and rescales the waterfall.';
  };
  body.appendChild(field('Speed of sound',
    input('number', d.sound_speed_m_s || C_RECORDED, (v) => {
      d.sound_speed_m_s = v; sayTemp(v);
    }, { step: '0.5', min: '1400', max: '1600' }), ''));
  sayTemp(d.sound_speed_m_s || C_RECORDED);
  body.appendChild(ssNote);

  const h = document.createElement('h4');
  h.textContent = 'Navigation';
  body.appendChild(h);
  const p = document.createElement('p');
  p.className = 'hint';
  p.textContent = 'Where the fish was, relative to the boat. This is what places '
    + 'the imagery, so changing it re-solves the track and repaints the mosaic.';
  body.appendChild(p);

  body.appendChild(field('Layback',
    input('number', nav.layback_m, (v) => { nav.layback_m = v; }, { step: '0.5' }),
    'metres of cable out, from the tow point to the fish'));
  body.appendChild(field('GPS → tow point',
    input('number', nav.gps_to_towpoint_m, (v) => { nav.gps_to_towpoint_m = v; }, { step: '0.5' }),
    'metres from the antenna aft to where the cable leaves the vessel'));

  const note = document.createElement('p');
  note.className = 'hint';
  note.textContent = MODEL_NOTE[nav.model] || '';
  body.appendChild(field('Layback model', select([
    ['astern', 'Constant, astern'],
    ['wake', 'Follows the wake'],
    ['tractrix', 'Tractrix (taut cable)'],
  ], nav.model, (v) => { nav.model = v; note.textContent = MODEL_NOTE[v] || ''; })));
  body.appendChild(note);

  body.appendChild(field('Swath bearing', select([
    ['cog', 'Course over ground'],
    ['compass', 'Fish compass'],
  ], nav.bearing, (v) => { nav.bearing = v; })));
  body.appendChild(field('Course baseline',
    input('number', nav.cog_baseline_s, (v) => { nav.cog_baseline_s = v; }, { step: '1' }),
    'seconds of track used to work out the heading; longer is smoother and lags more'));
  body.appendChild(field('Roll flag',
    input('number', nav.roll_flag_deg, (v) => { nav.roll_flag_deg = v; }, { step: '0.5' }),
    'degrees. Rows past this are striped in the waterfall, not dropped: whether roll corrupts the geometry is not established.'));

  const acc = document.createElement('p');
  acc.className = 'hint warn';
  acc.textContent = 'Good to 5–10 m on a straight line. Worse in turns: the three '
    + 'models disagree by 9–15 m there, and the cable bounds the offset’s length '
    + 'but nothing observes its bearing.';
  body.appendChild(acc);

  const m = d.mosaic;
  const mh = document.createElement('h4');
  mh.textContent = 'Mosaic';
  body.appendChild(mh);
  const mp = document.createElement('p');
  mp.className = 'hint';
  mp.textContent = 'How the imagery is painted, as opposed to where. These '
    + 'repaint the mosaic; the colour scheme on each mosaic layer does not.';
  body.appendChild(mp);

  body.appendChild(field('Time-varied gain',
    slider(0, 100, 5, Math.round((m.tvg ?? 0.7) * 100), (v) => { m.tvg = v / 100; },
      (v) => `${v}%`),
    'Undoes spreading and absorption, which cost 39 dB across a 47 m swath at '
    + '580 kHz and 44 dB across 30 m at 1550 kHz. Without it every pass paints a '
    + 'bright band down its own middle. Full correction is the physically right '
    + 'answer and also amplifies whatever is at the swath edge, noise included.'));

  body.appendChild(field('Across-track axis', select([
    ['ground', 'Ground range \u2014 water column removed'],
    ['slant', 'Slant range \u2014 water column kept'],
  ], m.axis || 'ground', (v) => { m.axis = v; }),
    'The same choice the waterfall\u2019s Axis control makes, so the two views can '
    + 'be read side by side. Ground range puts every sample where the seabed it '
    + 'came off actually is and the water column disappears, because ground range '
    + 'zero is the first bottom return. Slant range draws what the sonar measured, '
    + 'uncorrected \u2014 so the water column stays, a band two flying heights wide '
    + 'down every pass, and everything outside it sits slant\u2212ground too far out. '
    + 'On slant the mosaic is a picture, not a map: a position read off it is wrong '
    + 'by that much. Blank nadir set to the flying height cuts the water column back '
    + 'out without leaving the slant axis.'));

  body.appendChild(field('Angle-varying gain',
    slider(0, 100, 5, Math.round((m.angular_gain ?? 1) * 100),
      (v) => { m.angular_gain = v / 100; }, (v) => `${v}%`),
    'Divides out the across-track shading the time-varied gain leaves behind — '
    + 'the beam pattern and the seabed\u2019s angular response — measured against '
    + 'grazing angle over the whole recording rather than modelled. Without it '
    + 'every pass paints a bright core with dark edges (a 5\u00d7 swing on '
    + '080929_demimines, 2\u00d7 on 070926_measures_star) and the mosaic breaks '
    + 'into patches wherever two passes cross at different headings. Measured '
    + 'per side, so it takes the port/starboard offset with it.'));

  body.appendChild(field('Along-track gain',
    slider(0, 100, 5, Math.round((m.agc ?? 0.5) * 100), (v) => { m.agc = v / 100; },
      (v) => `${v}%`),
    'Pulls each ping onto the running median of its neighbours. The waterfall '
    + 'has always done this and the mosaic never did, which is why one looked '
    + 'even and the other striped. It only removes the part of the banding that '
    + 'port and starboard share — the rest is the seabed, and turning this up '
    + 'flattens a real hard-to-soft transition along with the artefact.'));

  body.appendChild(field('Speckle',
    slider(0, 100, 5, Math.round((m.despeckle ?? 0) * 100), (v) => { m.despeckle = v / 100; },
      (v) => `${v}%`),
    'Suppresses the pixel-scale scatter that is sonar speckle rather than '
    + 'seabed — 37% of the variance in a painted raster, measured over a fully '
    + 'covered 69 m square. It is adaptive: it smooths where the local spread is '
    + 'no more than speckle can explain and leaves targets and shadow edges '
    + 'exactly as they were, which a median filter would not. Zero by default '
    + 'because how hard to smooth a survey is your call, not a constant.'));

  // Suggested from this recording's own flying height: the main lobe starts
  // at tan(32 deg) x altitude, so anything inside that is the beam's edge.
  const sum = S.loaded.get(l.dataset);
  const alt = sum && sum.altitude_m;
  const lobe = alt > 0 ? (alt * 0.6249).toFixed(1) : null;
  body.appendChild(field('Blank nadir',
    input('number', m.nadir_blank_m || '', (v) => { m.nadir_blank_m = v > 0 ? v : 0; },
      { step: '0.5', min: '0', placeholder: '0' }),
    'Metres either side of the track left unpainted. Straight under the fish is '
    + 'outside the main beam — the array is depressed 33° with a 50° vertical '
    + 'beam, so it lights 32° to 82° off vertical'
    + (lobe ? `, which on this recording starts ${lobe} m out. ` : '. ')
    + 'The band inside is painted at the lowest priority so the chart has no hole '
    + 'down every pass; set this if you would rather have the hole.'));

  body.appendChild(field('', checkbox('decibel scale', !!m.db, (v) => { m.db = v; }),
    'Backscatter spans orders of magnitude and the contrast clips are '
    + 'percentiles, so on a linear scale the specular return under the fish sets '
    + 'the white point for the whole survey.'));

  body.appendChild(field('Range limit',
    input('number', m.max_range_m || '', (v) => { m.max_range_m = v > 0 ? v : 0; },
      { step: '5', min: '0', placeholder: 'none' }),
    'Metres of slant range to paint, all channels of this recording. The high '
    + 'channel is often set to a range it cannot reach — 1550 kHz is specified '
    + 'to 35 m and was recorded to 50 m here — and the tail of those traces is '
    + 'noise that the gain above will happily amplify.'));
}

function buildMosaicFields(body, l) {
  const st = l.style || (l.style = { ramp: 'grey', reverse: false, lo: 0, hi: 255 });
  body.appendChild(field('Colour scheme', rampPicker(st.ramp || 'grey', (v) => {
    st.ramp = v; live();
  }), 'Grey is the honest one. A ramp makes small differences in return strength easier to see and equally easy to over-read.'));
  body.appendChild(field('', checkbox('reverse', st.reverse, (v) => { st.reverse = v; live(); })));
  body.appendChild(field('Black point',
    slider(0, 254, 1, st.lo ?? 0, (v) => { st.lo = v; live(); }, (v) => `${v}`)));
  body.appendChild(field('White point',
    slider(1, 255, 1, st.hi ?? 255, (v) => { st.hi = v; live(); }, (v) => `${v}`),
    'A contrast stretch on the painted raster. Narrowing these costs nothing — it is a lookup on the way out, not a repaint.'));
  const b = document.createElement('button');
  b.type = 'button';
  b.className = 'mini';
  b.textContent = 'Repaint from the recording';
  b.addEventListener('click', async () => {
    busy(`repainting ${l.label}…`);
    try {
      await api.buildMosaic(l.dataset, l.subsystem, true);
      map.tiles.clear();
      await syncLayers();
      msg(`${l.label} repainted`);
    } catch (e) { msg(e.message, 'error'); }
  });
  body.appendChild(field('', b, 'Discards the raster and paints it again from the pings, with the navigation as it stands now.'));
}

function buildRasterFields(body, l) {
  const st = l.style || (l.style = {});
  body.appendChild(field('Colour scheme', rampPicker(st.ramp, (v) => { st.ramp = v; live(); })));
  body.appendChild(field('', checkbox('reverse', st.reverse, (v) => { st.reverse = v; live(); })));
  const range = document.createElement('div');
  range.className = 'ld-range';
  const lo = input('number', st.min ?? '', (v) => {
    st.min = Number.isFinite(v) ? v : null; live();
  }, { step: '0.1', placeholder: 'auto' });
  const hi = input('number', st.max ?? '', (v) => {
    st.max = Number.isFinite(v) ? v : null; live();
  }, { step: '0.1', placeholder: 'auto' });
  range.appendChild(lo);
  range.appendChild(hi);
  body.appendChild(field('Value range', range,
    'Ends of the ramp, in the grid’s own units. Blank uses the 2nd and 98th percentile.'));
  body.appendChild(field('Relief',
    slider(0, 1, 0.05, st.shade ?? 0.55, (v) => { st.shade = v; live(); })));
  body.appendChild(field('Exaggeration',
    slider(0, 20, 0.5, st.shade_exaggeration ?? 6, (v) => { st.shade_exaggeration = v; live(); },
      (v) => `×${v}`),
    'Seabed relief is centimetres over metres of ground. Lit truthfully it would be invisible.'));
  body.appendChild(field('Sun bearing',
    slider(0, 355, 5, st.sun_azimuth_deg ?? 315, (v) => { st.sun_azimuth_deg = v; live(); },
      (v) => `${v}°`)));
  body.appendChild(field('Sun elevation',
    slider(5, 85, 5, st.sun_elevation_deg ?? 40, (v) => { st.sun_elevation_deg = v; live(); },
      (v) => `${v}°`)));
}

function buildTrackFields(body, l) {
  body.appendChild(field('Colour',
    input('color', l.colour || '#35b8a6', (v) => { l.colour = v; live(); })));
  // Which line this is, is what the layer is. The two checkboxes that used to
  // be here made every track able to be both, which is how a recording ended up
  // drawing the same line twice.
  body.appendChild(field('', note(l.show_boat
    ? 'Where the antenna was, from the recorded fixes.'
    : 'Where the fish was: the boat track laid back, and what every mosaic of '
      + 'this recording is painted from.')));
}

function note(text) {
  const p = document.createElement('p');
  p.className = 'hint';
  p.style.margin = '0';
  p.textContent = text;
  return p;
}

on('layer-dialog', 'close', async () => {
  const l = dialogLayer;
  dialogLayer = null;
  if (!l) return;
  const v = $('layer-dialog').returnValue;
  if (v === 'remove') {
    S.layers = removeNode(S.layers, l.id);
    if (l.kind === RECORDING) await unloadDataset(l.dataset);
    S.features.delete(l.id);
    await syncLayers();
    saveView();
    msg(`${l.label} removed`);
    return;
  }
  if (v !== 'save') { await syncLayers(); return; }   // cancel: redraw as saved
  if (l.kind === RECORDING) await applyNav(l.dataset);
  await syncLayers();
  saveView();
});

// ---- projects -------------------------------------------------------------

/// Open a project by name, or none at all.
///
/// Switching away discards this project's in-memory state, so what is pending
/// has to reach disk first.
async function switchProject(name) {
  await flushView();
  if (!name) {
    S.project = null;
    S.contacts = [];
    renderContacts();
    $('project').value = '';
    return;
  }
  S.project = await api.openProject(name);
  localStorage.setItem('swath.project', S.project.name);
  $('project').value = name;
  adoptLayers();
  await loadContacts();
  await restore();
  msg(`project ${S.project.name}`);
}

/// Every project in the workspace, and what can be done to one.
///
/// The list comes from the server rather than from the header's dropdown,
/// because what tells two projects apart is what was marked in them and which
/// recordings they hold -- and because a delete has to be able to say, before
/// it happens, what it is about to take with it.
async function openProjects() {
  const dlg = $('projects-dialog');
  dlg.returnValue = '';
  dlg.showModal();
  await renderProjects();
}

async function renderProjects() {
  const list = $('pm-list');
  list.innerHTML = '<li class="none">reading…</li>';
  let r;
  try {
    r = await api.projects();
  } catch (e) {
    list.innerHTML = `<li class="none">${esc(e.message)}</li>`;
    return;
  }
  list.innerHTML = '';
  if (!r.projects.length) {
    list.innerHTML = '<li class="none">No projects yet.</li>';
    return;
  }
  for (const p of r.projects) list.appendChild(projectRow(p));
}

function projectRow(p) {
  const li = document.createElement('li');
  li.className = 'pm-row' + (p.open ? ' open' : '');
  const bits = [
    `${p.datasets} recording${p.datasets === 1 ? '' : 's'}`,
    `${p.contacts} contact${p.contacts === 1 ? '' : 's'}`,
  ];
  if (p.holds.length) bits.push(`${p.holds.length} held here`);
  if (p.modified) bits.push(`edited ${p.modified.slice(0, 10)}`);
  const sub = [];
  if (p.title && p.title !== p.name) sub.push(esc(p.title));
  if (p.area) sub.push(esc(p.area));
  sub.push(...bits.map(esc));

  li.innerHTML = `
    <div class="pm-name">${esc(p.name)}${p.open ? '<span class="tag">open</span>' : ''}</div>
    <div class="pm-sub">${sub.join('<span class="sep">·</span>')}</div>
    <div class="pm-acts">
      <button type="button" data-a="open"${p.open ? ' disabled' : ''}>Open</button>
      <button type="button" data-a="rename">Rename</button>
      <button type="button" data-a="copy">Duplicate</button>
      <button type="button" data-a="delete" class="danger">Delete</button>
    </div>`;
  if (!p.readable) {
    li.querySelector('.pm-sub').innerHTML =
      '<span class="pm-warn">project.json could not be read</span>';
  }

  const act = (a) => li.querySelector(`[data-a="${a}"]`);
  act('open').addEventListener('click', async () => {
    $('projects-dialog').close();
    try { await switchProject(p.name); await refresh(); }
    catch (e) { msg(`could not open ${p.name}: ${e.message}`, 'error'); }
  });
  act('rename').addEventListener('click', () => {
    askName(li, 'Rename', p.name, async (to) => {
      await runOnProject(li, `renaming ${p.name}`, () => api.renameProject(p.name, to),
        () => { if (p.open) localStorage.setItem('swath.project', to); });
    });
  });
  act('copy').addEventListener('click', () => {
    askName(li, 'Duplicate', `${p.name} copy`, async (to) => {
      await runOnProject(li, `copying ${p.name}`, () => api.copyProject(p.name, to));
    });
  });
  act('delete').addEventListener('click', () => askDelete(li, p));
  return li;
}

function clearRowExtras(li) {
  li.querySelectorAll('.pm-edit, .pm-warn-row').forEach(x => x.remove());
}

/// A name asked for in the row itself. `prompt()` is not available in every
/// shell this runs in, and a modal on top of a modal is worse than an inline
/// field anyway.
function askName(li, label, value, onok) {
  clearRowExtras(li);
  const row = document.createElement('div');
  row.className = 'pm-edit';
  row.innerHTML = `<input spellcheck="false" autocomplete="off">
    <button type="button" class="primary">${esc(label)}</button>
    <button type="button">Cancel</button>`;
  const input = row.querySelector('input');
  const [ok, cancel] = row.querySelectorAll('button');
  input.value = value;
  cancel.addEventListener('click', () => row.remove());
  ok.addEventListener('click', () => {
    const v = input.value.trim();
    if (v) onok(v);
  });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); ok.click(); }
    if (e.key === 'Escape') { e.preventDefault(); row.remove(); }
  });
  li.appendChild(row);
  input.focus();
  input.select();
}

/// Deleting a project is the one thing here that cannot be undone, so it says
/// what it will take before it takes it -- and a recording the project holds
/// outright has to be named, because nothing else has a copy of it.
function askDelete(li, p) {
  clearRowExtras(li);
  const warn = document.createElement('div');
  warn.className = 'pm-warn pm-warn-row';
  warn.textContent = p.holds.length
    ? `${p.name} holds ${p.holds.join(', ')}. Nothing else has those files.`
    : `Its contacts, layer stack and report settings go. Linked recordings stay where they are.`;
  const row = document.createElement('div');
  row.className = 'pm-edit';
  row.innerHTML = `<div class="spacer"></div>
    <button type="button" class="danger">${p.holds.length ? 'Delete it and the recordings' : 'Delete'}</button>
    <button type="button">Keep</button>`;
  const [del, keep] = row.querySelectorAll('button');
  keep.addEventListener('click', () => clearRowExtras(li));
  del.addEventListener('click', async () => {
    await runOnProject(li, `deleting ${p.name}`,
      () => api.deleteProject(p.name, p.holds.length > 0),
      () => {
        if (!p.open) return;
        S.project = null;
        S.contacts = [];
        S.layers = [];
        S.loaded.clear();
        localStorage.removeItem('swath.project');
      });
  });
  li.append(warn, row);
}

/// Run one project operation, then put the window back in step with the disk.
///
/// The flush is not optional. The browser holds unwritten changes for up to six
/// hundred milliseconds, and a pending save landing after a rename would write
/// the project back out under the name it no longer has.
async function runOnProject(li, what, fn, after) {
  clearRowExtras(li);
  busy(`${what}…`);
  try {
    await flushView();
    await fn();
    if (after) after();
    await refresh();
    renderContacts();
    await syncLayers();
    await renderProjects();
    msg(what.replace(/ing /, 'ed '));
  } catch (e) {
    msg(`could not ${what}: ${e.message}`, 'error');
    await renderProjects();
  }
}

// ---- adding a recording ----------------------------------------------------

let recAt = null;
let recChosen = null;

/// Add a recording from anywhere on the machine.
///
/// The recording then belongs to the project -- copied, moved or linked into
/// `projects/<name>/data` -- rather than having to be put under the workspace's
/// own `data/` first. Recordings already in `data/` are still offered by the
/// project settings dialog; this is for everything that is not.
async function openRecordingImport() {
  if (!S.project) { msg('open or create a project first', 'error'); return; }
  recChosen = null;
  $('rec-add').disabled = true;
  $('rec-name').value = '';
  $('rec-name').disabled = true;
  $('rec-note').textContent = 'Nothing picked yet.';
  $('recording-dialog').returnValue = '';
  $('recording-dialog').showModal();
  await recBrowseTo(localStorage.getItem('swath.rec-browse')
    || localStorage.getItem('swath.browse') || '');
}

async function recBrowseTo(path) {
  const list = $('rec-list');
  list.innerHTML = '<li class="none">reading…</li>';
  let r;
  try {
    r = await api.browse(path, 'recordings');
  } catch (e) {
    list.innerHTML = `<li class="none">${esc(e.message)}</li>`;
    return;
  }
  recAt = r.path;
  $('rec-path').value = r.path;
  localStorage.setItem('swath.rec-browse', r.path);
  list.innerHTML = '';
  if (r.parent) {
    list.appendChild(browseRow('↑ ' + r.parent, 'dir', () => recBrowseTo(r.parent)));
  }
  // The folder being looked at can be the answer, and usually is: the operator
  // navigates into the recording to see what is in it before choosing it.
  if (r.sonar) {
    list.appendChild(takeRow(
      { name: baseName(r.path), path: r.path, sonar: r.sonar, bytes: r.bytes },
      'use this folder'));
  }
  for (const d of r.dirs) {
    list.appendChild(d.sonar
      ? takeRow(d)
      : browseRow(d.name + '/', 'dir', () => recBrowseTo(d.path)));
  }
  if (!r.dirs.length && !r.sonar) {
    list.innerHTML = '<li class="none">Nothing here. Open a folder that holds '
      + '.jsf or .xtf files, or one that contains such folders.</li>';
  }
}

/// A folder that holds sonar: still navigable, with a button that says "this is
/// the one".
function takeRow(d, label = null) {
  const li = browseRow(label || d.name + '/', 'dir',
    () => { if (!label) recBrowseTo(d.path); },
    `${d.sonar} file${d.sonar === 1 ? '' : 's'} · ${fmtBytes(d.bytes || 0)}`);
  li.classList.add('br-rec');
  const take = document.createElement('button');
  take.type = 'button';
  take.className = 'br-take';
  take.textContent = 'Use';
  take.addEventListener('click', (e) => { e.stopPropagation(); recTake(li, d); });
  li.appendChild(take);
  return li;
}

async function recTake(li, d) {
  recChosen = { ...d };
  $('rec-list').querySelectorAll('li').forEach(x => x.classList.remove('sel'));
  li.classList.add('sel');
  $('rec-add').disabled = false;
  const size = `${d.sonar} file${d.sonar === 1 ? '' : 's'} · ${fmtBytes(d.bytes || 0)}`;

  // Already known -- browsed to in `data/`, or held by another project and
  // reached through it. Importing it again would give the same recording a
  // second name and a second index; adding the one that exists is what was
  // meant.
  const known = (S.datasets || []).find(x => x.path === d.path);
  if (known) {
    recChosen.existing = known.name;
    $('rec-name').value = known.name;
    $('rec-name').disabled = true;
    $('rec-note').textContent =
      `Already in the workspace as ${known.name} · ${size}. Adding it to this project.`;
    return;
  }

  $('rec-name').disabled = false;
  $('rec-note').textContent = `${d.path} · ${size}`;
  // The name has to be free across the whole workspace, because everything
  // derived from a recording is filed under it. Ask for a free one rather than
  // letting the operator find the clash when the import fails.
  try {
    const { name, taken } = await api.freeDatasetName(d.name);
    $('rec-name').value = name;
    if (taken) {
      $('rec-note').textContent +=
        ` — there is already a recording called ${d.name}, so this one would be ${name}`;
    }
  } catch {
    $('rec-name').value = d.name;
  }
}

function baseName(p) {
  const parts = String(p).split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] || p;
}

async function recAddChosen() {
  if (!recChosen) return;
  if (recChosen.existing) {
    $('recording-dialog').close();
    datasetRef(recChosen.existing);
    await loadDataset(recChosen.existing, { fit: true });
    await flushView();
    return;
  }
  const name = $('rec-name').value.trim();
  const mode = document.querySelector('input[name="rec-mode"]:checked').value;
  if (!name) { msg('the recording needs a name', 'error'); return; }
  busy(mode === 'copy' ? `copying ${name}… ${fmtBytes(recChosen.bytes || 0)} to write` : `adding ${name}…`);
  try {
    const r = await api.importDataset(recChosen.path, name, mode);
    $('recording-dialog').close();
    await refresh();
    datasetRef(r.name);
    await loadDataset(r.name, { fit: true });
    await flushView();
    msg(`${r.name}: ${r.files} file${r.files === 1 ? '' : 's'} ${mode === 'link' ? 'linked' : mode === 'move' ? 'moved' : 'copied'} into ${S.project.name}`);
  } catch (e) {
    msg(`could not add ${name}: ${e.message}`, 'error');
  }
}

// ---- project dialog --------------------------------------------------------

let pdMode = {};

/// One dialog for creating a project and for editing it.
///
/// Creating one used to be a `prompt()` for a name and nothing else, which left
/// every decision that matters -- which recordings, how each was rigged, what
/// grids the client wants -- to be found later in three different panels.
async function openProjectDialog(opts = {}) {
  pdMode = opts;
  const creating = !!opts.create;
  if (!creating && !S.project) { msg('open or create a project first', 'error'); return; }
  const p = creating ? { name: '', title: '', meta: {}, datasets: [] } : S.project;
  const m = p.meta || {};

  $('pd-title-h').textContent = creating ? 'New project' : 'Project settings';
  $('pd-save').textContent = creating ? 'Create' : 'Save';
  $('pd-name-row').style.display = creating ? '' : 'none';
  $('pd-name').value = '';
  $('pd-title').value = p.title || '';
  $('pd-client').value = m.client || '';
  $('pd-vessel').value = m.vessel || '';
  $('pd-operator').value = m.operator || '';
  $('pd-job').value = m.job_number || '';
  $('pd-area').value = m.area || '';
  $('pd-notes').value = m.notes || '';

  // Recordings: everything on disk, ticked if the project already has it, each
  // with its own navigation.
  const box = $('pd-datasets');
  box.innerHTML = '';
  pdNav = new Map();
  if (!S.datasets.length) {
    box.innerHTML = '<span class="none">No recordings yet. '
      + 'Use Recording… in the left panel to add one from anywhere on disk.</span>';
  }
  for (const d of S.datasets) {
    const have = (p.datasets || []).find(x => x.name === d.name);
    const nav = { ...(have?.nav || defaultNav()) };
    pdNav.set(d.name, nav);
    box.appendChild(datasetRow(d, !!have, nav));
  }

  // Grids: the ones that apply where the survey actually is.
  const crsBox = $('pd-crs');
  crsBox.innerHTML = '<span class="none">looking up…</span>';
  const centre = surveyCentre();
  const chosenCrs = new Set(m.report_crs || []);
  try {
    const { systems } = await api.crs(centre[0], centre[1]);
    crsBox.innerHTML = '';
    for (const c of systems) {
      if (c.epsg === 4326) continue;              // always the first column
      const l = document.createElement('label');
      l.innerHTML = `<input type="checkbox" value="${c.epsg}" ${chosenCrs.has(c.epsg) ? 'checked' : ''}>
        <span>${esc(c.name)}</span><span class="epsg">EPSG:${c.epsg}</span>`;
      crsBox.appendChild(l);
    }
    for (const e of chosenCrs) {
      if (!systems.some(c => c.epsg === e)) {
        const l = document.createElement('label');
        l.innerHTML = `<input type="checkbox" value="${e}" checked>
          <span>EPSG:${e}</span><span class="epsg">not local</span>`;
        crsBox.appendChild(l);
      }
    }
  } catch (e) {
    crsBox.innerHTML = `<span class="none">could not list systems: ${esc(e.message)}</span>`;
  }

  $('project-dialog').returnValue = '';
  $('project-dialog').showModal();
  if (creating) $('pd-name').focus();
}

let pdNav = new Map();

function datasetRow(d, ticked, nav) {
  const wrap = document.createElement('div');
  wrap.className = 'pd-ds';
  wrap.innerHTML = `
    <label class="pd-ds-head">
      <input type="checkbox" data-name="${esc(d.name)}" ${ticked ? 'checked' : ''}>
      <span class="pd-ds-name">${esc(d.name)}</span>
      <span class="epsg">${esc(datasetWhere(d))}</span>
    </label>
    <div class="pd-ds-nav">
      <label class="field"><span>Layback</span><input type="number" step="0.5" data-k="layback_m" value="${nav.layback_m}"><em>m</em></label>
      <label class="field"><span>GPS→tow</span><input type="number" step="0.5" data-k="gps_to_towpoint_m" value="${nav.gps_to_towpoint_m}"><em>m</em></label>
      <label class="field"><span>Model</span>
        <select data-k="model">
          <option value="astern">Constant, astern</option>
          <option value="wake">Follows the wake</option>
          <option value="tractrix">Tractrix</option>
        </select>
      </label>
      <label class="field"><span>Bearing</span>
        <select data-k="bearing">
          <option value="cog">Course over ground</option>
          <option value="compass">Fish compass</option>
        </select>
      </label>
    </div>`;
  wrap.querySelector('[data-k="model"]').value = nav.model;
  wrap.querySelector('[data-k="bearing"]').value = nav.bearing;
  wrap.querySelectorAll('[data-k]').forEach(el => {
    el.addEventListener('change', () => {
      const k = el.dataset.k;
      nav[k] = el.type === 'number' ? parseFloat(el.value) : el.value;
    });
  });
  const tick = wrap.querySelector('input[type=checkbox]');
  const sync = () => wrap.classList.toggle('on', tick.checked);
  tick.addEventListener('change', sync);
  sync();
  // A recording the project holds is the project's to get rid of. One in the
  // shared pool is not: unticking it is the whole of "remove" there.
  if (d.owned) wrap.querySelector('.pd-ds-head').appendChild(removeDatasetButton(d, wrap));
  return wrap;
}

/// Where a recording lives, and how much of it there is.
function datasetWhere(d) {
  const files = `${d.files} file${d.files === 1 ? '' : 's'}`;
  const where = !d.owned ? 'in data/' : d.linked ? 'linked here' : 'held here';
  return `${where} · ${d.indexed ? files : `${files}, not indexed yet`}`;
}

function removeDatasetButton(d, wrap) {
  const b = document.createElement('button');
  b.type = 'button';
  b.className = 'pd-ds-drop';
  b.title = d.linked
    ? 'Drop the link. The files it points at stay where they are.'
    : 'Delete this recording from the project. Nothing else has these files.';
  b.textContent = '✕';
  b.addEventListener('click', async (e) => {
    e.preventDefault();
    if (b.dataset.armed !== '1') {
      b.dataset.armed = '1';
      b.textContent = d.linked ? 'unlink?' : 'delete?';
      b.classList.add('armed');
      setTimeout(() => {
        if (b.dataset.armed !== '1') return;
        b.dataset.armed = '';
        b.textContent = '✕';
        b.classList.remove('armed');
      }, 4000);
      return;
    }
    try {
      await api.removeDataset(d.name);
      S.loaded.delete(d.name);
      S.layers = removeNode(S.layers, recordingId(d.name));
      if (S.project) {
        S.project.datasets = (S.project.datasets || []).filter(x => x.name !== d.name);
      }
      pdNav.delete(d.name);
      wrap.remove();
      await refresh();
      msg(`${d.name}: ${d.linked ? 'unlinked' : 'deleted'}`);
    } catch (err) {
      msg(`could not remove ${d.name}: ${err.message}`, 'error');
    }
  });
  return b;
}

on('project-dialog', 'close', async () => {
  if ($('project-dialog').returnValue !== 'save') return;
  const creating = !!pdMode.create;
  const meta = {
    client: $('pd-client').value.trim(),
    vessel: $('pd-vessel').value.trim(),
    operator: $('pd-operator').value.trim(),
    job_number: $('pd-job').value.trim(),
    area: $('pd-area').value.trim(),
    notes: $('pd-notes').value,
    report_crs: [...$('pd-crs').querySelectorAll('input:checked')]
      .map(i => parseInt(i.value, 10)).filter(Number.isFinite),
  };
  const wanted = [...$('pd-datasets').querySelectorAll('input[type=checkbox]:checked')]
    .map(i => i.dataset.name);

  try {
    if (creating) {
      const name = $('pd-name').value.trim();
      if (!name) { msg('a project needs a name', 'error'); return; }
      S.project = await api.newProject(name);
      localStorage.setItem('swath.project', S.project.name);
      adoptLayers();
    }
    S.project.title = $('pd-title').value.trim() || S.project.name;
    S.project.meta = { ...(S.project.meta || {}), ...meta };
    S.project.datasets = S.project.datasets || [];
    for (const [name, nav] of pdNav) {
      const d = S.project.datasets.find(x => x.name === name);
      if (d) d.nav = { ...d.nav, ...nav };
    }

    // Load what was ticked, unload what was not.
    for (const name of [...S.loaded.keys()]) {
      if (!wanted.includes(name)) await unloadDataset(name);
    }
    for (const name of wanted) {
      const d = datasetRef(name);
      d.nav = { ...d.nav, ...(pdNav.get(name) || {}) };
      if (S.loaded.has(name)) await applyNav(name);
      else await loadDataset(name, { fit: false, waterfall: false });
    }
    await loadWaterfall(true);
    await syncLayers();
    fitAll();
    // Written before the refresh, not after: `refresh` re-reads the project
    // list and would otherwise race the debounce.
    saveView();
    await flushView();
    await refresh();
    msg(creating ? `created ${S.project.name}` : 'project saved');
  } catch (e) {
    msg(`could not save the project: ${e.message}`, 'error');
  }
});

function fitAll() {
  const b = allBounds();
  if (b) map.fit(b);
}

/// Middle of everything loaded, for suggesting coordinate systems.
function surveyCentre() {
  const bs = [...S.loaded.values()].map(s => s.bounds).filter(Boolean);
  if (!bs.length) return [map.centre[0], map.centre[1]];
  const u = bs.reduce((a, x) => ({
    min_lat: Math.min(a.min_lat, x.min_lat), min_lon: Math.min(a.min_lon, x.min_lon),
    max_lat: Math.max(a.max_lat, x.max_lat), max_lon: Math.max(a.max_lon, x.max_lon),
  }));
  return [(u.min_lat + u.max_lat) / 2, (u.min_lon + u.max_lon) / 2];
}

// ---- formatting ------------------------------------------------------------

function esc(s) {
  return String(s).replace(/[&<>"']/g, ch =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch]));
}
function cssEsc(s) { return String(s).replace(/["\\]/g, '\\$&'); }
function fmtInt(n) { return (n ?? 0).toLocaleString('en-GB'); }
function fmtBytes(n) {
  if (!(n > 0)) return '';
  const u = ['B', 'kB', 'MB', 'GB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i && n < 10 ? 1 : 0)} ${u[i]}`;
}
function hms(s) {
  s = Math.max(0, Math.round(s || 0));
  return `${String((s / 3600) | 0).padStart(2, '0')}:${String(((s % 3600) / 60) | 0).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`;
}
function fmtDM(deg, isLat) {
  const hemi = isLat ? (deg >= 0 ? 'N' : 'S') : (deg >= 0 ? 'E' : 'W');
  const a = Math.abs(deg), d = Math.floor(a);
  return `${d}° ${((a - d) * 60).toFixed(5).padStart(8, '0')}' ${hemi}`;
}
function bearingOf(a, b) {
  const D = Math.PI / 180;
  const p1 = a[0] * D, p2 = b[0] * D, dl = (b[1] - a[1]) * D;
  return (Math.atan2(Math.sin(dl) * Math.cos(p2),
    Math.cos(p1) * Math.sin(p2) - Math.sin(p1) * Math.cos(p2) * Math.cos(dl)) * 180 / Math.PI + 360) % 360;
}

boot().catch(e => { console.error(e); msg(`startup failed: ${e.message}`, 'error'); });
