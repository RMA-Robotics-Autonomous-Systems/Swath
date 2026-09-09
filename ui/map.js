// The chart view.
//
// A canvas rather than a map library, for one reason: the thing being drawn is
// measured imagery, and the operator has to be able to trust the scale bar and
// the position readout. Owning the projection means there is exactly one place
// where a pixel becomes a position, and the waterfall's inverse agrees with it
// by construction rather than by luck.

import { TILE, lonLatToPx, pxToLonLat, metresPerPx, niceDistance, formatDistance,
         haversine, bearing } from './geo.js';
import { tileUrl } from './api.js';
import { contactColour } from './contacts.js';

const MAX_Z = 21;
const MIN_Z = 3;
/// Any zoom works as the reference for a fit: the pixel span of a rectangle
/// doubles per zoom, so the solve is the same wherever it starts.
const REF_Z = 16;

/// The tile URL for one layer of the stack.
function urlFor(l) {
  return l.kind === 'raster'
    ? (z, x, y) => tileUrl.layer(l.id, z, x, y, l.style)
    : (z, x, y) => tileUrl.mosaic(l.dataset, l.subsystem, z, x, y, l.rev, l.style);
}

/// How many tile requests may be in flight at once, and of what.
///
/// Two budgets rather than one, because the two kinds of tile have nothing in
/// common but the shape. A mosaic square is made from memory in about two
/// milliseconds; a base map square that is not on disk yet is a request to
/// somebody else's server. Sharing one counter meant a cold coastline used the
/// whole allowance and the sonar underneath it -- the thing the operator
/// actually came to look at -- was skipped, silently, with no redraw queued to
/// come back for it.
const BUDGET = { local: 12, base: 4 };

/// Tiles kept, oldest use dropped first.
///
/// Held as images rather than as `ImageBitmap`s, and that is the whole point.
/// A bitmap is a quarter of a megabyte of decoded pixels that the browser is
/// not allowed to reclaim; six hundred of them measured at 157 MB pinned, and
/// eight seconds of panning allocated close to a gigabyte churning through
/// them. On a machine with its swap already full that is not a cache, it is a
/// way of being killed. An image hands the pixels back to the browser, which
/// may drop them under pressure and decode them again from the blob -- which
/// is a tenth of the size and is what we are actually paying to keep.
const TILE_CACHE = 400;

/// How far up the pyramid to look for something to draw in the meantime.
///
/// Five levels covers a wheel spin from one end of the range to the other. The
/// ancestor is blurry, and it is enormously better than black.
const FALLBACK_DEPTH = 5;

/// A square that has been asked for and not answered is asked again.
const RETRY_MS = 280;
const RETRY_MAX_MS = 4000;
/// After this many refusals the square is rested rather than hammered -- but it
/// is never given up on, which is what "error" used to mean here.
const RETRY_TRIES = 10;
const REST_MS = 30000;

/// Give a tile's bytes back.
///
/// The blob behind an object URL lives until the URL is revoked, whatever the
/// garbage collector thinks of the image pointing at it. Dropping the entry is
/// not enough; this is the other half.
function release(e) {
  // Guarded, because one caller hands this whatever the fetch resolved to, and
  // that is the string 'pending' when the server said "not yet". Modules are
  // strict mode, so writing a property onto a string throws rather than being
  // quietly ignored -- and it would throw inside a promise handler, where it
  // would have gone unseen except as a tile that never arrived.
  if (!e || typeof e !== 'object') return;
  if (e.href) {
    URL.revokeObjectURL(e.href);
    e.href = null;
  }
  e.img = null;
}

/// The decoded tiles, and the rules about asking for more of them.
///
/// Everything the renderer knows about loading lives here, so `draw` can be
/// what it wants to be: a pass over the layer stack that paints whatever is
/// ready and says so when something new arrives.
export class TileStore {
  constructor(onready) {
    this.map = new Map();   // url -> entry
    this.onready = onready || (() => {});
    this.live = { local: 0, base: 0 };
    this.clock = 0;
    this.generation = 0;
    // The squares the last frame asked for, url -> kind. What the store comes
    // back to when a slot frees or a square falls due for another try, so
    // asking again does not have to go through a repaint to find out what is
    // on screen.
    this.wanted = new Map();
  }

  /// A new pass over the layer stack is starting.
  ///
  /// `draw` announces itself so the store can rebuild the set of squares that
  /// are actually on screen; `want` fills it in as the pass goes.
  beginFrame() {
    this.wanted.clear();
  }

  /// Forget every square.
  ///
  /// For when the imagery itself changed under a URL the browser would
  /// otherwise consider settled -- a repaint after the navigation or the speed
  /// of sound moved. Loads already in flight are let run and their results
  /// discarded, because cancelling them is not worth a generation check on
  /// every frame; `live` is left alone so their slots come back when they
  /// finish.
  ///
  /// This exists because it used not to: `tiles` was a plain `Map` until the
  /// store grew around it, and the two callers of `.clear()` in the viewer went
  /// on calling it into a class that had no such method. The repaint after a
  /// settings change threw instead of dropping the old imagery, which is the
  /// most misleading possible way for it to fail -- the chart kept showing
  /// pictures painted with settings it no longer had.
  clear() {
    this.generation = (this.generation || 0) + 1;
    for (const e of this.map.values()) release(e);
    this.map.clear();
  }

  /// The image for this square if it is already decoded, without asking for it.
  ///
  /// Used for the ancestor search: looking for something to draw in the
  /// meantime should never itself start a download of a zoom nobody is on.
  peek(url) {
    const e = this.map.get(url);
    if (!e || e.state !== 'ready') return null;
    e.used = ++this.clock;
    return e.img;
  }

  /// The image for this square, requesting it if it is not here.
  ///
  /// Returns null while it is on its way; `onready` fires when that changes.
  want(url, kind) {
    this.wanted.set(url, kind);
    const e = this.map.get(url);
    if (e) {
      if (e.state === 'ready') { e.used = ++this.clock; return e.img; }
      if (e.state === 'due') this.#start(url, kind, e);
      return null;
    }
    this.#start(url, kind, null);
    return null;
  }

