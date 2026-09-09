// The waterfall.
//
// The view is a window onto a virtual image that is far too tall to hold: at
// one row per ping, a day's recording is half a million rows. So the server
// renders it in blocks and this keeps several of them either side of what is on
// screen, which is what makes scrolling feel like scrolling rather than like
// asking for a new picture each time.
//
// The row metadata that comes with each block is what makes the view more than
// a picture: it is how a click becomes a position, how the map cursor finds its
// row, and how the across-track ruler knows what a pixel is worth.

import { localScale } from './geo.js';

// The across-track axis the image was drawn on. A column offset is a *slant*
// range in slant mode and a ground range in ground mode, and the seabed is only
// ever at the ground one -- so every inverse below has to know which it has.
// Reading a slant offset as a ground distance is what used to slide the whole
// image outward the moment the axis changed.
//
// These mirror `Axis::to_ground` / `Axis::from_ground` in `waterfall.rs`; the
// tests check the two agree.

/// A distance along the image's axis, as ground range. Saturates at nadir:
/// inside the water column a column looks at no seabed at all.
export function toGround(axis, d, alt) {
  if (axis !== 'slant') return d;
  return (d < 0 ? -1 : 1) * Math.sqrt(Math.max(d * d - alt * alt, 0));
}

/// The inverse: where a ground range lands on the image's axis.
export function fromGround(axis, across, alt) {
  if (axis !== 'slant') return across;
  return (across < 0 ? -1 : 1) * Math.sqrt(across * across + alt * alt);
}

/// Which end of a vertical slider is the minimum, and the conversion for it.
///
/// Two mechanisms lay a vertical `input[type=range]` out and they disagree.
/// `writing-mode: vertical-lr` -- the standard one, Chrome 121 and Firefox --
/// runs the value along the inline axis, top to bottom, so the minimum is at
/// the top where a scrollbar's is. WebKit's older `-webkit-appearance:
/// slider-vertical` puts the minimum at the *bottom*, so the same slider runs
/// backwards. The desktop shell is WebKitGTK and the browser is not, which is
/// why the waterfall scrolled the wrong way in one of them and not the other.
///
/// One function for both directions, because the mapping is its own inverse.
export function sliderRow(v, max, down) {
  return down ? v : max - v;
}

/// Does this engine lay a vertical range out top to bottom?
///
/// Measured rather than sniffed: an engine that honours `writing-mode` on a
/// range renders it taller than it is wide with no other help, and one that
/// does not renders it as an ordinary horizontal slider.
export function verticalSliderRunsDown(doc = document) {
  const t = doc.createElement('input');
  t.type = 'range';
  t.style.cssText = 'writing-mode:vertical-lr;position:absolute;visibility:hidden;'
                  + 'left:-9999px;width:auto;height:auto;padding:0;margin:0';
  doc.body.appendChild(t);
  const down = t.offsetHeight > t.offsetWidth;
  t.remove();
  return down;
}

export class WaterfallView {
  constructor(canvas, opts = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d', { alpha: false });
    // Loaded blocks, sorted by start ping. Each is one server render:
    // { key, start, count, stride, width, height, rows, image, total_pings }
    this.blocks = [];
    this.stride = 1;
    this.imageWidth = 1024;
    this.totalPings = 0;
    /// Which axis the loaded blocks were drawn on, from the server. Changing
    /// the Axis control clears the blocks, so one value covers all of them.
    this.axis = 'ground';
    /// Row at the top of the pane, on the virtual image's axis. Fractional, so
    /// scrolling is smooth rather than row-by-row.
    this.top = 0;
    this.contacts = [];
    this.cursorRow = null;   // global row the map is pointing at
    this.hover = null;
    this.onhover = opts.onhover || (() => {});
    this.onclick = opts.onclick || (() => {});
    this.onscroll = opts.onscroll || (() => {});
    this.tool = 'pan';
    // 'fit' draws one image row per ping, which is what a sonar operator is
    // used to. 'true' stretches the along-track axis so a metre down the image
    // is the same as a metre across it -- the only mode in which a target's
    // shape on screen is its shape on the seabed.
    this.aspectMode = 'true';
    /// Across-track metres per image pixel to *draw* at, or null for this
    /// view's own.
    ///
    /// Panes over one recording have to agree on the vertical scale, because
    /// they are the same rows of the same tow and they scroll together. At true
    /// scale the vertical scale follows the across-track one, and the bands do
    /// not share that: the low band here runs a 50 m swath and the high band a
    /// 25 m one, so left to themselves the two panes put 1.7 times as many rows
    /// on screen as each other and slid apart the moment either was scrolled.
    ///
    /// Setting the widest band's scale here makes them agree. The narrower
    /// band's image is then drawn narrower and centred, which is not a
    /// compromise -- it is what a 25 m swath beside a 50 m one looks like.
    this.acrossRef = null;
    this._raf = null;
    this._bind();
    this.resize();
  }

