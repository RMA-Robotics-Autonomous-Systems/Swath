// Headless checks for the browser-side logic.
//
//     bun ui/test/run.js
//
// These cover the two things that are arithmetic rather than appearance: where
// a scrolled waterfall row actually is, and what a drag does to the layer
// order. Both were wrong at some point during development in ways that looked
// perfectly fine on screen.

import { installGlobals, fakeCanvas, fakeBlock } from './dom.js';
installGlobals();

const { WaterfallView } = await import('../waterfall.js');
const { toGround, fromGround, sliderRow } = await import('../waterfall.js');
const { buildTree, flatten, moveWithinSiblings, dropIndex, removeNode,
        drawOrder, isVisible, shouldAdopt, isReady, mosaicId, trackId, recordingId,
        normalise, migrate, oneTrackPerTow } = await import('../layers.js');
const { buildReportSpec, bandName, chartSize, overviewId, datasetChartId }
  = await import('../report.js');
const geo = await import('../geo.js');
const { api } = await import('../api.js');
const { nextEnabled } = await import('../select.js');
const { TileStore, MapView } = await import('../map.js');

let failures = 0;
let checks = 0;

function ok(cond, what) {
  checks++;
  if (!cond) { failures++; console.error(`  FAIL  ${what}`); }
}
function near(got, want, tol, what) {
  checks++;
  if (!(Math.abs(got - want) <= tol)) {
    failures++;
    console.error(`  FAIL  ${what}: ${got} vs ${want} (tolerance ${tol})`);
  }
}
async function group(name, fn) { console.log(name); await fn(); }

// ---- the project tree ------------------------------------------------------

group('tree ordering', () => {
  // A recording with two children, an imported grid, and a GPX.
  const mk = () => [
    { id: 'rec:A', kind: 'recording', dataset: 'A', parent: '' },
    { id: 'mosaic:A:20', kind: 'mosaic', parent: 'rec:A' },
    { id: 'track:A:20', kind: 'track', parent: 'rec:A' },
    { id: 'rec:B', kind: 'recording', dataset: 'B', parent: '' },
    { id: 'mosaic:B:20', kind: 'mosaic', parent: 'rec:B' },
    { id: 'raster:dtm', kind: 'raster', parent: '' },
    { id: 'vector:gpx', kind: 'vector', parent: '' },
  ];
  const ids = (l) => l.map(x => x.id).join(' ');

  const t = buildTree(mk());
  ok(t.length === 4, 'four top-level nodes');
  ok(t[0].children.length === 2, 'recording A has two children');
  ok(ids(flatten(t)) === ids(mk()), 'tree and flat list are the same thing');

  // A whole recording moves with its children.
  let r = moveWithinSiblings(mk(), 'rec:B', 0);
  ok(ids(r.list) === 'rec:B mosaic:B:20 rec:A mosaic:A:20 track:A:20 raster:dtm vector:gpx',
     'recording B dragged to the top brings its mosaic');

  // Dragging downwards: the drop index counts rows before the move.
  r = moveWithinSiblings(mk(), 'rec:A', 2);
  ok(ids(r.list) === 'rec:B mosaic:B:20 rec:A mosaic:A:20 track:A:20 raster:dtm vector:gpx',
     'A dropped between B and the grid');
  r = moveWithinSiblings(mk(), 'rec:A', 4);
  ok(ids(r.list) === 'rec:B mosaic:B:20 raster:dtm vector:gpx rec:A mosaic:A:20 track:A:20',
     'A dropped at the very bottom');
  r = moveWithinSiblings(mk(), 'vector:gpx', 0);
  ok(ids(r.list) === 'vector:gpx rec:A mosaic:A:20 track:A:20 rec:B mosaic:B:20 raster:dtm',
     'the GPX dragged to the top');

  // Children reorder inside their own parent and nowhere else.
  r = moveWithinSiblings(mk(), 'track:A:20', 0);
  ok(ids(r.list) === 'rec:A track:A:20 mosaic:A:20 rec:B mosaic:B:20 raster:dtm vector:gpx',
     "A's track moved above its mosaic");
  ok(r.list.every(l => l.parent !== 'rec:B' || l.id.includes(':B:')),
     'a child cannot land under a different recording');

  // No-ops report no move, or the panel saves on every stray click.
  ok(moveWithinSiblings(mk(), 'rec:A', 0).moved === false, 'dropped where it already is');
  ok(moveWithinSiblings(mk(), 'rec:A', 1).moved === false, 'dropped just after itself');
  ok(moveWithinSiblings(mk(), 'nope', 0).moved === false, 'an unknown id changes nothing');
  ok(ids(mk()) === ids(mk()), 'the input list is left alone');

  // Down then back up returns the original order.
  const down = moveWithinSiblings(mk(), 'rec:A', 3);
  const back = moveWithinSiblings(down.list, 'rec:A', 0);
  ok(ids(back.list) === ids(mk()), 'moved down then back up');

  // An orphan is promoted, not lost.
  const orphaned = mk().filter(l => l.id !== 'rec:B');
  ok(buildTree(orphaned).length === 4, 'a child whose recording went away is promoted');

  // Removing a recording takes its children with it.
  ok(ids(removeNode(mk(), 'rec:A')) === 'rec:B mosaic:B:20 raster:dtm vector:gpx',
     'removing a recording removes its layers');

  const mids = [10, 30, 50, 70];
  ok(dropIndex(mids, 5) === 0, 'above everything');
  ok(dropIndex(mids, 20) === 1, 'between the first and second');
  ok(dropIndex(mids, 100) === 4, 'below everything');
  ok(mosaicId('day2', 20) === 'mosaic:day2:20', 'mosaic id');
  ok(trackId('day2', 20) === 'track:day2:20', 'track id');
  ok(recordingId('day2') === 'rec:day2', 'recording id');
});