  /// Ask again for what is on screen and has nothing on its way.
  ///
  /// The store's own way back to the network, used when a square falls due and
  /// when a slot frees. It exists so that neither of those has to be announced
  /// as `onready` -- which repaints. A screen of squares that are not coming
  /// (a base map the server cannot reach, so every ask is answered "not yet")
  /// used to put the chart into a permanent redraw: twenty to thirty full
  /// passes a second, for as long as the window stayed open, with nobody
  /// touching it. Small projects only felt warm; a big mosaic and a wide
  /// window is a frame that does not fit in the time between two of them, and
  /// the page stops answering.
  #pump() {
    for (const [url, kind] of this.wanted) {
      if (this.live[kind] >= BUDGET[kind]) continue;
      const e = this.map.get(url);
      if (!e) this.#start(url, kind, null);
      else if (e.state === 'due') this.#start(url, kind, e);
    }
  }

  #start(url, kind, prev) {
    if (this.live[kind] >= BUDGET[kind]) {
      // No slot now. Left absent rather than recorded as anything, so the next
      // frame asks again -- and a frame is coming, because whatever is using
      // the slot will finish and say so.
      return;
    }
    const e = prev || { img: null, tries: 0, used: ++this.clock };
    e.state = 'loading';
    e.generation = this.generation || 0;
    this.map.set(url, e);
    this.live[kind]++;
    this.#fetch(url).then(
      (got) => {
        this.live[kind]--;
        // Cleared while this was on its way: the picture is of settings we no
        // longer hold, so drop it rather than putting it back on the chart.
        if (e.generation !== (this.generation || 0)) {
          release(got);
          return this.#pump();
        }
        if (got === 'pending') {
          this.#again(url, kind, e);
          // A slot has just come free; whatever else is on screen and waiting
          // for one can have it, without waiting for a frame to ask.
          return this.#pump();
        }
        e.state = 'ready';
        e.img = got.img;
        e.href = got.href;
        e.tries = 0;
        e.used = ++this.clock;
        this.#evict();
        this.onready();
      },
      () => {
        this.live[kind]--;
        if (e.generation !== (this.generation || 0)) return this.#pump();
        this.#again(url, kind, e);
        this.#pump();
      });
  }

  /// Come back for it.
  ///
  /// The old code wrote the failure down as `error` and never asked again, so a
  /// blip -- a request dropped under load, a base map square still being
  /// fetched -- left a hole in the chart for as long as the window stayed open.
  #again(url, _kind, e) {
    e.tries++;
    // Backing off, then resting -- but never giving up. A square that has
    // refused ten times in a row is probably a source that is down; it is asked
    // again half a minute later, with a fresh set of attempts, because the
    // alternative is a hole in the chart that outlives the reason for it.
    const wait = e.tries > RETRY_TRIES
      ? (e.tries = 0, REST_MS)
      : Math.min(RETRY_MAX_MS, RETRY_MS * Math.pow(1.6, e.tries - 1));
    e.state = 'waiting';
    setTimeout(() => {
      // The timer decides it is due, rather than the next caller re-reading the
      // clock. Comparing against a deadline meant a timer that fired a
      // hairsbreadth early left the square waiting with nothing scheduled to
      // wake it -- the exact fault this retry exists to fix.
      if (this.map.get(url) !== e || e.state !== 'waiting') return;
      e.state = 'due';
      // Asked again, and only for squares still on screen -- but without a
      // repaint, because nothing has changed on the chart yet. The picture
      // arriving is what repaints it.
      this.#pump();
    }, wait);
  }

  /// One square, over fetch rather than an Image element.
  ///
  /// Two reasons. The server answers a base map square it has not fetched yet
  /// with 202 rather than holding the connection, and an `<img>` cannot see a
  /// status code -- it would take the placeholder for the picture. And
  /// `createImageBitmap` decodes off the thread that draws, which for a screen
  /// of imagery is several megabytes of PNG not spent blocking the frame.
  async #fetch(url) {
    const r = await fetch(url);
    // A body that is never read is a connection that is never handed back: the
    // browser gives an origin six of them, and "not yet" is the answer that
    // arrives in the hundreds. Cancelling the stream costs nothing and is the
    // difference between the sonar loading behind a cold base map and not.
    if (r.status === 202) { await r.body?.cancel(); return 'pending'; }
    if (!r.ok) { await r.body?.cancel(); throw new Error(`${r.status}`); }
    const blob = await r.blob();
    // The object URL is kept, not revoked on load: it is the browser's copy of
    // the compressed bytes, and the reason it is allowed to throw the decoded
    // pixels away and get them back later without asking the server. Revoking
    // it here would turn every reclaimed tile into a blank square -- the exact
    // fault this store was written to end. It goes when the tile is evicted.
    const href = URL.createObjectURL(blob);
    return new Promise((res, rej) => {
      const img = new Image();
      img.onload = () => res({ img, href });
      img.onerror = () => { URL.revokeObjectURL(href); rej(new Error('decode')); };
      img.src = href;
    });
  }

  /// Drop what is not worth keeping, oldest use first.
  ///
  /// Counted by kind rather than by entry. The cap is about decoded pixels --
  /// a quarter of a megabyte a square -- and an entry that is merely waiting
  /// its turn holds none of them. Counting the two together meant a screen of
  /// squares still on their way could evict every picture on the chart.
  #evict() {
    if (this.map.size <= TILE_CACHE) return;
    const ready = [], noted = [];
    for (const [url, e] of this.map) {
      if (e.state === 'ready') ready.push([url, e]);
      else if (e.state !== 'loading') noted.push([url, e]);
    }
    const oldestFirst = (a, b) => a[1].used - b[1].used;
    if (ready.length > TILE_CACHE) {
      ready.sort(oldestFirst);
      for (const [url, e] of ready.slice(0, ready.length - TILE_CACHE)) {
        release(e);
        this.map.delete(url);
      }
    }
    // A square waiting to be asked again is a note to self, and a cheap one to
    // write out afresh. Forgetting it cancels nothing that matters: the timer
    // finds the entry gone and stops, and `draw` asks again if it is still on
    // screen.
    if (noted.length > TILE_CACHE) {
      noted.sort(oldestFirst);
      for (const [url] of noted.slice(0, noted.length - TILE_CACHE)) this.map.delete(url);
    }
  }

  /// Every square in the list, decoded, for the report.
  ///
  /// The on-screen path may draw what it has and come back for the rest. A
  /// report image may not, so this waits -- including through the 202 that says
  /// a base map square is still being fetched.
  async all(urls, timeoutMs = 15000) {
    const deadline = performance.now() + timeoutMs;
    await Promise.all([...urls].map(async (url) => {
      if (this.peek(url)) return;
      while (performance.now() < deadline) {
        try {
          const got = await this.#fetch(url);
          if (got !== 'pending') {
            this.map.set(url, { img: got.img, href: got.href, state: 'ready',
                                tries: 0, used: ++this.clock,
                                generation: this.generation || 0 });
            return;
          }
        } catch { return; }
        await new Promise(r => setTimeout(r, 300));
      }
    }));
    this.#evict();
  }
}