  // ---- the virtual image ---------------------------------------------------

  /// Rows in the whole recording at the current stride.
  totalRows() {
    return Math.max(1, Math.ceil(this.totalPings / this.stride));
  }

  /// Global row of a block's first line.
  blockTop(b) {
    return b.start / b.stride;
  }

  get ready() {
    return this.blocks.length > 0;
  }

  clear() {
    this.blocks = [];
    this.top = 0;
    this.draw();
  }

  /// Add a block, keeping the list sorted and free of duplicates.
  addBlock(b) {
    if (b.stride !== this.stride) return;      // a stale fetch from before a change
    this.blocks = this.blocks.filter(x => x.start !== b.start);
    this.blocks.push(b);
    this.blocks.sort((a, c) => a.start - c.start);
    this.totalPings = b.total_pings ?? this.totalPings;
    this.imageWidth = b.width;
    this.axis = b.axis || 'ground';
    this.draw();
  }

  dropBlocksOutside(loPing, hiPing) {
    this.blocks = this.blocks.filter(b =>
      b.start + b.count > loPing && b.start < hiPing);
  }

  hasBlock(start) {
    return this.blocks.some(b => b.start === start);
  }

  /// The block containing a global row, and the row's index inside it.
  locate(g) {
    for (const b of this.blocks) {
      const t = this.blockTop(b);
      if (g >= t && g < t + b.height) return { block: b, local: Math.floor(g - t) };
    }
    return null;
  }

  /// Row metadata at a global row, or null if that row is not loaded.
  rowAt(g) {
    const at = this.locate(g);
    return at ? at.block.rows[at.local] : null;
  }

  /// Time at a global row, from the rows themselves where they are loaded and
  /// from the ping rate where they are not.
  ///
  /// The pings of one recording are evenly spaced -- 0.069 s apart on this
  /// sonar, measured -- so any loaded block fixes the mapping for the whole of
  /// it. That is what lets two panes over two different recordings be scrolled
  /// to the same moment without asking the server where that moment is.
  timeAt(g) {
    const r = this.rowAt(Math.floor(g));
    if (r) return r.time;
    const m = this.timeModel();
    return m && m.t0 + (g - m.g0) * m.dt;
  }

  /// The inverse: which row is at a time. Fractional, and outside the loaded
  /// blocks it is an extrapolation -- good to a row or two over a recording,
  /// which is a tenth of a second.
  rowAtTime(t) {
    const m = this.timeModel();
    return m && m.g0 + (t - m.t0) / m.dt;
  }

  /// Row-to-time as a straight line, taken from the first block that has two
  /// rows to fix it with.
  timeModel() {
    for (const b of this.blocks) {
      const rows = b.rows;
      if (!rows || rows.length < 2) continue;
      const dt = (rows[rows.length - 1].time - rows[0].time) / (rows.length - 1);
      if (dt > 0) return { g0: this.blockTop(b), t0: rows[0].time, dt };
    }
    return null;
  }

  /// Every loaded row, in order. Used for drawing overlays across blocks.
  *eachRow() {
    for (const b of this.blocks) {
      const t = this.blockTop(b);
      for (let i = 0; i < b.rows.length; i++) yield [t + i, b.rows[i]];
    }
  }

  // ---- scale ---------------------------------------------------------------

  /// Metres of seabed per image pixel, across-track. This view's own swath.
  metresPerPxAcross() {
    const r = this.rowAt(Math.floor(this.top)) || this.blocks[0]?.rows[0];
    if (!r) return null;
    return (r.half_width_m * 2) / this.imageWidth;
  }

  /// The across-track scale the image is drawn at: shared where a reference has
  /// been set, this view's own otherwise.
  drawnMetresPerPx() {
    return this.acrossRef || this.metresPerPxAcross();
  }