group('migrating an older project', () => {
  // Saved by the flat-list version: mosaics at the top level, no parents, no
  // recording nodes, and the operator's order to preserve.
  const old = [
    { id: 'raster:dtm', kind: 'raster' },
    { id: 'mosaic:A:20', kind: 'mosaic', dataset: 'A', subsystem: 20 },
    { id: 'vector:gpx', kind: 'vector' },
    { id: 'mosaic:A:21', kind: 'mosaic', dataset: 'A', subsystem: 21 },
  ];
  const m = migrate(old);
  ok(m.length === 4, 'nothing is dropped');
  ok(m.every(l => l.parent !== undefined), 'every node has a parent field');
  ok(m.find(l => l.id === 'mosaic:A:20').parent === 'rec:A', "A's mosaic is adopted");
  ok(m.find(l => l.id === 'raster:dtm').parent === '', 'an imported grid stays top level');
  // The recording node does not exist yet, so its orphans are promoted and the
  // relative order of what is left is untouched.
  ok(m.map(l => l.id).join(' ') === 'raster:dtm mosaic:A:20 vector:gpx mosaic:A:21',
     'the order the operator arranged survives');

  // Once the recording node arrives, normalise gathers its children.
  const withRec = normalise([{ id: 'rec:A', kind: 'recording', dataset: 'A', parent: '' }, ...m]);
  ok(withRec.map(l => l.id).join(' ')
     === 'rec:A mosaic:A:20 mosaic:A:21 raster:dtm vector:gpx',
     'children gather under their recording');
  ok(buildTree(withRec)[0].children.length === 2, 'and the tree sees them');
  ok(migrate([null, { id: 'x' }, { kind: 'raster' }]).length === 0, 'junk is dropped');
});

group('migrating the first schema', () => {
  // One old project had a GeoTIFF layer written as kind "geotiff", which the
  // draw order does not know about, so it silently stopped being drawn.
  const old = migrate([
    { id: 'merged-dtm', kind: 'geotiff', visible: true, label: 'DTM' },
    { id: 'lines', kind: 'gpx', visible: true, label: 'lines.gpx' },
  ]);
  ok(old[0].kind === 'raster', 'a geotiff layer becomes a raster');
  ok(old[1].kind === 'vector', 'and a gpx layer becomes a vector');
  ok(drawOrder(old).tiles.length === 1 && drawOrder(old).vectors.length === 1,
     'and both are drawn again');
});

group('adopting a project', () => {
  // Creating a project and ticking a recording into it, in one dialog: the
  // recording exists only in the browser until the debounced save lands, and
  // the copy on disk is still the empty one the create call wrote. Re-reading
  // it discarded the recording and then saved the discard.
  const inHand = { name: 'survey-1', datasets: [{ name: 'A' }], layers: [{ id: 'rec:A' }] };
  const onDisk = { name: 'survey-1', datasets: [], layers: [] };
  ok(shouldAdopt(inHand, onDisk) === false,
     'the server copy of the project already open is not adopted over it');
  ok(shouldAdopt(inHand, { name: 'survey-2', datasets: [] }) === true,
     'a different project is');
  ok(shouldAdopt(null, onDisk) === true, 'and so is the first one, at boot');
  ok(shouldAdopt(inHand, null) === false, 'no project means nothing to adopt');
});

group('draw order', () => {
  const list = [
    { id: 'v', kind: 'vector', visible: true, parent: '' },
    { id: 'rec', kind: 'recording', visible: true, parent: '' },
    { id: 'm', kind: 'mosaic', visible: true, parent: 'rec' },
    { id: 't', kind: 'track', visible: true, parent: 'rec' },
    { id: 'r', kind: 'raster', visible: true, parent: '' },
    { id: 'hidden', kind: 'raster', visible: false, parent: '' },
  ];
  const { tiles, vectors } = drawOrder(list);
  // Bottom of the stack is painted first, so the list comes back reversed.
  ok(tiles.map(l => l.id).join('') === 'rm', 'tiles bottom-first, hidden dropped');
  ok(vectors.map(l => l.id).join('') === 'tv', 'vectors bottom-first');
  ok(!tiles.concat(vectors).some(l => l.kind === 'recording'),
     'a recording is a container, not something drawn');

  // Unticking a container takes its contents with it: a checkbox that
  // switches a recording off and leaves its mosaic on screen is lying.
  const hidden = list.map(l => (l.id === 'rec' ? { ...l, visible: false } : l));
  const off = drawOrder(hidden);
  ok(!off.tiles.some(l => l.id === 'm'), 'a hidden recording takes its mosaic with it');
  ok(!off.vectors.some(l => l.id === 't'), 'and its track');
  ok(off.tiles.some(l => l.id === 'r'), 'without touching anything outside it');
  ok(isVisible(hidden, hidden.find(l => l.id === 'm')) === false, 'isVisible agrees');
  ok(isVisible(list, list.find(l => l.id === 'm')) === true, 'and says so when it is on');

  // An orphan is promoted by buildTree, so it must draw rather than vanish.
  const orphan = [{ id: 'x', kind: 'mosaic', visible: true, parent: 'gone' }];
  ok(drawOrder(orphan).tiles.length === 1, 'a layer whose parent is missing still draws');

  // A parent chain that loops cannot hang the render.
  const loop = [
    { id: 'p', kind: 'recording', visible: true, parent: 'q' },
    { id: 'q', kind: 'recording', visible: true, parent: 'p' },
    { id: 'z', kind: 'mosaic', visible: true, parent: 'p' },
  ];
  ok(drawOrder(loop).tiles.length === 1, 'a cycle in a hand-edited project does not spin');

  // The report brings its own selection and borrows the tree's order.
  const chosen = drawOrder(hidden, l => l.id === 'm' || l.id === 't' || l.id === 'r');
  ok(chosen.tiles.map(l => l.id).join('') === 'rm',
     'an explicit selection ignores what is ticked, and keeps the stacking');
  ok(chosen.vectors.map(l => l.id).join('') === 't', 'vectors likewise');

  const loaded = new Map([['A', {}]]);
  ok(isReady({ kind: 'mosaic', dataset: 'A' }, loaded) === true, 'loaded recording is ready');
  ok(isReady({ kind: 'mosaic', dataset: 'B' }, loaded) === false, 'unloaded is not');
  ok(isReady({ kind: 'raster' }, loaded) === true, 'an imported file needs nothing');
});

// ---- the waterfall's virtual image -----------------------------------------