export class MapView {
  constructor(canvas, opts = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d', { alpha: false });
    this.centre = opts.centre || [52.56, 4.06];
    this.zoom = opts.zoom ?? 14;
    this.basemap = 'osm';
    // On by default. Offshore, OSM has nothing to draw -- the basemap is a
    // flat blue rectangle -- and the seamark overlay is the layer that
    // actually carries information: traffic separation schemes, cable and
    // pipeline routes, charted area boundaries, buoyage.
    this.seamark = true;
    // Both lists arrive bottom-of-the-stack first, ready to paint in order.
    // Tiles: { kind:'mosaic', dataset, subsystem, rev, style } or
    //        { kind:'raster', id, style }.
    this.layers = [];
    // Vectors, drawn over the tiles: { kind:'track', track, colour, show_boat,
    // show_fish } or { kind:'vector', id, colour, features }.
    this.vectors = [];
    this.contacts = [];
    // The search plan being worked on, as GeoJSON from /api/plan/solve. Not a
    // vector layer: it is redrawn on every drag of the azimuth and belongs to
    // the planner rather than to the project's layer stack. A plan that has
    // been exported and imported comes back as an ordinary vector layer.
    this.plan = null;
    this.planLayers = { cov: true, turn: true, run: true, num: true };
    this.selected = null;
    this.cursor = null;        // linked cursor from the waterfall
    this.viewSpan = null;      // fish track the waterfall is currently showing
    this.measure = null;       // { from, to }
    this.tiles = new TileStore(() => this.schedule());
    this.onmove = opts.onmove || (() => {});
    this.onclick = opts.onclick || (() => {});
    this.onhover = opts.onhover || (() => {});
    this.tool = 'pan';
    this._raf = null;
    this._bind();
    this.resize();
  }

  // ---- projection ----------------------------------------------------------

  /// Canvas pixel -> [lat, lon].
  unproject(px, py) {
    const [cx, cy] = lonLatToPx(this.centre[1], this.centre[0], this.zoom);
    const [lon, lat] = pxToLonLat(
      cx + (px - this.w / 2),
      cy + (py - this.h / 2),
      this.zoom);
    return [lat, lon];
  }

  /// [lat, lon] -> canvas pixel.
  project(lat, lon) {
    const [cx, cy] = lonLatToPx(this.centre[1], this.centre[0], this.zoom);
    const [x, y] = lonLatToPx(lon, lat, this.zoom);
    return [x - cx + this.w / 2, y - cy + this.h / 2];
  }

  metresPerPixel() { return metresPerPx(this.centre[0], this.zoom); }

  resize() {
    const dpr = window.devicePixelRatio || 1;
    const r = this.canvas.getBoundingClientRect();
    this.w = Math.max(1, Math.round(r.width));
    this.h = Math.max(1, Math.round(r.height));
    this.canvas.width = this.w * dpr;
    this.canvas.height = this.h * dpr;
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.draw();
  }

  fit(bounds, pad = 0.12) {
    if (!bounds) return;
    const { min_lat, min_lon, max_lat, max_lon } = bounds;
    this.centre = [(min_lat + max_lat) / 2, (min_lon + max_lon) / 2];
    for (let z = MAX_Z; z >= MIN_Z; z--) {
      const a = lonLatToPx(min_lon, max_lat, z);
      const b = lonLatToPx(max_lon, min_lat, z);
      if (Math.abs(b[0] - a[0]) < this.w * (1 - pad) &&
          Math.abs(b[1] - a[1]) < this.h * (1 - pad)) { this.zoom = z; break; }
    }
    this.draw();
  }

  flyTo(lat, lon, zoom) {
    this.centre = [lat, lon];
    if (zoom) this.zoom = Math.min(MAX_Z, Math.max(MIN_Z, zoom));
    this.draw();
  }

  // ---- tiles ---------------------------------------------------------------

  drawTileLayer(z, urlFor, alpha, kind = 'local') {
    const ctx = this.ctx;
    const [cx, cy] = lonLatToPx(this.centre[1], this.centre[0], this.zoom);
    const scale = Math.pow(2, this.zoom - z);
    const size = TILE * scale;
    const left = cx - this.w / 2, top = cy - this.h / 2;
    const x0 = Math.floor(left / size), x1 = Math.floor((left + this.w) / size);
    const y0 = Math.floor(top / size), y1 = Math.floor((top + this.h) / size);
    const n = 1 << z;
    ctx.globalAlpha = alpha;
    for (let ty = y0; ty <= y1; ty++) {
      if (ty < 0 || ty >= n) continue;
      for (let tx = x0; tx <= x1; tx++) {
        const wx = ((tx % n) + n) % n;
        const dx = Math.round(tx * size - left), dy = Math.round(ty * size - top);
        const dw = Math.ceil(size), dh = Math.ceil(size);
        const img = this.tiles.want(urlFor(z, wx, ty), kind);
        if (img) { ctx.drawImage(img, dx, dy, dw, dh); continue; }
        this.drawAncestor(z, wx, ty, urlFor, dx, dy, dw, dh);
      }
    }
    ctx.globalAlpha = 1;
  }