  /// Canvas pixels per image pixel, and where the image's left edge sits.
  ///
  /// A band narrower than the reference is drawn narrower and centred on the
  /// nadir, so a metre across is a metre across in every pane beside it.
  xScale() {
    const own = this.metresPerPxAcross(), ref = this.drawnMetresPerPx();
    const shrink = own && ref ? Math.min(1, own / ref) : 1;
    return (this.w / this.imageWidth) * shrink;
  }

  xOffset() {
    return (this.w - this.imageWidth * this.xScale()) / 2;
  }

  /// Metres of seabed per *canvas* pixel across-track -- what the operator is
  /// actually looking at. Equal in every pane that shares a reference scale,
  /// which is the point of having one.
  metresPerCanvasPx() {
    const own = this.metresPerPxAcross();
    const sx = this.xScale();
    return own && sx > 0 ? own / sx : null;
  }

  /// Median along-track advance per image row, metres. The fish does not cover
  /// a fixed distance per ping, so this is a median rather than a constant.
  metresPerRow() {
    const b = this.locate(Math.floor(this.top))?.block || this.blocks[0];
    if (!b || b.rows.length < 2) return null;
    const v = b.rows.slice(1).map(r => r.advance_m).filter(x => x > 0).sort((a, c) => a - c);
    return v.length ? v[v.length >> 1] : null;
  }

  /// How much taller than wide one image pixel is drawn.
  aspectFactor() {
    if (this.aspectMode !== 'true') return 1;
    const across = this.drawnMetresPerPx(), along = this.metresPerRow();
    if (!across || !along) return 1;
    return Math.min(Math.max(along / across, 0.2), 40);
  }

  /// Canvas pixels one image row occupies vertically.
  ///
  /// Deliberately not `xScale()`: the row height follows the *reference* across
  /// scale, so every pane sharing that reference puts the same rows in the same
  /// place however wide its own swath is.
  rowHeightPx() {
    return (this.w / this.imageWidth) * this.aspectFactor();
  }

  /// Image rows the pane can show at the current scale.
  visibleRows() {
    return this.h / Math.max(this.rowHeightPx(), 1e-6);
  }

  /// Clamp the scroll position to the recording.
  clampTop(t) {
    const max = Math.max(0, this.totalRows() - this.visibleRows());
    return Math.min(Math.max(t, 0), max);
  }

  scrollTo(g, { silent = false } = {}) {
    const t = this.clampTop(g);
    if (Math.abs(t - this.top) < 1e-6) return false;
    this.top = t;
    this.schedule();
    if (!silent) this.onscroll(this.top);
    return true;
  }

  /// Put a global row in the middle of the pane.
  centreOn(g, opts) {
    return this.scrollTo(g - this.visibleRows() / 2, opts);
  }

  /// Global row currently at the middle of the pane.
  centreRow() {
    return this.top + this.visibleRows() / 2;
  }

  resize() {
    const dpr = window.devicePixelRatio || 1;
    const r = this.canvas.getBoundingClientRect();
    this.w = Math.max(1, Math.round(r.width));
    this.h = Math.max(1, Math.round(r.height));
    this.canvas.width = this.w * dpr;
    this.canvas.height = this.h * dpr;
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.top = this.clampTop(this.top);
    this.draw();
  }

  // ---- coordinates ---------------------------------------------------------

  /// Canvas pixel -> [image column, global row].
  toImage(px, py) {
    return [(px - this.xOffset()) / this.xScale(), this.top + py / this.rowHeightPx()];
  }

  /// [image column, global row] -> canvas pixel.
  fromImage(ix, g) {
    return [this.xOffset() + ix * this.xScale(), (g - this.top) * this.rowHeightPx()];
  }

  /// Across-track metres at an image column, on a global row.
  ///
  /// Ground range: this is what places a mark on the chart, so it is the ground
  /// distance in both axes, not the number the ruler happens to show.
  acrossAt(ix, g) {
    const r = this.rowAt(Math.floor(g));
    if (!r) return null;
    const half = this.imageWidth / 2;
    return toGround(this.axis, (ix - half) / half * r.half_width_m, r.altitude);
  }