group('waterfall geometry', () => {
  const wf = new WaterfallView(fakeCanvas(512, 600));
  wf.aspectMode = 'fit';               // one canvas row per image row * scale
  wf.stride = 1;
  wf.totalPings = 8192;
  const B = 2048;
  for (const start of [0, B, 2 * B]) {
    wf.addBlock(fakeBlock({ start, count: B, stride: 1, rows: 8192 }));
  }
  ok(wf.blocks.length === 3, 'three blocks held');
  ok(wf.totalRows() === 8192, 'total rows from the recording length');

  // A row must resolve to the block that actually contains it, including at
  // the seams -- the whole reason for holding several blocks at once.
  for (const g of [0, 1, 2047, 2048, 2049, 4095, 4096, 6143]) {
    const at = wf.locate(g);
    ok(at !== null, `row ${g} is in a loaded block`);
    ok(at.block.start / at.block.stride + at.local === g, `row ${g} maps back to itself`);
    ok(wf.rowAt(g).ping_row === g, `row ${g} carries its own ping ordinal`);
  }
  ok(wf.locate(6144) === null, 'a row past the loaded blocks is not invented');

  // Canvas <-> image round trip at several scroll positions.
  wf.scrollTo(1500);
  near(wf.top, 1500, 1e-9, 'scrolled to row 1500');
  for (const [px, py] of [[0, 0], [256, 100], [511, 599], [128, 12.5]]) {
    const [ix, g] = wf.toImage(px, py);
    const [bx, by] = wf.fromImage(ix, g);
    near(bx, px, 1e-6, `column round trip at ${px},${py}`);
    near(by, py, 1e-6, `row round trip at ${px},${py}`);
  }

  // The top of the pane is the row that was scrolled to.
  near(wf.toImage(0, 0)[1], 1500, 1e-9, 'the first visible row is the scroll position');

  // Across-track distance: the middle column is nadir, the edges are the
  // swath's half width, and the sign says which side.
  near(wf.acrossAt(512, 1500), 0, 1e-9, 'the centre column is nadir');
  near(wf.acrossAt(1024, 1500), 50, 1e-9, 'the right edge is starboard half width');
  near(wf.acrossAt(0, 1500), -50, 1e-9, 'the left edge is port half width');

  // A pixel resolves to a position, and the distance from the fish is the
  // across-track distance that same pixel reports. This is the browser-side
  // half of the invariant `tests/views.rs` checks in Rust.
  for (const ix of [100, 300, 700, 900]) {
    const r = wf.rowAt(1500);
    const across = wf.acrossAt(ix, 1500);
    const ll = geo.offset(r.fish_lat, r.fish_lon, r.bearing + 90, across);
    const d = geo.haversine([r.fish_lat, r.fish_lon], ll);
    near(d, Math.abs(across), 1e-6, `pixel ${ix} is its own across-track distance`);
  }

  // Scrolling is clamped to the recording, not to the loaded blocks: the
  // scrollbar has to span the whole thing or it lies about where you are.
  wf.scrollTo(-500);
  near(wf.top, 0, 1e-9, 'cannot scroll above the start');
  wf.scrollTo(1e9);
  near(wf.top, wf.totalRows() - wf.visibleRows(), 1e-6, 'cannot scroll past the end');

  // Dropping distant blocks must not disturb where the view is.
  const before = wf.top;
  wf.dropBlocksOutside(4096, 8192);
  ok(wf.blocks.length === 1, 'far blocks dropped');
  near(wf.top, before, 1e-9, 'the scroll position survives eviction');
});

group('waterfall at true scale', () => {
  // At true scale one image row is drawn several pixels tall, so the number of
  // rows that fit is not the number of canvas pixels. Getting this wrong made
  // the first render ask for four times the rows it could show.
  const wf = new WaterfallView(fakeCanvas(512, 600));
  wf.aspectMode = 'true';
  wf.stride = 1;
  wf.totalPings = 4096;
  // 100 m swath over 1024 px is 0.0977 m across; 0.2 m per ping along.
  wf.addBlock(fakeBlock({ start: 0, count: 2048, stride: 1, rows: 4096 }));
  const across = wf.metresPerPxAcross();
  near(across, 100 / 1024, 1e-9, 'metres per pixel across');
  near(wf.metresPerRow(), 0.2, 1e-9, 'metres per row along');
  near(wf.aspectFactor(), 0.2 / across, 1e-6, 'one pixel is taller than it is wide');
  // 512/1024 = 0.5 canvas px per image px across, times the aspect factor.
  near(wf.rowHeightPx(), 0.5 * (0.2 / across), 1e-6, 'row height in canvas pixels');
  near(wf.visibleRows(), 600 / wf.rowHeightPx(), 1e-6, 'rows that fit the pane');
  ok(wf.visibleRows() < 600, 'fewer rows fit than there are pixels, at true scale');
});

// ---- the across-track axis -------------------------------------------------

group('slant and ground range', () => {
  // These mirror `Axis::to_ground` / `Axis::from_ground` in waterfall.rs. The
  // browser owns the live path -- nothing calls the Rust pick route -- so an
  // axis fix on one side only is an axis fix on neither.
  const alt = 15;
  ok(toGround('ground', 23.4, alt) === 23.4, 'ground range is already ground range');
  ok(fromGround('ground', 23.4, alt) === 23.4, 'and inverts to itself');

  // A target 20 m away on the seabed, seen from 15 m up, is 25 m down the
  // trace. The 3-4-5 triangle is the whole of the correction.
  near(fromGround('slant', 20, alt), 25, 1e-9, 'ground 20 m is slant 25 m');
  near(toGround('slant', 25, alt), 20, 1e-9, 'and back again');
  near(fromGround('slant', -20, alt), -25, 1e-9, 'port side keeps its sign');
  near(toGround('slant', -25, alt), -20, 1e-9, 'and comes back to port');

  for (const g of [1, 5, 20, 47.5]) {
    near(toGround('slant', fromGround('slant', g, alt), alt), g, 1e-9,
      `slant round trip at ${g} m`);
  }
  // Inside the water column there is no seabed: saturate rather than go NaN.
  ok(toGround('slant', 10, alt) === 0, 'the water column has no ground range');
  ok(!Number.isNaN(toGround('slant', 0, alt)), 'and nadir is a number, not a NaN');

  // The error the bug produced: reading a slant offset as a ground distance.
  near(fromGround('slant', 20, alt) - 20, 5, 1e-9, 'five metres out at 20 m range');
});

group('the axis moves the picture, not the seabed', () => {
  const wf = new WaterfallView(fakeCanvas(512, 600));
  wf.stride = 1;
  wf.totalPings = 4096;
  wf.addBlock(fakeBlock({ start: 0, count: 512, stride: 1, rows: 4096, axis: 'slant' }));
  ok(wf.axis === 'slant', 'the view takes the axis from the block');

  // Column 768 of 1024 is three quarters out: 25 m of slant on a 50 m swath.
  const hw = 50, half = 1024 / 2;
  const d = (768 - half) / half * hw;
  near(d, 25, 1e-9, 'the ruler reads 25 m there');
  near(wf.rangeAt(768, 0), 25, 1e-9, 'and rangeAt says so');
  // ...which is 20 m of seabed, at 15 m altitude.
  near(wf.acrossAt(768, 0), 20, 1e-9, 'but the seabed is 20 m away');
  ok(wf.inWaterColumn(520, 0), 'just off nadir is water');
  ok(!wf.inWaterColumn(768, 0), 'three quarters out is not');

  // The same column on a ground image is the distance it says it is.
  const g = new WaterfallView(fakeCanvas(512, 600));
  g.stride = 1;
  g.totalPings = 4096;
  g.addBlock(fakeBlock({ start: 0, count: 512, stride: 1, rows: 4096 }));
  near(g.acrossAt(768, 0), 25, 1e-9, 'ground range needs no correction');
  ok(!g.inWaterColumn(520, 0), 'and has no water column on the axis');
});