  /// Draw the piece of a coarser tile that covers this square.
  ///
  /// Zoom is rounded to pick the tile level, so every second notch of the wheel
  /// changes it and the entire stack becomes a set of squares nobody has yet.
  /// Clearing to black and waiting is what made a zoom look like a fault. The
  /// ancestor is already decoded and already covers the ground; scaled up it is
  /// soft for a moment and then it is replaced.
  drawAncestor(z, x, y, urlFor, dx, dy, dw, dh) {
    for (let k = 1; k <= FALLBACK_DEPTH && k <= z; k++) {
      const ax = x >> k, ay = y >> k;
      const img = this.tiles.peek(urlFor(z - k, ax, ay));
      if (!img) continue;
      const part = TILE >> k;
      // Below one source pixel there is nothing left to say.
      if (part < 1) return;
      this.ctx.drawImage(img,
        (x - (ax << k)) * part, (y - (ay << k)) * part, part, part,
        dx, dy, dw, dh);
      return;
    }
  }

  // ---- drawing -------------------------------------------------------------

  schedule() {
    if (this._raf) return;
    this._raf = requestAnimationFrame(() => { this._raf = null; this.draw(); });
  }

  draw() {
    const ctx = this.ctx;
    // What this pass asks for is the set of squares that are on screen, and
    // the store keeps it: it is how a retry finds its way back to the network
    // without a repaint.
    this.tiles.beginFrame();
    ctx.fillStyle = '#0a0d10';
    ctx.fillRect(0, 0, this.w, this.h);

    const z = Math.min(19, Math.max(0, Math.round(this.zoom)));
    if (this.basemap !== 'none') {
      this.drawTileLayer(z, (z, x, y) => tileUrl.base(this.basemap, z, x, y), 0.78, 'base');
    }
    const lz = Math.min(20, Math.max(0, Math.round(this.zoom)));
    for (const l of this.layers) this.drawTileLayer(lz, urlFor(l), l.opacity ?? 1);
    if (this.seamark) {
      // OpenSeaMap's own tiles stop being useful past z18; beyond that the
      // z18 tile is scaled up rather than fetching empties.
      this.drawTileLayer(Math.min(z, 18), (z, x, y) => tileUrl.base('seamark', z, x, y), 0.9, 'base');
    }

    // Vectors over the tiles, always: a track drawn under the imagery it
    // describes is of no use to anyone, so the stack orders like against like
    // rather than pretending one list can decide both.
    for (const l of this.vectors) {
      if (l.kind === 'track') this.drawTrack(l);
      else this.drawVector(l);
    }
    if (this.viewSpan) this.drawViewSpan();
    if (this.plan) this.drawPlan();
    this.drawContacts();
    if (this.measure) this.drawMeasure();
    if (this.cursor) this.drawCursor();
    this.drawScale();
  }

  drawTrack(t) {
    if (!t.track) return;
    const ctx = this.ctx;
    const line = (pts, colour, width, dash) => {
      if (!pts || pts.length < 2) return;
      ctx.save();
      ctx.strokeStyle = colour;
      ctx.lineWidth = width;
      ctx.lineJoin = 'round';
      ctx.lineCap = 'round';
      if (dash) ctx.setLineDash(dash);
      ctx.beginPath();
      let pen = false;
      for (const p of pts) {
        const [x, y] = this.project(p[0], p[1]);
        // skip the jump when the track leaves and re-enters the view
        if (x < -2000 || x > this.w + 2000 || y < -2000 || y > this.h + 2000) { pen = false; continue; }
        if (!pen) { ctx.moveTo(x, y); pen = true; } else ctx.lineTo(x, y);
      }
      ctx.stroke();
      ctx.restore();
    };
    ctx.globalAlpha = t.opacity ?? 1;
    if (t.show_boat) line(t.track.boat, 'rgba(95,168,232,.6)', 1.2, [5, 4]);
    if (t.show_fish !== false) line(t.track.fish, t.colour || '#35b8a6', 1.6);
    ctx.globalAlpha = 1;
  }

  /// An imported GPX: lines for tracks and routes, pins for waypoints.
  ///
  /// Vector rather than tiled, because these are a few thousand points at most
  /// and because a line drawn from its own coordinates stays crisp at every
  /// zoom, which is exactly what you want when the question is whether it lies
  /// on top of the sonar track or beside it.
  drawVector(l) {
    const fc = l.features;
    if (!fc || !fc.features) return;
    const ctx = this.ctx;
    ctx.save();
    ctx.globalAlpha = l.opacity ?? 1;
    const colour = l.colour || '#f0b429';
    for (const f of fc.features) {
      const g = f.geometry;
      if (!g) continue;
      if (g.type === 'LineString') {
        ctx.strokeStyle = colour;
        ctx.lineWidth = f.properties?.kind === 'route' ? 1.2 : 1.8;
        ctx.lineJoin = 'round';
        ctx.lineCap = 'round';
        if (f.properties?.kind === 'route') ctx.setLineDash([7, 5]);
        ctx.beginPath();
        let pen = false;
        for (const c of g.coordinates) {
          const [x, y] = this.project(c[1], c[0]);
          if (x < -3000 || x > this.w + 3000 || y < -3000 || y > this.h + 3000) { pen = false; continue; }
          if (!pen) { ctx.moveTo(x, y); pen = true; } else ctx.lineTo(x, y);
        }
        ctx.stroke();
        ctx.setLineDash([]);
      } else if (g.type === 'Point') {
        const [x, y] = this.project(g.coordinates[1], g.coordinates[0]);
        if (x < -20 || x > this.w + 20 || y < -20 || y > this.h + 20) continue;
        ctx.beginPath();
        ctx.moveTo(x, y - 6); ctx.lineTo(x + 6, y); ctx.lineTo(x, y + 6); ctx.lineTo(x - 6, y);
        ctx.closePath();
        ctx.fillStyle = colour;
        ctx.globalAlpha = (l.opacity ?? 1) * 0.85;
        ctx.fill();
        ctx.globalAlpha = l.opacity ?? 1;
        ctx.strokeStyle = 'rgba(10,13,16,.85)';
        ctx.lineWidth = 1.2;
        ctx.stroke();
        const name = f.properties?.name;
        if (name && this.zoom >= 15) {
          ctx.font = '10px "IBM Plex Mono", monospace';
          ctx.fillStyle = '#dde5ec';
          ctx.shadowColor = 'rgba(0,0,0,.9)'; ctx.shadowBlur = 3;
          ctx.fillText(name, x + 9, y + 4);
          ctx.shadowBlur = 0;
        }
      }
    }
    ctx.restore();
  }

