// The project tree, without the DOM.
//
// One list holds everything the chart draws, in draw order, first on top:
// recordings with their mosaics and tracks nested under them, and imported
// files alongside. It is stored flat, as a pre-order flattening -- a node's
// children follow it contiguously -- because that is what round-trips through
// the project file without a schema for trees, and because the flat order *is*
// the draw order, so drawing needs no traversal at all.
//
// The ordering arithmetic is the part most likely to be quietly wrong -- an
// index that is off by one only when you drag downwards is easy to write and
// hard to notice -- so it lives here, where it can be tested without a window.

export const RECORDING = 'recording';
export const MOSAIC = 'mosaic';
export const TRACK = 'track';
export const RASTER = 'raster';
export const VECTOR = 'vector';

/// Nodes that are drawn as tiles, in the order the chart should paint them.
export const TILED = new Set([MOSAIC, RASTER]);

export function recordingId(dataset) { return `rec:${dataset}`; }
export function mosaicId(dataset, subsystem) { return `mosaic:${dataset}:${subsystem}`; }
/// `which` is 'fish' or 'boat'. Not a channel: there is one fish and one boat,
/// and every band was towed by the same one. A track per subsystem was the same
/// line drawn twice, and it implied the mosaics were navigated separately.
export function trackId(dataset, which) { return `track:${dataset}:${which}`; }

/// Group the flat list into `{ layer, children }`, keeping order.
///
/// A child whose parent is missing is promoted to the top rather than dropped:
/// losing a layer because its recording was unloaded is worse than showing it
/// in the wrong place, and the operator can see what happened.
export function buildTree(list) {
  const byId = new Map(list.map(l => [l.id, l]));
  const out = [];
  const nodes = new Map();
  for (const l of list) {
    if (!l.parent || !byId.has(l.parent)) {
      const n = { layer: l, children: [] };
      nodes.set(l.id, n);
      out.push(n);
    }
  }
  for (const l of list) {
    if (l.parent && byId.has(l.parent) && nodes.has(l.parent)) {
      nodes.get(l.parent).children.push(l);
    }
  }
  return out;
}

/// The inverse: back to one pre-order list.
export function flatten(tree) {
  const out = [];
  for (const n of tree) {
    out.push(n.layer);
    for (const c of n.children) out.push(c);
  }
  return out;
}

/// Put the list back into pre-order: children contiguous after their parent.
///
/// Anything that inserts a node has to leave the list in this shape, because
/// the flat order is what the chart paints and what the project file stores.
export function normalise(list) {
  return flatten(buildTree(list));
}

/// Bring a list saved before the tree existed up to date.
///
/// Projects written by the flat-list version have mosaics at the top level with
/// no parent. Attaching them to their recording is better than dropping them:
/// the operator arranged that order, and losing it silently would be the worst
/// of the three outcomes.
/// What the first schema called each kind of imported layer.
const OLD_KIND = { geotiff: RASTER, tiff: RASTER, raster: RASTER, gpx: VECTOR };

export function migrate(list) {
  const out = list
    .filter(l => l && l.id && l.kind)
    .map(l => ({ ...l, kind: OLD_KIND[l.kind] || l.kind, parent: l.parent || '' }));
  for (const l of out) {
    if ((l.kind === MOSAIC || l.kind === TRACK) && !l.parent && l.dataset) {
      l.parent = recordingId(l.dataset);
      // The old panel wrote the label itself, so replacing that one is a
      // rename of something the operator never chose. A label they did choose
      // is left alone.
      if (l.kind === MOSAIC && l.label === `${l.dataset} · ss${l.subsystem}`) {
        l.label = `ss${l.subsystem} mosaic`;
      }
    }
  }
  return normalise(oneTrackPerTow(out));
}

/// Fold per-channel track layers into one fish track and one boat track.
///
/// A track used to be created per subsystem, each carrying a `show_fish` and a
/// `show_boat` flag, so a two-band recording drew the same line twice and the
/// tree read as though each band had a navigation of its own. It does not: one
/// fish, one boat, and the mosaic of every band is painted from the same fixes.
///
/// What was switched on stays switched on. The new pair takes the place of the
/// first old track, so the draw order the operator arranged is kept.
export function oneTrackPerTow(list) {
  const olds = list.filter(l => l.kind === TRACK && l.dataset
                                && l.id !== trackId(l.dataset, 'fish')
                                && l.id !== trackId(l.dataset, 'boat'));
  if (!olds.length) return list;

  const out = [];
  const done = new Set();
  for (const l of list) {
    if (!olds.includes(l)) { out.push(l); continue; }
    if (done.has(l.dataset)) continue;         // the second channel's copy
    done.add(l.dataset);
    const mine = olds.filter(x => x.dataset === l.dataset);
    const on = (f) => mine.some(x => x.visible !== false && f(x));
    for (const which of ['fish', 'boat']) {
      const id = trackId(l.dataset, which);
      if (list.some(x => x.id === id)) continue;   // already migrated
      out.push({
        ...l,
        id,
        label: which === 'fish' ? 'fish track' : 'boat track',
        subsystem: null,
        show_fish: which === 'fish',
        show_boat: which === 'boat',
        visible: which === 'fish'
          ? on(x => x.show_fish !== false)
          : on(x => !!x.show_boat),
      });
    }
  }
  return out;
}