// ---- what the report draws -------------------------------------------------

group('report outline', () => {
  // Real numbers off this survey: the JSF records ss21's 1550 kHz chirp as
  // 184-294 kHz, because the field wraps, and the server puts it back.
  const loaded = new Map([
    ['A', { name: 'A', subsystems: [20, 21],
            bands: [{ subsystem: 20, f0: 552500, f1: 607500, centre_hz: 580000 },
                    { subsystem: 21, f0: 184280, f1: 294280, centre_hz: 1550000 }] }],
    ['B', { name: 'B', subsystems: [20],
            bands: [{ subsystem: 20, f0: 552500, f1: 607500, centre_hz: 580000 }] }],
  ]);
  const layers = [
    { id: 'raster:dtm', kind: 'raster', label: 'Multibeam DTM', parent: '' },
    { id: 'rec:A', kind: 'recording', dataset: 'A', parent: '' },
    { id: mosaicId('A', 20), kind: 'mosaic', dataset: 'A', subsystem: 20, parent: 'rec:A' },
    { id: mosaicId('A', 21), kind: 'mosaic', dataset: 'A', subsystem: 21, parent: 'rec:A' },
    // One fish and one boat per recording, whatever the band.
    { id: trackId('A', 'fish'), kind: 'track', dataset: 'A', parent: 'rec:A',
      show_fish: true, show_boat: false },
    { id: trackId('A', 'boat'), kind: 'track', dataset: 'A', parent: 'rec:A',
      show_fish: false, show_boat: true },
    { id: 'rec:B', kind: 'recording', dataset: 'B', parent: '' },
    { id: mosaicId('B', 20), kind: 'mosaic', dataset: 'B', subsystem: 20, parent: 'rec:B' },
    { id: trackId('B', 'fish'), kind: 'track', dataset: 'B', parent: 'rec:B',
      show_fish: true, show_boat: false },
  ];
  const project = { datasets: [{ name: 'A' }, { name: 'B' }] };

  const spec = buildReportSpec(project, loaded, layers);
  const ids = spec.charts.map(c => c.id);
  ok(ids.join(',') === [overviewId(20), overviewId(21),
                        datasetChartId('A', 20), datasetChartId('A', 21),
                        datasetChartId('B', 20)].join(','),
     'overviews first, then each recording band by band');

  ok(spec.basemap === 'osm' && spec.seamark === true && spec.contacts === true,
     'the chart is on OpenStreetMap with seamarks, by default');

  // Two bands, so they get named rather than numbered.
  ok(spec.charts[0].title === 'Coverage — Low frequency',
     'ss20 at 580 kHz is the low band, whatever its number');
  ok(spec.charts[1].title === 'Coverage — High frequency', 'and ss21 at 1550 kHz is the high one');
  ok(spec.charts[0].subtitle === '580 kHz · 55 kHz sweep · ss20', 'the label carries both');
  ok(spec.charts[1].subtitle === '1550 kHz · 110 kHz sweep · ss21',
     'the wrapped band reads as what it transmits, not as what the ping said');
  ok(bandName([{ subsystem: 20, f0: 0, f1: 0, centre_hz: 0 }], 20).title === 'Channel ss20',
     'a format with no frequency falls back to the number');

  const ov = spec.charts[0];
  const layerIds = ov.layers.map(l => l.id);
  ok(layerIds.includes(mosaicId('A', 20)) && layerIds.includes(mosaicId('B', 20)),
     'an overview carries every recording at that band');
  ok(!layerIds.includes(mosaicId('A', 21)), 'and nothing from the other band');
  ok(ov.layers.find(l => l.id === mosaicId('A', 20)).on === true, 'the mosaic is on');
  // A track is one line now, so which line is the layer's to say and the chart
  // only chooses whether to draw it. An overview answers "where did we go", so
  // it takes the boat's line -- the one with a GPS behind it.
  ok(ov.layers.find(l => l.id === trackId('A', 'boat')).on === true,
     'an overview shows the boat track');
  ok(ov.layers.find(l => l.id === trackId('A', 'fish')).on === false,
     'and not the fish track');
  ok(ov.layers.filter(l => l.id.startsWith('track:A')).length === 2,
     'both are offered, so either can be switched on');
  // A recording's own chart is about the imagery, and the fish is what shot it.
  const ds = spec.charts[2].layers;
  ok(ds.find(l => l.id === trackId('A', 'fish')).on === true,
     "a recording's own chart shows the fish track");
  ok(ds.find(l => l.id === trackId('A', 'boat')).on === false, 'and not the boat');
  ok(ds.find(l => l.id === trackId('A', 'fish')).fish === true
     && ds.find(l => l.id === trackId('A', 'fish')).boat === false,
     'the fish layer draws the fish line, whichever chart it is in');
  // A track belongs to the recording, not to a band, so it reaches both bands'
  // charts rather than only the one whose number it happens to carry.
  ok(spec.charts[3].layers.some(l => l.id === trackId('A', 'fish')),
     "and it is in the other band's chart too");
  ok(ov.layers.find(l => l.id === 'raster:dtm').on === false,
     'an imported grid is off until it is asked for');

  // A recording that is in the project but not read cannot be drawn.
  const half = buildReportSpec(project, new Map([['A', loaded.get('A')]]), layers);
  ok(!half.charts.some(c => c.dataset === 'B'), 'an unloaded recording gets no chart');

  // Choices survive, and only choices about things that still exist.
  spec.charts[0].enabled = false;
  spec.charts[0].layers.find(l => l.id === 'raster:dtm').on = true;
  spec.charts[0].layers.find(l => l.id === trackId('A', 'fish')).on = true;
  const again = buildReportSpec({ ...project, report: spec }, loaded, layers);
  ok(again.charts[0].enabled === false, 'a chart switched off stays off');
  ok(again.charts[0].layers.find(l => l.id === 'raster:dtm').on === true,
     'a grid ticked on stays on');
  ok(again.charts[0].layers.find(l => l.id === trackId('A', 'fish')).on === true,
     'and so does a track switched on');

  // A new import arrives with the default, not with the neighbour's answer.
  const more = buildReportSpec({ ...project, report: spec }, loaded,
    [...layers, { id: 'vector:lines', kind: 'vector', label: 'lines.gpx', parent: '' }]);
  ok(more.charts[0].layers.find(l => l.id === 'vector:lines').on === false,
     'a newly imported file arrives off');
  ok(more.charts[0].layers.find(l => l.id === 'raster:dtm').on === true,
     'without disturbing what was already chosen');

  // And a layer that has gone stops being offered.
  const fewer = buildReportSpec({ ...project, report: spec }, loaded,
    layers.filter(l => l.id !== 'raster:dtm'));
  ok(!fewer.charts[0].layers.some(l => l.id === 'raster:dtm'),
     'a removed layer leaves the outline');
});