  /// The plan, drawn under the contacts it is a search for.
  ///
  /// Every line is drawn twice: a dark casing first, then the colour over it.
  /// That is what keeps a plan legible over an arbitrary basemap -- offshore
  /// OSM is a flat mid blue, and a thin magenta line on it disappears -- and it
  /// costs one extra stroke per feature.
  ///
  /// The three kinds of line are three colours rather than three dash patterns
  /// of one colour, because at survey spacing they sit a few pixels apart and a
  /// dash pattern is not a distinction you can see there. Magenta is the
  /// recorded line, amber the run-in and run-out that is not recording, and
  /// grey the turns, which are transit.
  drawPlan() {
    const ctx = this.ctx;
    const fc = this.plan;
    if (!fc || !fc.features) return;
    const L = this.planLayers || { cov: true, turn: true, run: true, num: true };
    ctx.save();

    const path = (coords, close) => {
      ctx.beginPath();
      coords.forEach((c, i) => {
        const [x, y] = this.project(c[1], c[0]);
        if (i) ctx.lineTo(x, y); else ctx.moveTo(x, y);
      });
      if (close) ctx.closePath();
    };
    const poly = (rings) => {
      ctx.beginPath();
      for (const ring of rings) {
        ring.forEach((c, i) => {
          const [x, y] = this.project(c[1], c[0]);
          if (i) ctx.lineTo(x, y); else ctx.moveTo(x, y);
        });
        ctx.closePath();
      }
    };
    /// Casing under colour, both from one path.
    const cased = (coords, colour, width, dash) => {
      path(coords, false);
      ctx.setLineDash([]);
      ctx.strokeStyle = 'rgba(6,10,14,.72)';
      ctx.lineWidth = width + 2.2;
      ctx.stroke();
      ctx.setLineDash(dash || []);
      ctx.strokeStyle = colour;
      ctx.lineWidth = width;
      ctx.stroke();
      ctx.setLineDash([]);
    };

    ctx.lineJoin = 'round';
    ctx.lineCap = 'round';

    for (const f of fc.features) {
      const k = f.properties?.kind;
      const g = f.geometry;
      if (!g || g.type !== 'Polygon') continue;
      if (k === 'swath' && L.cov) {
        ctx.globalAlpha = 0.10; ctx.fillStyle = '#38c8ff';
        poly(g.coordinates); ctx.fill();
      } else if (k === 'gap') {
        // Always drawn, whatever the coverage layer is doing: a hole is the one
        // thing on this chart nobody should have to switch on to find out about.
        ctx.globalAlpha = 0.5; ctx.fillStyle = '#ff3b30';
        poly(g.coordinates); ctx.fill();
      } else if (k === 'box') {
        ctx.globalAlpha = 1;
        ctx.setLineDash([6, 5]);
        ctx.strokeStyle = 'rgba(6,10,14,.6)'; ctx.lineWidth = 3;
        poly(g.coordinates); ctx.stroke();
        ctx.strokeStyle = '#cbd8e4'; ctx.lineWidth = 1;
        poly(g.coordinates); ctx.stroke();
        ctx.setLineDash([]);
      }
    }
    ctx.globalAlpha = 1;

    // Turns first, then run-ins, then the recorded lines on top: the line that
    // matters is the one that must not be crossed out by anything else.
    if (L.turn) {
      for (const f of fc.features) {
        if (f.properties?.kind !== 'turn' || f.geometry?.type !== 'LineString') continue;
        cased(f.geometry.coordinates, '#93a7b8', 1.3, [4, 4]);
      }
    }
    if (L.run) {
      for (const f of fc.features) {
        const k = f.properties?.kind;
        if ((k !== 'runin' && k !== 'runout') || f.geometry?.type !== 'LineString') continue;
        cased(f.geometry.coordinates, '#ffa726', 1.6, [5, 4]);
      }
    }
    for (const f of fc.features) {
      if (f.properties?.kind !== 'line' || f.geometry?.type !== 'LineString') continue;
      const c = f.geometry.coordinates;
      cased(c, '#ff2d95', 2.2);
      const [ax, ay] = this.project(c[0][1], c[0][0]);
      const [bx, by] = this.project(c[1][1], c[1][0]);
      const len = Math.hypot(bx - ax, by - ay);
      if (len < 26) continue;
      // A head at the middle, so which way the line is run is readable without
      // counting from the ends.
      const mx = (ax + bx) / 2, my = (ay + by) / 2;
      const a = Math.atan2(by - ay, bx - ax);
      ctx.beginPath();
      ctx.moveTo(mx + 7 * Math.cos(a), my + 7 * Math.sin(a));
      ctx.lineTo(mx - 5 * Math.cos(a - 0.55), my - 5 * Math.sin(a - 0.55));
      ctx.lineTo(mx - 5 * Math.cos(a + 0.55), my - 5 * Math.sin(a + 0.55));
      ctx.closePath();
      ctx.strokeStyle = 'rgba(6,10,14,.72)'; ctx.lineWidth = 2; ctx.stroke();
      ctx.fillStyle = '#ff2d95'; ctx.fill();
      // Numbers only where they will not collide: at survey spacing the labels
      // are closer together than they are tall.
      if (L.num && len > 60 && this.metresPerPixel() < 4) {
        ctx.font = '600 10px "IBM Plex Mono", monospace';
        ctx.textAlign = 'center';
        ctx.lineWidth = 3; ctx.strokeStyle = 'rgba(6,10,14,.85)';
        ctx.strokeText(f.properties.name, ax, ay - 6);
        ctx.fillStyle = '#ffd7ee';
        ctx.fillText(f.properties.name, ax, ay - 6);
        ctx.textAlign = 'left';
      }
    }

    // Box the line crosses and does not survey: the wheel goes over before the
    // fish has reached the far edge. Drawn last and drawn always, like a
    // coverage hole -- it is a defect, not a layer to switch on.
    for (const f of fc.features) {
      if (f.properties?.kind !== 'short' || f.geometry?.type !== 'LineString') continue;
      cased(f.geometry.coordinates, '#ff3b30', 2.6, [3, 3]);
    }
    ctx.restore();
  }