  /// The raw distance along the image's own axis at a column -- what the range
  /// ruler is measuring. Equal to `acrossAt` in ground mode.
  rangeAt(ix, g) {
    const r = this.rowAt(Math.floor(g));
    if (!r) return null;
    const half = this.imageWidth / 2;
    return (ix - half) / half * r.half_width_m;
  }

  /// Is this column looking at seabed at all? In slant mode the band either
  /// side of nadir narrower than the altitude is water, and it has no position.
  inWaterColumn(ix, g) {
    if (this.axis !== 'slant') return false;
    const r = this.rowAt(Math.floor(g));
    if (!r) return false;
    const half = this.imageWidth / 2;
    return Math.abs((ix - half) / half * r.half_width_m) < r.altitude;
  }

  // ---- drawing -------------------------------------------------------------

  schedule() {
    if (this._raf) return;
    this._raf = requestAnimationFrame(() => { this._raf = null; this.draw(); });
  }

  draw() {
    const ctx = this.ctx;
    ctx.fillStyle = '#07090b';
    ctx.fillRect(0, 0, this.w, this.h);
    if (!this.blocks.length) return;

    const rh = this.rowHeightPx();
    const scaleX = this.xScale();
    const x0 = this.xOffset();
    const vis = this.visibleRows();
    ctx.imageSmoothingEnabled = false;

    // Only the slice of each block that the pane can see. Handing the whole
    // block to drawImage and letting it clip is the same picture and several
    // times the work when a block is four screens tall.
    for (const b of this.blocks) {
      if (!b.image) continue;
      const t = this.blockTop(b);
      const from = Math.max(this.top, t);
      const to = Math.min(this.top + vis, t + b.height);
      if (to <= from) continue;
      const sy = Math.floor(from - t);
      const sh = Math.ceil(to - t) - sy;
      if (sh <= 0) continue;
      ctx.drawImage(b.image,
        0, sy, b.width, sh,
        x0, (t + sy - this.top) * rh, b.width * scaleX, sh * rh);
    }

    // Nadir line: the fish's own ground track down the middle of the image.
    ctx.save();
    ctx.strokeStyle = 'rgba(240,180,41,.35)';
    ctx.lineWidth = 1;
    ctx.setLineDash([3, 5]);
    ctx.beginPath();
    ctx.moveTo(this.w / 2, 0);
    ctx.lineTo(this.w / 2, this.h);
    ctx.stroke();
    ctx.restore();

    this.drawGaps();
    this.drawRangeTicks();
    this.drawFlags();
    this.drawContacts();

    if (this.cursorRow != null) {
      const [, y] = this.fromImage(0, this.cursorRow);
      if (y >= 0 && y <= this.h) {
        ctx.save();
        ctx.strokeStyle = '#35b8a6';
        ctx.lineWidth = 1.2;
        ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(this.w, y); ctx.stroke();
        ctx.restore();
      }
    }
    if (this.hover) this.drawHover();
  }

  /// Hatch the parts of the window that have no block behind them yet, so a
  /// fast scroll reads as "still loading" rather than as "nothing here".
  drawGaps() {
    const rh = this.rowHeightPx();
    const vis = this.visibleRows();
    const spans = [];
    let cursor = this.top;
    for (const b of this.blocks) {
      const t = this.blockTop(b);
      if (t > cursor) spans.push([cursor, Math.min(t, this.top + vis)]);
      cursor = Math.max(cursor, t + b.height);
    }
    if (cursor < this.top + vis) spans.push([cursor, this.top + vis]);
    const ctx = this.ctx;
    ctx.save();
    ctx.fillStyle = 'rgba(53,184,166,.05)';
    for (const [a, z] of spans) {
      if (z <= a) continue;
      ctx.fillRect(0, (a - this.top) * rh, this.w, (z - a) * rh);
    }
    ctx.restore();
  }