group('chart frames', () => {
  // A frame shaped like its data, so nothing is letterboxed.
  const wide = { min_lat: 52.5, max_lat: 52.51, min_lon: 4.0, max_lon: 4.1 };
  const [ww, wh] = chartSize(wide);
  ok(ww === 1600 && wh < ww, 'a wide survey gets a wide frame');
  const tall = { min_lat: 52.5, max_lat: 52.6, min_lon: 4.0, max_lon: 4.001 };
  const [tw, th] = chartSize(tall);
  ok(th === 1600 && tw < th, 'a tall one gets a tall frame');
  // A single straight line has an aspect of about a hundred; clamp it.
  ok(wh >= 800 && tw >= 800, 'nothing is clamped past 2:1');
  const [sw, sh] = chartSize(null);
  ok(sw === 1600 && sh === 1600, 'no bounds means a square');
});


// ---- tiles -----------------------------------------------------------------
//
// The loading rules, which are the part of the chart that was actually broken:
// squares that went missing and never came back, and local imagery starved by
// the base map in front of it.

// The browser side of loading a tile: an object URL over the blob, and an
// Image that loads from it. `live` counts URLs handed out and not yet given
// back, which is what a leak would look like.
const objectUrls = { live: 0, made: 0 };
globalThis.URL = globalThis.URL || {};
globalThis.URL.createObjectURL = (blob) => {
  objectUrls.live++; objectUrls.made++;
  return `blob:${blob.tag}`;
};
globalThis.URL.revokeObjectURL = () => { objectUrls.live--; };
globalThis.Image = class {
  set src(v) { this._src = v; queueMicrotask(() => this.onload && this.onload()); }
  get src() { return this._src; }
};

/// A store wired to a scripted server rather than a real one.
///
/// `answer` decides what each URL does, by URL and by how many times it has
/// been asked. The store only signals that something changed; asking again is
/// the renderer's job, so `onready` here re-requests whatever is still on
/// screen, which is exactly what `draw` does.
function scriptedStore(answer) {
  const asked = [];
  const shown = new Map();   // url -> kind, the squares "in view"
  const redraws = { n: 0 };  // what `draw` would have cost
  const store = new TileStore(() => {
    redraws.n++;
    for (const [url, kind] of shown) store.want(url, kind);
  });
  globalThis.fetch = async (url) => {
    asked.push(url);
    const r = answer(url, asked.filter(u => u === url).length);
    if (r === 'boom') throw new Error('network');
    return { ok: r !== 404, status: r === 404 ? 404 : r, blob: async () => ({ tag: url }) };
  };
  /// Ask for a square and keep it on screen, so retries have somewhere to land.
  const want = (url, kind) => { shown.set(url, kind); return store.want(url, kind); };
  return { store, asked, want, redraws };
}

/// Wait for something to become true, or give up. Retries are on timers, and
/// pinning the test to their exact schedule would test the schedule.
async function until(cond, ms = 4000) {
  const stop = Date.now() + ms;
  while (Date.now() < stop) {
    if (cond()) return true;
    await new Promise(r => setTimeout(r, 15));
  }
  return cond();
}

await group('tile loading', async () => {
  // A square asked for once is not asked for again while it is on its way.
  {
    const { store, asked, want } = scriptedStore(() => 200);
    ok(want('/a', 'local') === null, 'a fresh square is not here yet');
    want('/a', 'local');
    want('/a', 'local');
    ok(asked.length === 1, 'and is only asked for once');
    await until(() => store.peek('/a'));
    ok(store.peek('/a') !== null, 'once it lands it is here');
    ok(asked.length === 1, 'and is not asked for again');
  }

  // The budgets are separate, so a cold coastline cannot starve the sonar.
  {
    const { asked, want } = scriptedStore(() => 200);
    for (let i = 0; i < 40; i++) want(`/base/${i}`, 'base');
    const base = asked.length;
    for (let i = 0; i < 40; i++) want(`/local/${i}`, 'local');
    ok(base === 4, `the base map gets its four slots (${base})`);
    ok(asked.length - base === 12,
       `and the local imagery still gets all twelve of its own (${asked.length - base})`);
  }

  // 202 means "on its way" -- not "empty", and not "broken".
  {
    const { store, asked, want } = scriptedStore((_u, nth) => (nth < 3 ? 202 : 200));
    want('/slow', 'base');
    ok(await until(() => store.peek('/slow')), 'a square answered 202 is waited for');
    ok(asked.length >= 3, `by asking again rather than giving up (${asked.length} asks)`);
  }

  // A base map the server cannot reach answers "not yet" for as long as it
  // takes, and the chart has to sit still under that. Coming back for a square
  // is the store's own business: it used to be announced as "something
  // changed", so a screen of squares that were never going to arrive repainted
  // the whole chart twenty to thirty times a second, for as long as the window
  // stayed open, with nobody touching it.
  {
    const { store, asked, want, redraws } = scriptedStore(() => 202);
    for (let i = 0; i < 30; i++) want(`/cold/${i}`, 'base');
    await until(() => false, 900);
    ok(asked.length > 30, `a pending square is asked for again (${asked.length} asks)`);
    ok(redraws.n === 0, `without repainting the chart for it (${redraws.n} repaints)`);
    ok(store.peek('/cold/0') === null, 'and there is still nothing to draw');
  }

  // The bug that made squares vanish: a failure used to be written down as
  // `error` and never revisited, so one dropped request left a hole in the
  // chart for as long as the window stayed open.
  {
    let fail = true;
    const { store, want } = scriptedStore(() => (fail ? 'boom' : 200));
    want('/blip', 'local');
    await until(() => false, 120);
    ok(store.peek('/blip') === null, 'a failed square is not drawn');
    fail = false;
    ok(await until(() => store.peek('/blip')),
       'but it is asked for again, and comes back');
  }

  // Old tiles are dropped, and their pixels with them.
  {
    const { store } = scriptedStore(() => 200);
    for (let i = 0; i < 700; i++) {
      store.want(`/t/${i}`, 'local');
      await until(() => store.peek(`/t/${i}`), 200);
    }
    const ready = [...store.map.values()].filter(e => e.state === 'ready').length;
    ok(ready <= 400, `the decoded tiles are bounded (${ready})`);
    ok(store.peek('/t/699') !== null, 'and it keeps what was drawn last');
    ok(store.peek('/t/0') === null, 'having dropped what was drawn first');
  }



  // Clearing the store while a square is still on its way. The 202 case is the
  // one that used to throw: what came back was not a tile but a note saying
  // "not yet", and it went down the same path that gives a tile's bytes back.
  {
    let hold;
    const { store, want } = scriptedStore(() => 202);
    const failures0 = failures;
    globalThis.fetch = async () => new Promise(res => {
      hold = () => res({ ok: true, status: 202, blob: async () => ({ tag: 'x' }) });
    });
    want('/inflight', 'base');
    store.clear();
    hold();
    await until(() => false, 120);
    ok(failures === failures0, 'clearing while a 202 is in flight does not throw');
  }

  // The bytes behind an evicted tile go back too. A blob outlives the image
  // pointing at it until its URL is revoked, so an eviction that only forgets
  // the entry is a leak that the cap does nothing to bound.
  {
    objectUrls.live = 0; objectUrls.made = 0;
    const { store } = scriptedStore(() => 200);
    for (let i = 0; i < 700; i++) {
      store.want(`/b/${i}`, 'local');
      await until(() => store.peek(`/b/${i}`), 200);
    }
    ok(objectUrls.made >= 700, `every tile took a blob url (${objectUrls.made})`);
    ok(objectUrls.live <= 400,
       `and evicting gave them back (${objectUrls.live} still out of ${objectUrls.made})`);
    const held = objectUrls.live;
    store.clear();
    ok(objectUrls.live === 0,
       `clearing gives back every one (${held} before, ${objectUrls.live} after)`);
  }

  // What the cap is actually protecting: decoded pixels. A screen of squares
  // still on their way holds none, and must not cost the chart the ones it has.
  {
    const { store } = scriptedStore((u) => (u.startsWith('/hang') ? 'boom' : 200));
    for (let i = 0; i < 40; i++) {
      store.want(`/have/${i}`, 'local');
      await until(() => store.peek(`/have/${i}`), 200);
    }
    for (let i = 0; i < 900; i++) store.want(`/hang/${i}`, 'base');
    await until(() => false, 200);
    ok(store.peek('/have/0') !== null,
       'a backlog of unanswered squares does not evict a drawn one');
  }
});