  drawContacts() {
    const ctx = this.ctx;
    for (const c of this.contacts) {
      const [x, y] = this.project(c.lat, c.lon);
      if (x < -40 || x > this.w + 40 || y < -40 || y > this.h + 40) continue;
      const sel = this.selected === c.id;
      const colour = contactColour(c);
      ctx.save();
      if (c.radius_m > 0) {
        const r = c.radius_m / this.metresPerPixel();
        if (r > 2) {
          ctx.beginPath(); ctx.arc(x, y, r, 0, Math.PI * 2);
          ctx.strokeStyle = colour; ctx.lineWidth = sel ? 2 : 1;
          ctx.globalAlpha = .85; ctx.stroke();
          ctx.globalAlpha = .12; ctx.fillStyle = colour; ctx.fill();
          ctx.globalAlpha = 1;
        }
      }
      ctx.beginPath(); ctx.arc(x, y, sel ? 6 : 4.5, 0, Math.PI * 2);
      ctx.fillStyle = colour; ctx.fill();
      ctx.strokeStyle = sel ? '#fff' : 'rgba(10,13,16,.85)';
      ctx.lineWidth = sel ? 2 : 1.5; ctx.stroke();
      if (sel || this.zoom >= 17) {
        ctx.font = '600 10px "IBM Plex Mono", monospace';
        ctx.fillStyle = '#dde5ec';
        ctx.shadowColor = 'rgba(0,0,0,.9)'; ctx.shadowBlur = 3;
        ctx.fillText(c.id, x + 8, y - 6);
        ctx.shadowBlur = 0;
      }
      ctx.restore();
    }
  }

  /// The stretch of track the waterfall pane is showing.
  ///
  /// Drawn over the track rather than instead of it, so the answer to "where am
  /// I on this line" is one glance rather than a calculation.
  drawViewSpan() {
    const ctx = this.ctx;
    ctx.save();
    ctx.strokeStyle = 'rgba(53,184,166,.9)';
    ctx.lineWidth = 4;
    ctx.lineCap = 'round';
    ctx.lineJoin = 'round';
    ctx.beginPath();
    let pen = false;
    for (const p of this.viewSpan) {
      const [x, y] = this.project(p[0], p[1]);
      if (x < -3000 || x > this.w + 3000 || y < -3000 || y > this.h + 3000) { pen = false; continue; }
      if (!pen) { ctx.moveTo(x, y); pen = true; } else ctx.lineTo(x, y);
    }
    ctx.stroke();
    ctx.restore();
  }

  drawCursor() {
    const [x, y] = this.project(this.cursor[0], this.cursor[1]);
    const ctx = this.ctx;
    ctx.save();
    ctx.strokeStyle = '#35b8a6'; ctx.lineWidth = 1.4;
    ctx.beginPath();
    ctx.moveTo(x - 11, y); ctx.lineTo(x - 4, y);
    ctx.moveTo(x + 4, y); ctx.lineTo(x + 11, y);
    ctx.moveTo(x, y - 11); ctx.lineTo(x, y - 4);
    ctx.moveTo(x, y + 4); ctx.lineTo(x, y + 11);
    ctx.stroke();
    ctx.restore();
  }

  drawMeasure() {
    const { from, to } = this.measure;
    if (!from || !to) return;
    const a = this.project(from[0], from[1]);
    const b = this.project(to[0], to[1]);
    const ctx = this.ctx;
    ctx.save();
    ctx.strokeStyle = '#d9803f'; ctx.lineWidth = 1.5; ctx.setLineDash([6, 4]);
    ctx.beginPath(); ctx.moveTo(a[0], a[1]); ctx.lineTo(b[0], b[1]); ctx.stroke();
    ctx.setLineDash([]);
    for (const p of [a, b]) {
      ctx.beginPath(); ctx.arc(p[0], p[1], 3, 0, Math.PI * 2);
      ctx.fillStyle = '#d9803f'; ctx.fill();
    }
    const d = haversine(from, to), brg = bearing(from, to);
    const label = `${formatDistance(d)}  ${brg.toFixed(1)}°`;
    ctx.font = '11px "IBM Plex Mono", monospace';
    const w = ctx.measureText(label).width + 12;
    const mx = (a[0] + b[0]) / 2, my = (a[1] + b[1]) / 2;
    ctx.fillStyle = 'rgba(21,27,33,.94)';
    ctx.fillRect(mx - w / 2, my - 22, w, 18);
    ctx.strokeStyle = '#26313b'; ctx.lineWidth = 1;
    ctx.strokeRect(mx - w / 2, my - 22, w, 18);
    ctx.fillStyle = '#dde5ec';
    ctx.fillText(label, mx - w / 2 + 6, my - 9);
    ctx.restore();
  }

  drawScale() {
    const mpp = this.metresPerPixel();
    const target = Math.min(180, this.w * 0.25);
    const metres = niceDistance(target * mpp);
    const px = metres / mpp;
    const el = document.getElementById('scalebar');
    if (el) {
      el.querySelector('.bar').style.width = `${px}px`;
      el.querySelector('span').textContent = formatDistance(metres);
    }
  }

  // ---- interaction ---------------------------------------------------------