/// Move `id` to position `at` among its own siblings, where `at` counts the
/// siblings as they stand *before* the move.
///
/// That is the number a drop naturally produces: the pointer is between two of
/// the rows currently on screen. Taking the row out first shifts everything
/// below it up by one, so a downward move has to be corrected; an upward one
/// does not. A node never changes parent by dragging -- a mosaic belongs to its
/// recording, and letting a drag say otherwise would only ever be a mistake.
export function moveWithinSiblings(list, id, at) {
  const node = list.find(l => l.id === id);
  if (!node) return { list, moved: false, to: null };
  const tree = buildTree(list);

  // Top-level move: the node and its children travel together.
  if (!node.parent) {
    const from = tree.findIndex(n => n.layer.id === id);
    if (from < 0) return { list, moved: false, to: null };
    const to = at > from ? at - 1 : at;
    if (to === from || to < 0) return { list, moved: false, to: from };
    const next = tree.slice();
    const [moving] = next.splice(from, 1);
    next.splice(Math.min(to, next.length), 0, moving);
    return { list: flatten(next), moved: true, to };
  }

  // Child move: within its own parent only.
  const parent = tree.find(n => n.layer.id === node.parent);
  if (!parent) return { list, moved: false, to: null };
  const from = parent.children.findIndex(c => c.id === id);
  const to = at > from ? at - 1 : at;
  if (to === from || to < 0) return { list, moved: false, to: from };
  const kids = parent.children.slice();
  const [moving] = kids.splice(from, 1);
  kids.splice(Math.min(to, kids.length), 0, moving);
  const next = tree.map(n => (n === parent ? { layer: n.layer, children: kids } : n));
  return { list: flatten(next), moved: true, to };
}

/// Where a drop at `y` lands, given the vertical midpoints of the rows a drag
/// may land between.
export function dropIndex(mids, y) {
  for (let i = 0; i < mids.length; i++) if (y < mids[i]) return i;
  return mids.length;
}

/// Remove a node and everything under it.
export function removeNode(list, id) {
  return list.filter(l => l.id !== id && l.parent !== id);
}

/// Should the layer stack be rebuilt from this project?
///
/// Only when it is a different project. For the one already open the browser
/// holds edits the server has not seen yet -- the save is debounced -- and
/// adopting the server's copy over them discards them. A project created and
/// populated in one dialog lost every recording that way: the copy on disk was
/// still the empty one the create call had written.
export function shouldAdopt(current, incoming) {
  if (!incoming?.name) return false;
  return current?.name !== incoming.name;
}

/// Is this layer shown, taking its ancestors into account?
///
/// The checkbox on a row means "display", and unticking a recording has to take
/// its mosaics and tracks with it -- a container you can switch off that leaves
/// its contents on screen is a checkbox that lies. A missing parent counts as
/// visible, matching `buildTree`: an orphan is promoted, not hidden.
export function isVisible(list, layer) {
  const byId = list instanceof Map ? list : new Map(list.map(l => [l.id, l]));
  let l = layer;
  // Bounded by the list length so a parent cycle in a hand-edited project file
  // cannot spin here.
  for (let depth = 0; l && depth <= byId.size; depth++) {
    if (l.visible === false) return false;
    if (!l.parent) return true;
    l = byId.get(l.parent);
  }
  return true;
}

/// Everything the chart should draw, bottom of the stack first.
///
/// Tiles first, then vectors over them: a track drawn under the imagery it
/// describes is of no use to anyone, so the stack orders like against like
/// rather than pretending one list can decide both.
/// `keep` selects which layers are in the stack; it defaults to what the tree
/// has switched on. The report passes its own, because a chart in a deliverable
/// is chosen rather than inherited -- but it wants the same *order*, so the
/// picture and the tree cannot disagree about what is on top of what.
export function drawOrder(list, keep) {
  const byId = new Map(list.map(l => [l.id, l]));
  const want = keep || ((l) => isVisible(byId, l));
  const drawable = list.filter(l => l.kind !== RECORDING && want(l));
  const tiles = drawable.filter(l => TILED.has(l.kind));
  const vectors = drawable.filter(l => !TILED.has(l.kind));
  return { tiles: tiles.slice().reverse(), vectors: vectors.slice().reverse() };
}

/// A node is only drawable when whatever it depends on is present. A mosaic
/// whose recording is not loaded stays in the tree -- the order the operator
/// arranged has to survive a reload -- but it is not asked for.
export function isReady(layer, loaded) {
  if (layer.kind === RECORDING) return loaded.has(layer.dataset);
  if (layer.kind === MOSAIC || layer.kind === TRACK) return loaded.has(layer.dataset);
  return true;
}