// ---- panning ---------------------------------------------------------------
//
// A drag is a state machine spread across four events, and the window is not
// obliged to deliver all of them. What it does when one goes missing is the
// difference between a chart that sits still and one that follows the pointer
// around on its own.

group('panning', () => {
  globalThis.document = globalThis.document || { getElementById: () => null };
  const newMap = () => {
    const c = fakeCanvas(800, 600);
    const m = new MapView(c, { centre: [52.566, 4.072], zoom: 16 });
    return { c, m, at: () => m.centre.join(',') };
  };

  // The ordinary case still works.
  {
    const { c, m, at } = newMap();
    const was = at();
    c.fire('pointerdown', { offsetX: 400, offsetY: 300, buttons: 1 });
    c.fire('pointermove', { offsetX: 300, offsetY: 300, buttons: 1 });
    ok(at() !== was, 'dragging with the button down pans the chart');
    c.fire('pointerup', { offsetX: 300, offsetY: 300, buttons: 0 });
    const held = at();
    c.fire('pointermove', { offsetX: 100, offsetY: 300, buttons: 0 });
    ok(at() === held, 'and letting go stops it');
  }

  // The window swallowed the release -- a context menu, a lost focus, a pen
  // handing over to the mouse. The chart used to keep panning from then on,
  // following the pointer with nothing held down.
  {
    const { c, m, at } = newMap();
    c.fire('pointerdown', { offsetX: 400, offsetY: 300, buttons: 1 });
    c.fire('pointermove', { offsetX: 380, offsetY: 300, buttons: 1 });
    // ...no pointerup ever arrives...
    const stranded = at();
    c.fire('pointermove', { offsetX: 100, offsetY: 300, buttons: 0 });
    c.fire('pointermove', { offsetX: 700, offsetY: 500, buttons: 0 });
    ok(at() === stranded,
       'a move with no button held does not pan, even after a lost release');
  }

  // Which is how the release goes missing in the first place: the context menu
  // takes it. A secondary button should not arm a drag at all.
  {
    const { c, m, at } = newMap();
    const was = at();
    c.fire('pointerdown', { offsetX: 400, offsetY: 300, button: 2, buttons: 2 });
    c.fire('pointermove', { offsetX: 100, offsetY: 300, buttons: 2 });
    ok(at() === was, 'the right button does not pan');
  }
});

group('off-screen band crops', () => {
  // The contact dialog draws a sonar crop for each band, and only one band is
  // ever on screen. The other comes from a block fetched into a detached view,
  // centred on the ping that has the contact abeam -- so it starts nowhere near
  // row zero, which every other test here does. `blockTop` offsets the whole
  // block by `start / stride`, and a crop taken at a row `locateWorld` reported
  // has to land back in the same block through `locate`.
  const START = 1000, COUNT = 512, TARGET = 1200, ACROSS = 20, HALF = 50;
  const wf = new WaterfallView(fakeCanvas(1024, 600));
  wf.stride = 1;
  wf.addBlock(fakeBlock({ start: START, count: COUNT, stride: 1, rows: 8192,
                          halfWidth: HALF }));

  // A point abeam of one row, out to starboard. The rows run due north, so
  // starboard is east.
  const row = [...wf.eachRow()].find(([g]) => g === TARGET)[1];
  const [, mLon] = geo.localScale(row.fish_lat);
  const at = wf.locateWorld(row.fish_lat, row.fish_lon + ACROSS / mLon);
  ok(at !== null, 'a point abeam of an off-screen block is found');
  near(at[1], TARGET + 0.5, 0.51, 'it resolves to the row it is abeam of');
  near(at[0], 1024 / 2 + (ACROSS / HALF) * (1024 / 2), 1,
       'and to the column its across-track distance puts it at');

  // The row it named has to be one this view can actually crop from.
  const found = wf.locate(at[1]);
  ok(!!found, 'the row locateWorld reported is inside a loaded block');
  ok(found.local === TARGET - START, 'and at the right offset within it');

  // Well outside the swath there is nothing to crop.
  ok(wf.locateWorld(row.fish_lat, row.fish_lon + (HALF + 30) / mLon) === null,
     'a point beyond the swath edge has no pixel');
});