  _bind() {
    const c = this.canvas;
    let drag = null;

    /// Stop dragging without treating it as a click.
    const drop = () => {
      if (!drag) return;
      drag = null;
      c.parentElement.classList.remove('drag');
    };

    c.addEventListener('pointerdown', (e) => {
      // The primary button only. A right click used to arm a drag like any
      // other, and the context menu it opens is exactly the thing that eats the
      // release -- so the chart was left holding a drag nobody was making.
      if (e.button !== 0) return;
      c.setPointerCapture(e.pointerId);
      drag = { x: e.offsetX, y: e.offsetY, centre: [...this.centre], moved: false };
      c.parentElement.classList.add('drag');
    });

    c.addEventListener('pointermove', (e) => {
      // Nothing held down, so whatever this is, it is not a drag. The release
      // can go missing in more ways than a window is obliged to tell us about:
      // a context menu takes it, focus moves to another window, a pen hands
      // over to the mouse. Every one of them used to leave `drag` latched, and
      // from then on the chart panned along with the pointer, with no button
      // pressed and nothing to say why.
      if (drag && e.buttons === 0) drop();
      if (drag) {
        const dx = e.offsetX - drag.x, dy = e.offsetY - drag.y;
        if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
        if (this.tool === 'pan' && drag.moved) {
          const [cx, cy] = lonLatToPx(drag.centre[1], drag.centre[0], this.zoom);
          const [lon, lat] = pxToLonLat(cx - dx, cy - dy, this.zoom);
          this.centre = [lat, lon];
          this.schedule();
          this.onmove();
        }
      }
      const ll = this.unproject(e.offsetX, e.offsetY);
      this.onhover(ll, e);
    });

    const end = (e) => {
      c.parentElement.classList.remove('drag');
      if (drag && !drag.moved) {
        this.onclick(this.unproject(e.offsetX, e.offsetY), e);
      }
      drag = null;
    };
    c.addEventListener('pointerup', end);
    c.addEventListener('pointercancel', drop);
    // Capture goes when the pointer is released, but also when the window
    // decides it has gone. Either way there is no drag any more.
    c.addEventListener('lostpointercapture', drop);

    c.addEventListener('wheel', (e) => {
      e.preventDefault();
      // zoom about the cursor, so the feature under it stays put
      const before = this.unproject(e.offsetX, e.offsetY);
      const step = e.ctrlKey ? 0.5 : (e.deltaMode === 1 ? 0.5 : 0.25);
      this.zoom = Math.min(MAX_Z, Math.max(MIN_Z,
        this.zoom - Math.sign(e.deltaY) * step));
      const after = this.unproject(e.offsetX, e.offsetY);
      this.centre = [
        this.centre[0] + (before[0] - after[0]),
        this.centre[1] + (before[1] - after[1]),
      ];
      this.schedule();
      this.onmove();
    }, { passive: false });

    c.addEventListener('dblclick', (e) => {
      e.preventDefault();
      const before = this.unproject(e.offsetX, e.offsetY);
      this.zoom = Math.min(MAX_Z, this.zoom + 1);
      const after = this.unproject(e.offsetX, e.offsetY);
      this.centre = [this.centre[0] + (before[0] - after[0]),
                     this.centre[1] + (before[1] - after[1])];
      this.schedule();
    });
  }

  /// The nearest contact within `tolPx` of a position, for hit testing.
  hit(lat, lon, tolPx = 10) {
    const [px, py] = this.project(lat, lon);
    let best = null, bestD = tolPx;
    for (const c of this.contacts) {
      const [x, y] = this.project(c.lat, c.lon);
      const d = Math.hypot(x - px, y - py);
      if (d < bestD) { bestD = d; best = c; }
    }
    return best;
  }

  /// Draw a whole view into another canvas, for the report.
  ///
  /// The same projection, the same layer stack, the same colours: the report
  /// shows the chart the operator was looking at rather than a second
  /// renderer's idea of it. Everything is loaded first and drawn second,
  /// because the on-screen path is allowed to draw what it has and come back
  /// for the rest -- a report image is not.
  async renderView(out, bounds, opts = {}) {
    const layers = opts.layers || this.layers;
    const vectors = opts.vectors || this.vectors;
    const contacts = opts.contacts || this.contacts;
    const pad = opts.pad ?? 0.02;
    // The report chooses its own basemap rather than inheriting whatever the
    // operator last picked on screen. `false` still means "none", which is what
    // the older callers pass.
    const base = opts.basemap === undefined ? this.basemap
      : opts.basemap === false ? 'none' : opts.basemap;
    const seam = opts.seamark === undefined ? this.seamark : opts.seamark !== false;
    const ctx = out.getContext('2d');
    const W = out.width, H = out.height;

    // Fit the bounds, then centre on them.
    //
    // Solved, not stepped. Walking integer zooms downward until the data fits
    // overshoots by up to a factor of two -- each step halves the span, so the
    // survey ends up filling anywhere between half and all of the frame, and
    // which one you get is luck. That is where the margin in the old reports
    // came from. The renderer has always taken a fractional zoom (`draw` picks
    // the tile zoom separately), so there is nothing to round here.
    let z;
    if (bounds) {
      const a = lonLatToPx(bounds.min_lon, bounds.max_lat, REF_Z);
      const b = lonLatToPx(bounds.max_lon, bounds.min_lat, REF_Z);
      const dx = Math.max(Math.abs(b[0] - a[0]), 1e-9);
      const dy = Math.max(Math.abs(b[1] - a[1]), 1e-9);
      z = REF_Z + Math.min(
        Math.log2(W * (1 - pad) / dx),
        Math.log2(H * (1 - pad) / dy));
    } else {
      z = this.zoom;
    }
    z = Math.min(MAX_Z, Math.max(MIN_Z, z));
    const centre = bounds
      ? [(bounds.min_lat + bounds.max_lat) / 2, (bounds.min_lon + bounds.max_lon) / 2]
      : this.centre;

    // Everything this view needs, fetched before anything is drawn.
    // Tile zooms are integers even when the view zoom is not; `drawTileLayer`
    // scales the difference, exactly as the on-screen path does.
    const tz = Math.min(19, Math.max(0, Math.round(z)));
    const lz = Math.min(20, Math.round(z));
    const need = new Set();
    const gather = (zz, urlFor) => {
      const [cx, cy] = lonLatToPx(centre[1], centre[0], z);
      const size = TILE * Math.pow(2, z - zz);
      const left = cx - W / 2, top = cy - H / 2;
      for (let ty = Math.floor(top / size); ty <= Math.floor((top + H) / size); ty++) {
        for (let tx = Math.floor(left / size); tx <= Math.floor((left + W) / size); tx++) {
          const n = 1 << zz;
          if (ty < 0 || ty >= n) continue;
          need.add(urlFor(zz, ((tx % n) + n) % n, ty));
        }
      }
    };
    if (base !== 'none') gather(tz, (zz, x, y) => tileUrl.base(base, zz, x, y));
    for (const l of layers) gather(lz, urlFor(l));
    if (seam) gather(Math.min(tz, 18), (zz, x, y) => tileUrl.base('seamark', zz, x, y));
    await this.tiles.all(need);

    // Borrow the live renderer by pointing it at the export canvas.
    const saved = {
      ctx: this.ctx, w: this.w, h: this.h, centre: this.centre, zoom: this.zoom,
      layers: this.layers, vectors: this.vectors, contacts: this.contacts,
      viewSpan: this.viewSpan, cursor: this.cursor, measure: this.measure,
      selected: this.selected,
    };
    Object.assign(this, {
      ctx, w: W, h: H, centre, zoom: z,
      layers, vectors, contacts,
      viewSpan: null, cursor: null, measure: null, selected: null,
    });
    try {
      ctx.save();
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.fillStyle = '#0a0d10';
      ctx.fillRect(0, 0, W, H);
      if (base !== 'none') {
        this.drawTileLayer(tz, (zz, x, y) => tileUrl.base(base, zz, x, y), 0.78, 'base');
      }
      for (const l of layers) this.drawTileLayer(lz, urlFor(l), l.opacity ?? 1);
      if (seam) {
        this.drawTileLayer(Math.min(tz, 18), (zz, x, y) => tileUrl.base('seamark', zz, x, y), 0.9, 'base');
      }
      for (const l of vectors) {
        if (l.kind === 'track') this.drawTrack(l);
        else this.drawVector(l);
      }
      this.drawContacts();
      this.drawScaleInto(ctx, W, H, metresPerPx(centre[0], z));
      ctx.restore();
    } finally {
      Object.assign(this, saved);
    }
    return { zoom: z, centre, metresPerPx: metresPerPx(centre[0], z) };
  }