  /// Across-track distance ticks, so the image can be read metrically.
  drawRangeTicks() {
    const r = this.rowAt(Math.floor(this.top)) || this.blocks[0]?.rows[0];
    const half = r?.half_width_m;
    if (!(half > 0)) return;
    const ctx = this.ctx;
    const step = half > 120 ? 50 : half > 50 ? 20 : half > 20 ? 10 : 5;
    ctx.save();
    ctx.font = '9px "IBM Plex Mono", monospace';
    ctx.strokeStyle = 'rgba(221,229,236,.16)';
    ctx.fillStyle = 'rgba(221,229,236,.45)';
    ctx.lineWidth = 1;
    const edge = (this.imageWidth / 2) * this.xScale();
    for (let d = step; d <= half; d += step) {
      for (const s of [-1, 1]) {
        const x = this.w / 2 + s * (d / half) * edge;
        ctx.beginPath();
        ctx.moveTo(x, 0); ctx.lineTo(x, this.h);
        ctx.stroke();
        // In slant mode these are slant ranges, not distances across the
        // seabed. Same number, different question -- so say which.
        ctx.fillText(d === step && this.axis === 'slant' ? `${d} slant` : `${d}`, x + 3, 11);
      }
    }
    ctx.restore();
  }

  /// Mark the rows whose roll exceeded the flag threshold.
  ///
  /// A stripe, not a filter. Whether roll actually corrupts the geometry is not
  /// established -- the two sides' bottom ranges show no correlation with it --
  /// so the operator is told where it happened and left to judge.
  drawFlags() {
    const ctx = this.ctx;
    const rh = this.rowHeightPx();
    const vis = this.visibleRows();
    ctx.save();
    ctx.fillStyle = 'rgba(217,128,63,.5)';
    for (const [g, r] of this.eachRow()) {
      if (r.clean) continue;
      if (g < this.top - 1 || g > this.top + vis) continue;
      ctx.fillRect(this.w - 4, (g - this.top) * rh, 4, Math.max(1, rh));
    }
    ctx.restore();
  }

  /// Where a position falls in the image, as [column, global row], or null.
  ///
  /// The row that has the point abeam, not the row nearest to it -- see
  /// `Waterfall::world_to_pixel` on the Rust side for why those differ. Search
  /// every loaded row unless told to stay on screen, so a snapshot can be taken
  /// of a contact that is not currently in view.
  locateWorld(lat, lon, onlyVisible = false) {
    const vis = this.visibleRows();
    let bg = -1, bAlong = Infinity, bAcross = 0, bHalf = 0;
    for (const [g, r] of this.eachRow()) {
      if (onlyVisible && (g < this.top - 2 || g > this.top + vis + 2)) continue;
      const [mLat, mLon] = localScale(r.fish_lat);
      const dn = (lat - r.fish_lat) * mLat;
      const de = (lon - r.fish_lon) * mLon;
      const b = r.bearing * Math.PI / 180;
      const along = dn * Math.cos(b) + de * Math.sin(b);
      const across = -dn * Math.sin(b) + de * Math.cos(b);
      // Accept on the image's own axis: in slant mode the outer columns cover
      // less ground than their range suggests, so a ground test lets in
      // points the picture does not actually contain.
      const d = fromGround(this.axis, across, r.altitude);
      if (Math.abs(d) > r.half_width_m) continue;
      if (Math.abs(along) < bAlong) {
        bAlong = Math.abs(along); bAcross = d; bg = g; bHalf = r.half_width_m;
      }
    }
    if (bg < 0 || bAlong > 30) return null;
    return [this.imageWidth / 2 + bAcross / bHalf * (this.imageWidth / 2), bg + 0.5];
  }

  drawContacts() {
    const ctx = this.ctx;
    for (const c of this.contacts) {
      const at = this.locateWorld(c.lat, c.lon, true);
      if (!at) continue;
      const [ix, bg] = at;
      const [x, y] = this.fromImage(ix, bg);
      ctx.save();
      ctx.strokeStyle = c.colour || '#f0b429';
      ctx.lineWidth = 1.4;
      ctx.beginPath(); ctx.arc(x, y, 7, 0, Math.PI * 2); ctx.stroke();
      ctx.font = '600 9px "IBM Plex Mono", monospace';
      ctx.fillStyle = c.colour || '#f0b429';
      ctx.fillText(c.id, x + 10, y + 3);
      ctx.restore();
    }
  }

  drawHover() {
    const ctx = this.ctx;
    const [x, y] = this.hover;
    ctx.save();
    ctx.strokeStyle = 'rgba(53,184,166,.55)';
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(x, 0); ctx.lineTo(x, this.h);
    ctx.moveTo(0, y); ctx.lineTo(this.w, y);
    ctx.stroke();
    ctx.restore();
  }

  // ---- interaction ---------------------------------------------------------