await group('contact snapshot kinds', async () => {
  // Every band crop used to go up with no kind on the URL, because the client
  // only passed one through when it was 'waterfall'. The server reads a missing
  // kind as the chart crop, so the low- and high-frequency pictures were filed
  // over the chart one and the report's band panes were never filled by
  // anything. Nothing failed and nothing was logged.
  // Only the posts under test: the map groups above leave basemap tiles in
  // flight, and they land in whatever stub is installed when they resolve.
  const seen = [];
  const real = globalThis.fetch;
  globalThis.fetch = async (path) => {
    if (String(path).includes('/snapshot')) seen.push(path);
    return { ok: true, text: async () => '{}' };
  };
  try {
    for (const k of ['map', 'waterfall', 'lf', 'hf', 'wf-lf', 'wf-hf']) {
      await api.putSnapshot('C001', new Uint8Array([1, 2, 3]), k);
    }
  } finally {
    globalThis.fetch = real;
  }
  ok(seen.length === 6, 'six snapshots were posted');
  ok(!/kind=/.test(seen[0]), 'the legacy chart crop needs no kind');
  ok(/[?&]kind=waterfall$/.test(seen[1]), 'the legacy sonar crop says waterfall');
  ok(/[?&]kind=lf$/.test(seen[2]), 'the low chart crop says lf');
  ok(/[?&]kind=hf$/.test(seen[3]), 'the high chart crop says hf');
  ok(/[?&]kind=wf-lf$/.test(seen[4]), 'the low sonar crop says wf-lf');
  ok(/[?&]kind=wf-hf$/.test(seen[5]), 'the high sonar crop says wf-hf');
  ok(new Set(seen).size === 6, 'every kind goes to its own URL');
});

// ---- two bands, one tow ----------------------------------------------------

group('panes over one recording share a scale', () => {
  // The two bands of a recording run at different ranges -- 50 m and 25 m on
  // these surveys -- and at true scale the vertical scale follows the
  // across-track one. Left to themselves the panes therefore put different
  // numbers of rows on screen and slide apart the moment either is scrolled,
  // which is what made two waterfalls of one recording look like two lengths.
  const B = 2048;
  const mk = (halfWidth) => {
    const v = new WaterfallView(fakeCanvas(512, 600));
    v.aspectMode = 'true';
    v.stride = 1;
    v.totalPings = 8192;
    v.addBlock(fakeBlock({ start: 0, count: B, stride: 1, rows: 8192, halfWidth }));
    return v;
  };
  const wide = mk(50), narrow = mk(25);

  ok(wide.rowHeightPx() !== narrow.rowHeightPx(),
     'unlocked, the two bands disagree about how tall a row is');

  // Locked to the wider band, both draw the same rows in the same places.
  const ref = Math.max(wide.metresPerPxAcross(), narrow.metresPerPxAcross());
  wide.acrossRef = ref;
  narrow.acrossRef = ref;
  near(narrow.rowHeightPx(), wide.rowHeightPx(), 1e-9, 'locked, a row is the same height');
  near(narrow.visibleRows(), wide.visibleRows(), 1e-9, 'so the same rows are on screen');
  for (const g of [0, 100, 1000]) {
    near(narrow.fromImage(0, g)[1], wide.fromImage(0, g)[1], 1e-9,
         `row ${g} is at the same height in both`);
  }

  // The narrow band is drawn narrow, centred on the nadir, because that is what
  // a 25 m swath beside a 50 m one looks like.
  near(narrow.xScale(), wide.xScale() / 2, 1e-9, 'half the swath, half the width');
  near(narrow.xOffset(), (512 - 512 / 2) / 2, 1e-9, 'and centred');
  near(wide.xOffset(), 0, 1e-9, 'the widest band fills its pane');
  near(narrow.fromImage(narrow.imageWidth / 2, 0)[0], 256, 1e-9, 'nadir stays at the centre');
  near(wide.fromImage(wide.imageWidth / 2, 0)[0], 256, 1e-9, 'in both panes');

  // A metre across the seabed is the same number of screen pixels in each.
  near(narrow.metresPerCanvasPx(), wide.metresPerCanvasPx(), 1e-9,
       'a metre across is a metre across in both panes');

  // The inverse still inverts, offset and all -- a click has to land on the
  // pixel it was aimed at.
  for (const v of [wide, narrow]) {
    for (const ix of [0, 512, 1023]) {
      const [x] = v.fromImage(ix, 10);
      near(v.toImage(x, 0)[0], ix, 1e-6, `column ${ix} survives a round trip`);
    }
  }
});

// ---- one fish, one boat ----------------------------------------------------

group('one track per tow', () => {
  // Tracks used to be created per channel, each able to draw the fish line, the
  // boat line or both -- so a two-band recording drew the same line twice and
  // the tree read as though each band had a navigation of its own.
  const old = [
    { id: 'rec:A', kind: 'recording', dataset: 'A', parent: '', visible: true },
    { id: 'mosaic:A:20', kind: 'mosaic', dataset: 'A', subsystem: 20, parent: 'rec:A' },
    { id: 'track:A:20', kind: 'track', dataset: 'A', subsystem: 20, parent: 'rec:A',
      visible: true, colour: '#abcdef', show_fish: true, show_boat: false },
    { id: 'track:A:21', kind: 'track', dataset: 'A', subsystem: 21, parent: 'rec:A',
      visible: true, colour: '#abcdef', show_fish: true, show_boat: true },
  ];
  const out = oneTrackPerTow(old);
  const ids = out.map(l => l.id);
  ok(ids.includes('track:A:fish') && ids.includes('track:A:boat'), 'one fish, one boat');
  ok(!ids.includes('track:A:20') && !ids.includes('track:A:21'), 'and no per-channel tracks');
  ok(out.filter(l => l.kind === 'track').length === 2, 'exactly two, not four');

  // What was switched on stays switched on, and the pair sits where the first
  // old track sat, so the order the operator arranged survives.
  const fish = out.find(l => l.id === 'track:A:fish');
  const boat = out.find(l => l.id === 'track:A:boat');
  ok(fish.visible === true, 'the fish line was on, so it is on');
  ok(boat.visible === true, 'the boat line was on in one of them, so it is on');
  ok(fish.show_fish === true && fish.show_boat === false, 'the fish layer draws the fish');
  ok(boat.show_boat === true && boat.show_fish === false, 'the boat layer draws the boat');
  ok(fish.colour === '#abcdef', 'the colour is kept');
  ok(fish.parent === 'rec:A' && boat.parent === 'rec:A', 'both stay under the recording');
  ok(ids.indexOf('track:A:fish') === 2, 'and they take the first old track\'s place');

  // Nothing switched on stays switched off.
  const off = oneTrackPerTow([
    { id: 'track:B:20', kind: 'track', dataset: 'B', subsystem: 20, parent: 'rec:B',
      visible: false, show_fish: true, show_boat: true },
  ]);
  ok(off.every(l => l.visible === false), 'a hidden track migrates hidden');

  // Running it twice must not double the tracks -- migrate runs on every open.
  const twice = oneTrackPerTow(oneTrackPerTow(old));
  ok(twice.filter(l => l.kind === 'track').length === 2, 'migrating again changes nothing');
  ok(migrate(old).filter(l => l.kind === 'track').length === 2, 'and migrate does it too');
});