  /// A scale bar and a north arrow drawn into the image itself, so the picture
  /// is still readable once it has left the application.
  drawScaleInto(ctx, W, H, mpp) {
    const metres = niceDistance(Math.min(W * 0.22, 260) * mpp);
    const px = metres / mpp;
    ctx.save();
    ctx.fillStyle = 'rgba(10,13,16,.72)';
    ctx.fillRect(12, H - 46, px + 28, 34);
    ctx.strokeStyle = '#dde5ec';
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.moveTo(26, H - 22); ctx.lineTo(26 + px, H - 22);
    ctx.moveTo(26, H - 27); ctx.lineTo(26, H - 17);
    ctx.moveTo(26 + px, H - 27); ctx.lineTo(26 + px, H - 17);
    ctx.stroke();
    ctx.fillStyle = '#dde5ec';
    ctx.font = '12px "IBM Plex Mono", ui-monospace, monospace';
    ctx.fillText(formatDistance(metres), 26, H - 31);
    // north
    ctx.beginPath();
    ctx.arc(W - 34, H - 34, 16, 0, Math.PI * 2);
    ctx.fillStyle = 'rgba(10,13,16,.72)';
    ctx.fill();
    ctx.fillStyle = '#dde5ec';
    ctx.beginPath();
    ctx.moveTo(W - 34, H - 46); ctx.lineTo(W - 29, H - 30); ctx.lineTo(W - 39, H - 30);
    ctx.closePath();
    ctx.fill();
    ctx.font = '10px "IBM Plex Mono", ui-monospace, monospace';
    ctx.fillText('N', W - 38, H - 20);
    ctx.restore();
  }

  /// Render a square crop centred on a position into `out`, for a contact
  /// snapshot. Drawn from the same tiles the view uses, at a zoom chosen so the
  /// requested span fills the frame -- so the snapshot is to scale and carries
  /// its own scale bar.
  ///
  /// `opts.layers` draws something other than what is on screen, which is how
  /// one band is cropped on its own: the chart may have both switched on and
  /// blended, and the whole point of a per-band crop is that it is not blended.
  /// `opts.basemap` false leaves the sea out from under it, so an empty square
  /// reads as "this band saw nothing here" rather than as a hole in the chart.
  async snapshot(out, lat, lon, spanM = 60, opts = {}) {
    const size = out.width;
    let z = MAX_Z;
    while (z > MIN_Z && (spanM / metresPerPx(lat, z)) > size) z--;
    const mpp = metresPerPx(lat, z);
    const [cx, cy] = lonLatToPx(lon, lat, z);
    const ctx = out.getContext('2d');
    ctx.fillStyle = '#07090b';
    ctx.fillRect(0, 0, size, size);

    const need = [];
    const collect = (urlFor, alpha) => {
      const left = cx - size / 2, top = cy - size / 2;
      const x0 = Math.floor(left / TILE), x1 = Math.floor((left + size) / TILE);
      const y0 = Math.floor(top / TILE), y1 = Math.floor((top + size) / TILE);
      for (let ty = y0; ty <= y1; ty++)
        for (let tx = x0; tx <= x1; tx++)
          need.push({ url: urlFor(z, tx, ty),
                      x: tx * TILE - left, y: ty * TILE - top, alpha });
    };
    const layers = opts.layers || this.layers;
    const base = opts.basemap === false ? 'none' : this.basemap;
    if (base !== 'none') collect((z, x, y) => tileUrl.base(base, z, x, y), 0.7);
    for (const l of layers) collect(urlFor(l), l.opacity ?? 1);

    // Through the same store as everything else, so a snapshot taken over
    // ground the operator has been looking at costs nothing, and one taken over
    // a base map square still being fetched waits for it rather than leaving a
    // hole in the sheet.
    await this.tiles.all(need.map(t => t.url));
    for (const t of need) {
      const img = this.tiles.peek(t.url);
      if (!img) continue;
      ctx.globalAlpha = t.alpha;
      ctx.drawImage(img, Math.round(t.x), Math.round(t.y), TILE, TILE);
    }
    ctx.globalAlpha = 1;

    // No reticle: the crop is built around the contact's own world pixel, so
    // the centre of the image *is* the contact. Drawing a marker there only
    // covers the target the sheet exists to show.
    const barM = niceDistance(size * 0.3 * mpp);
    const barPx = barM / mpp;
    ctx.fillStyle = 'rgba(7,9,11,.7)';
    ctx.fillRect(8, size - 30, barPx + 16, 22);
    ctx.strokeStyle = '#dde5ec'; ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(16, size - 14); ctx.lineTo(16 + barPx, size - 14);
    ctx.moveTo(16, size - 18); ctx.lineTo(16, size - 10);
    ctx.moveTo(16 + barPx, size - 18); ctx.lineTo(16 + barPx, size - 10);
    ctx.stroke();
    ctx.fillStyle = '#dde5ec';
    ctx.font = '10px "IBM Plex Mono", monospace';
    ctx.fillText(formatDistance(barM), 16, size - 20);
    return { zoom: z, metresPerPx: mpp };
  }
}