  _bind() {
    const c = this.canvas;
    let drag = null;

    c.addEventListener('wheel', (e) => {
      e.preventDefault();
      // A wheel notch moves a fixed fraction of the pane, not a fixed number
      // of rows: at true scale a row can be twenty pixels tall, and scrolling
      // by rows would leap a screen at a time.
      const lines = e.deltaMode === 1 ? e.deltaY * 3
                  : e.deltaMode === 2 ? e.deltaY * this.visibleRows()
                  : e.deltaY / 100 * 3;
      this.scrollTo(this.top + lines * (this.visibleRows() / 12));
    }, { passive: false });

    c.addEventListener('pointerdown', (e) => {
      c.setPointerCapture(e.pointerId);
      drag = { y: e.offsetY, top: this.top, moved: false };
    });

    c.addEventListener('pointermove', (e) => {
      if (drag) {
        const dy = e.offsetY - drag.y;
        if (Math.abs(dy) > 3) drag.moved = true;
        if (this.tool === 'pan' && drag.moved) {
          this.scrollTo(drag.top - dy / Math.max(this.rowHeightPx(), 1e-6));
          c.parentElement.classList.add('drag');
        }
      }
      this.hover = [e.offsetX, e.offsetY];
      const [ix, g] = this.toImage(e.offsetX, e.offsetY);
      const info = this.rowAt(Math.floor(g));
      if (info) {
        this.onhover({
          row: Math.floor(g), info, image: [ix, g],
          across_m: this.acrossAt(ix, g),
          range_m: this.rangeAt(ix, g),
          water: this.inWaterColumn(ix, g),
        });
      } else {
        this.onhover(null);
      }
      this.schedule();
    });

    const end = (e) => {
      c.parentElement.classList.remove('drag');
      if (drag && !drag.moved) {
        const [ix, g] = this.toImage(e.offsetX, e.offsetY);
        const info = this.rowAt(Math.floor(g));
        if (info) {
          this.onclick({
            row: Math.floor(g), info, image: [ix, g],
            across_m: this.acrossAt(ix, g),
            range_m: this.rangeAt(ix, g),
            water: this.inWaterColumn(ix, g),
          }, e);
        }
      }
      drag = null;
    };
    c.addEventListener('pointerup', end);
    c.addEventListener('pointercancel', () => { drag = null; });

    c.addEventListener('pointerleave', () => {
      this.hover = null;
      this.onhover(null);
      this.schedule();
    });
  }

  /// Copy a square crop around a point into `out`, for a snapshot taken from
  /// the waterfall rather than the map.
  crop(out, ix, g, spanPx) {
    const ctx = out.getContext('2d');
    ctx.fillStyle = '#07090b';
    ctx.fillRect(0, 0, out.width, out.height);
    const at = this.locate(g);
    if (!at || !at.block.image) return null;
    const s = spanPx / 2;
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(at.block.image,
      Math.round(ix - s), Math.round(at.local - s), Math.round(spanPx), Math.round(spanPx),
      0, 0, out.width, out.height);

    const row = at.block.rows[at.local];
    const mPerImgPx = (row.half_width_m * 2) / at.block.width;
    const scale = out.width / spanPx;
    // No reticle: the crop is centred on the contact by construction, and a
    // marker drawn over the target hides the thing the reader came to look at.

    // Across-track scale only. Along-track is pings, not metres -- the fish
    // does not advance a fixed distance per ping -- so drawing one bar for both
    // axes would be a lie the reader could not see.
    const barM = 5 * Math.max(1, Math.round((spanPx * mPerImgPx / 6) / 5));
    const barPx = barM / mPerImgPx * scale;
    ctx.fillStyle = 'rgba(7,9,11,.7)';
    ctx.fillRect(8, out.height - 30, barPx + 16, 22);
    ctx.strokeStyle = '#dde5ec'; ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(16, out.height - 14); ctx.lineTo(16 + barPx, out.height - 14);
    ctx.moveTo(16, out.height - 18); ctx.lineTo(16, out.height - 10);
    ctx.moveTo(16 + barPx, out.height - 18); ctx.lineTo(16 + barPx, out.height - 10);
    ctx.stroke();
    ctx.fillStyle = '#dde5ec';
    ctx.font = '10px "IBM Plex Mono", monospace';
    ctx.fillText(`${barM} m across`, 16, out.height - 20);
    return { metres_per_px: mPerImgPx };
  }
}