// ---- panes over different recordings --------------------------------------

group('matching panes by time', () => {
  // Two channels of one recording share a row space, so a pane is put where
  // the other one is and that is that. Two *recordings* do not: they were
  // started at different moments and ping at their own rate, so the panes are
  // matched through time instead. `fakeBlock` spaces its rows 0.1 s apart.
  const B = 2048;
  const a = new WaterfallView(fakeCanvas(512, 600));
  a.aspectMode = 'fit'; a.stride = 1; a.totalPings = 8192;
  a.addBlock(fakeBlock({ start: 0, count: B, stride: 1, rows: 8192 }));

  const t0 = 1_700_000_000;
  near(a.timeAt(0), t0, 1e-6, 'the first row is at the recording start');
  near(a.timeAt(1000), t0 + 100, 1e-6, 'a loaded row reads its own time');
  near(a.rowAtTime(t0 + 100), 1000, 1e-6, 'and the time reads back to the row');

  // Outside the loaded block the answer is an extrapolation, not a refusal:
  // the ping rate is what it is, and a pane whose blocks have not arrived yet
  // still has to be put somewhere.
  near(a.timeAt(5000), t0 + 500, 1e-6, 'time past the loaded block');
  // A thousandth of a ping, not exact: a time is epoch seconds, and 1.7e9 of
  // them leaves a double about a microsecond of resolution to difference with.
  // That is four zeroes below anything the sonar can tell apart.
  const ROW = 1e-3;
  near(a.rowAtTime(t0 + 500), 5000, ROW, 'row past the loaded block');
  for (const g of [0, 37, 2047, 6000]) {
    near(a.rowAtTime(a.timeAt(g)), g, ROW, `row ${g} survives a round trip`);
  }

  // A second recording whose blocks start further in: the same instant is a
  // different row number, which is the whole point of going through time.
  const b = new WaterfallView(fakeCanvas(512, 600));
  b.aspectMode = 'fit'; b.stride = 1; b.totalPings = 8192;
  b.addBlock(fakeBlock({ start: 2 * B, count: B, stride: 1, rows: 8192 }));
  const t = a.timeAt(4096);
  near(b.rowAtTime(t), 4096, ROW, 'the same instant, found from a later block');
  near(b.timeAt(0), t0, 1e-6, 'and the mapping reaches back before the loaded block');

  // Nothing loaded is nothing known. Guessing here would scroll a pane to a
  // place it has no reason to believe in.
  const empty = new WaterfallView(fakeCanvas(512, 600));
  ok(empty.timeAt(10) === null, 'no blocks, no time');
  ok(empty.rowAtTime(t0) === null, 'no blocks, no row');
});

// ---- the dropdown ----------------------------------------------------------

group('stepping a dropdown', () => {
  // Its own popup is drawn because the native one opened in the wrong place;
  // moving the highlight is then ours to get right too. A native dropdown skips
  // what cannot be chosen and stops at the ends rather than wrapping.
  const opts = (...d) => d.map(x => ({ disabled: x }));
  const plain = opts(false, false, false);
  ok(nextEnabled(plain, 0, 1) === 1, 'down moves on');
  ok(nextEnabled(plain, 2, 1) === 2, 'down at the end stays');
  ok(nextEnabled(plain, 0, -1) === 0, 'up at the start stays');
  ok(nextEnabled(plain, 2, -1) === 1, 'up moves back');

  const gappy = opts(false, true, true, false);
  ok(nextEnabled(gappy, 0, 1) === 3, 'a run of disabled options is stepped over');
  ok(nextEnabled(gappy, 3, -1) === 0, 'and stepped back over');

  // Nothing choosable at all, and a selection sitting on a disabled option:
  // both have to end somewhere rather than on an index that cannot be picked.
  ok(nextEnabled(opts(true, true), 0, 1) === -1, 'a list of nothing choosable gives up');
  ok(nextEnabled([], 0, 1) === -1, 'so does an empty list');
  ok(nextEnabled(opts(false, true), 1, 1) === -1, 'and so does a dead end on a disabled row');
});

// ---- the waterfall scrollbar ----------------------------------------------

group('scrollbar direction', () => {
  // A vertical range input runs one way under `writing-mode: vertical-lr` and
  // the other under WebKit's `slider-vertical`, which is why the same scrollbar
  // scrolled forwards in the browser and backwards in the desktop shell.
  const max = 1000;

  // Minimum at the top: the value is the row, untouched.
  ok(sliderRow(0, max, true) === 0, 'top of a downward slider is the start');
  ok(sliderRow(max, max, true) === max, 'bottom of a downward slider is the end');
  ok(sliderRow(250, max, true) === 250, 'a downward slider needs no conversion');

  // Minimum at the bottom: the value counts the other way.
  ok(sliderRow(0, max, false) === max, 'bottom of an upward slider is the end');
  ok(sliderRow(max, max, false) === 0, 'top of an upward slider is the start');
  ok(sliderRow(250, max, false) === 750, 'an upward slider counts back');

  // The same function does both directions, so a round trip has to land where
  // it started or the scrollbar would drift every time it was touched.
  for (const down of [true, false]) {
    for (const row of [0, 1, 37, 999, 1000]) {
      ok(sliderRow(sliderRow(row, max, down), max, down) === row,
         `row ${row} survives a round trip (down=${down})`);
    }
  }

  // Dragging towards the bottom of the pane always goes forward through the
  // recording, whichever way the engine draws the slider.
  for (const down of [true, false]) {
    const near = sliderRow(down ? 100 : max - 100, max, down);
    const far = sliderRow(down ? 900 : max - 900, max, down);
    ok(near < far, `lower on screen is later in the recording (down=${down})`);
  }
});


console.log(`\n${checks - failures}/${checks} checks passed`);
process.exit(failures ? 1 : 0);
