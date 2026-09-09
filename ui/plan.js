// The search planner: the right-hand pane, when the app is planning a survey
// rather than reading one.
//
// It takes the waterfall's place rather than opening a window of its own,
// because the whole value of planning here instead of on paper is seeing the
// lines over the real chart -- last survey's mosaic, the seamarks, the contacts
// already marked. A modal would have thrown that away, and while planning there
// is no recording under examination for the waterfall to show.
//
// The pane owns nothing. The settings live on the project, the targets are
// contacts, and the solved geometry comes back from the server -- so what is
// drawn here and what `swath plan` prints cannot drift apart.

import { api } from './api.js';

const $ = (id) => document.getElementById(id);

/// Rank a verdict so the worst target speaks for the plan.
const RANK = { good: 2, thin: 1, missed: 0 };

export class Planner {
  constructor(opts) {
    this.on = false;
    this.spec = null;
    this.targets = [];
    this.quadrant = [];
    this.az = 0;
    this.solved = null;
    this.busy = false;
    this.onplan = opts.onplan || (() => {});   // hand the GeoJSON to the chart
    this.onmsg = opts.onmsg || (() => {});
    this.onbounds = opts.onbounds || (() => {});
    this.onimport = opts.onimport || (async () => {});
    this.onchanged = opts.onchanged || (async () => {});
    this.onlayers = opts.onlayers || (() => {});
    /// What of the plan is drawn. Twenty-odd lines with their run-ins, their
    /// turns and their coverage all at once is a solid block; being able to
    /// take layers off is what makes a dense plan readable.
    this.layers = { cov: true, turn: true, run: true, num: true };
    /// What the file is: a trace to follow, routes to steer, or both. Plenty of
    /// plotters take one or the other from a file but not both.
    this.form = 'trace';
    /// Every contact in the project. Any of them may be a target: a datum
    /// typed in from a client, or something found last week worth another look.
    this.contacts = [];
    this.chosen = null;    // null until the operator has ticked anything
    this._bind();
  }

  // ---- loading -------------------------------------------------------------

  async load() {
    const r = await api.plan();
    this.spec = r.spec;
    this.targets = r.targets || [];
    this.quadrant = r.quadrant || [];
    this.chosen = r.target_ids ?? null;
    this.fillSettings();
    this.renderTargets();
    this.renderQuadrant();
    if (this.targets.length) {
      const has = this.quadrant.some((q) => q.azimuth_deg === this.az);
      await this.select(has ? this.az : (this.quadrant[0]?.azimuth_deg ?? 0), true);
    } else {
      this.solved = null;
      this.onplan(null);
      this.renderReport();
    }
  }

  /// Re-solve after anything that changes the geometry.
  async refresh() {
    const r = await api.savePlan({ spec: this.spec, targets: this.chosen });
    this.spec = r.spec;
    this.targets = r.targets || [];
    this.quadrant = r.quadrant || [];
    this.chosen = r.target_ids ?? null;
    this.renderTargets();
    this.renderQuadrant();
    if (!this.targets.length) { this.solved = null; this.onplan(null); this.renderReport(); return; }
    await this.select(this.az, true);
  }

  /// Which contacts the search is for. Ticking is the answer to "how does it
  /// know what to look for?", so it is on screen rather than implied by a
  /// field buried in the contact dialog.
  isTarget(c) {
    return this.chosen === null ? c.source === 'datum' : this.chosen.includes(c.id);
  }

  renderTargets() {
    const ul = $('pl-tlist');
    if (!ul) return;
    $('pl-ccount').textContent = this.contacts.length;
    $('pl-tcount').textContent = this.contacts.filter((c) => this.isTarget(c)).length;
    ul.innerHTML = '';
    if (!this.contacts.length) {
      ul.innerHTML = '<li class="pl-empty">No contacts yet. “Datum…” beside Contacts takes a typed position.</li>';
      return;
    }
    for (const c of this.contacts) {
      const on = this.isTarget(c);
      const li = document.createElement('li');
      li.className = 'pl-trow' + (on ? '' : ' off');
      const tick = document.createElement('input');
      tick.type = 'checkbox';
      tick.checked = on;
      tick.title = `Search for ${c.name || c.id}`;
      tick.addEventListener('change', () => this.setTarget(c.id, tick.checked));
      const nm = document.createElement('span');
      nm.className = 'nm';
      nm.textContent = c.name || c.id;
      const src = document.createElement('span');
      src.className = 'src' + (c.source === 'datum' ? ' datum' : '');
      src.textContent = c.source === 'datum' ? 'datum' : c.source === 'waterfall' ? 'sonar' : 'chart';
      // The search radius is the uncertainty the lines have to cover, so a
      // point contact needs one before it can be planned for. Editable here
      // rather than only inside the contact dialog, because this is where the
      // question comes up.
      const r = document.createElement('input');
      r.type = 'number'; r.min = '0'; r.step = '5';
      r.value = c.radius_m || '';
      r.placeholder = '50';
      r.title = 'Search radius in metres. Empty is treated as 50 m.';
      r.addEventListener('change', () => this.setRadius(c, parseFloat(r.value)));
      li.append(tick, nm, src, r);
      ul.appendChild(li);
    }
  }

  async setTarget(id, on) {
    const base = this.chosen === null
      ? this.contacts.filter((c) => c.source === 'datum').map((c) => c.id)
      : this.chosen.slice();
    this.chosen = on ? [...new Set([...base, id])] : base.filter((x) => x !== id);
    await this.refresh();
  }

  async setRadius(c, v) {
    const r = Number.isFinite(v) && v > 0 ? v : 0;
    if (r === c.radius_m) return;
    try {
      await api.saveContact({ ...c, radius_m: r, shape: r > 0 && c.shape === 'point' ? 'circle' : c.shape });
      await this.onchanged();
    } catch (e) {
      this.onmsg(String(e.message || e), 'warn');
    }
  }

  async select(az, force = false) {
    if (!this.targets.length) return;
    if (!force && az === this.az && this.solved) return;
    this.az = az;
    if (this.busy) { this._pending = az; return; }
    this.busy = true;
    try {
      let want = az;
      for (;;) {
        this.solved = await api.solvePlan(want);
        if (this._pending == null || this._pending === want) break;
        want = this._pending;
        this._pending = null;
      }
      this._pending = null;
      this.onplan(this.solved.geojson);
      this.renderReport();
      this.renderQuadrant();
    } catch (e) {
      this.onmsg(String(e.message || e), 'warn');
    } finally {
      this.busy = false;
    }
  }

  // ---- settings ------------------------------------------------------------

  fillSettings() {
    const s = this.spec;
    if (!s) return;
    const set = (id, v) => { const el = $(id); if (el) el.value = v; };
    set('pl-range', s.sonar.range_m);
    set('pl-alt', s.sonar.altitude_m);
    set('pl-nadk', s.sonar.nadir_factor);
    set('pl-spacing', s.sonar.spacing_m);
    set('pl-lay', s.rig.layback_m);
    set('pl-gps', s.rig.gps_to_towpoint_m);
    set('pl-runin', s.rig.run_in_m);
    set('pl-speed', s.rig.speed_kn);
    set('pl-turn', s.rig.turn_radius_m);
    set('pl-dead', s.rig.turn_allowance_s);
    set('pl-margin', s.margin_m);
    set('pl-step', s.step_deg);
    for (const [name, val] of [['pl-regime', s.sonar.regime], ['pl-order', s.order],
                               ['pl-dir', s.direction]]) {
      const el = document.querySelector(`#${name} [value="${val}"]`);
      if (el) el.checked = true;
    }
    this.syncRegime();
  }

  readSettings() {
    const n = (id, d) => { const v = parseFloat($(id)?.value); return Number.isFinite(v) ? v : d; };
    const pick = (name, d) => document.querySelector(`#${name} input:checked`)?.value ?? d;
    const s = this.spec;
    s.sonar.range_m = n('pl-range', s.sonar.range_m);
    s.sonar.altitude_m = n('pl-alt', s.sonar.altitude_m);
    s.sonar.nadir_factor = n('pl-nadk', s.sonar.nadir_factor);
    s.sonar.spacing_m = n('pl-spacing', s.sonar.spacing_m);
    s.sonar.regime = pick('pl-regime', s.sonar.regime);
    s.rig.layback_m = n('pl-lay', s.rig.layback_m);
    s.rig.gps_to_towpoint_m = n('pl-gps', s.rig.gps_to_towpoint_m);
    s.rig.run_in_m = n('pl-runin', s.rig.run_in_m);
    s.rig.speed_kn = n('pl-speed', s.rig.speed_kn);
    s.rig.turn_radius_m = n('pl-turn', s.rig.turn_radius_m);
    s.rig.turn_allowance_s = n('pl-dead', s.rig.turn_allowance_s);
    s.margin_m = n('pl-margin', s.margin_m);
    s.order = pick('pl-order', s.order);
    s.direction = pick('pl-dir', s.direction);
    s.step_deg = n('pl-step', s.step_deg);
    this.syncRegime();
  }

  /// The three regimes, priced in metres, so the choice is made on what it
  /// buys rather than on a percentage.
  syncRegime() {
    const s = this.spec;
    if (!s) return;
    const r = s.sonar.range_m;
    const nd = Math.min(r * 0.6, s.sonar.nadir_factor * s.sonar.altitude_m);
    const at = { recon: 2 * (r - nd), full: r, double: r - nd, custom: s.sonar.spacing_m };
    for (const k of ['recon', 'full', 'double', 'custom']) {
      const el = $(`pl-at-${k}`);
      if (el) el.textContent = `${Math.round(at[k])} m`;
    }
    $('pl-nadir').textContent = `${Math.round(nd)} m`;
    $('pl-offset').textContent = `${Math.round(s.rig.gps_to_towpoint_m + s.rig.layback_m)} m`;
    const custom = document.querySelector('#pl-regime input:checked')?.value === 'custom';
    $('pl-spacing-row').hidden = !custom;
  }

  // ---- the quadrant --------------------------------------------------------

  renderQuadrant() {
    const ul = $('pl-list');
    if (!ul) return;
    if (!this.quadrant.length) {
      ul.innerHTML = this.contacts.length
        ? '<li class="pl-empty">Nothing ticked above, so there is nothing to plan a search over.</li>'
        : '<li class="pl-empty">No contacts yet. Enter a position with &ldquo;Datum&hellip;&rdquo;, or mark one on the chart.</li>';
      return;
    }
    ul.innerHTML = '';
    for (const q of this.quadrant) {
      const li = document.createElement('li');
      li.className = 'pl-row';
      if (Math.abs(q.azimuth_deg - this.az) < 1e-6) li.classList.add('on');
      const worst = q.worst || 'good';
      const unseen = q.coverage_none * 100;
      li.innerHTML =
        `<span class="pl-az">${String(Math.round(q.azimuth_deg)).padStart(3, '0')}&deg;</span>` +
        `<span class="pl-n">${q.lines}</span>` +
        `<span class="pl-d">${(q.distance_m / 1000).toFixed(1)} km</span>` +
        `<span class="pl-t">${hm(q.seconds)}</span>` +
        `<span class="pl-u${unseen > 0.05 ? ' bad' : ''}" title="Seabed inside the box that no line covers">` +
          `${unseen > 0.05 ? unseen.toFixed(1) + '%' : '—'}</span>` +
        `<span class="pl-v ${worst}" title="${verdictWords(worst)}"></span>`;
      li.addEventListener('click', () => this.select(q.azimuth_deg));
      ul.appendChild(li);
    }
  }

  // ---- the report ----------------------------------------------------------

  renderReport() {
    const p = this.solved;
    const box = $('pl-report');
    if (!box) return;
    if (!p) {
      box.innerHTML = '';
      $('pl-az-read').textContent = '---';
      return;
    }
    $('pl-az-read').textContent = String(Math.round(p.azimuth)).padStart(3, '0') + '°T';
    const c = p.coverage;
    const rows = [
      ['Lines', `${p.lines}${p.outer_lines ? ` (${p.outer_lines} outside the box)` : ''}`],
      ['Spacing', `${Math.round(p.spacing_m)} m`],
      ['Line length', `${Math.round(p.line_length_m)} m`],
      ['Box', `${Math.round(p.box_m[0])} × ${Math.round(p.box_m[1])} m`],
      ['On the lines', `${(p.line_distance_m / 1000).toFixed(1)} km`],
      ['Turning', `${(p.turn_distance_m / 1000).toFixed(1)} km · ${p.turns}`],
      ['Total', `${(p.distance_m / 1000).toFixed(1)} km · ${(p.distance_m / 1852).toFixed(1)} NM`],
      ['Time', hms(p.seconds)],
    ];
    const bar =
      `<div class="pl-bar">` +
      `<i style="width:${(c.twice * 100).toFixed(1)}%;background:#4aa3bd"></i>` +
      `<i style="width:${((c.once - c.twice) * 100).toFixed(1)}%;background:rgba(74,163,189,.42)"></i>` +
      `<i style="width:${(c.none * 100).toFixed(1)}%;background:#c0392b"></i></div>`;

    const cov = c.none > 0.0005
      ? `<b class="bad">${(c.none * 100).toFixed(1)}% of the box is unseen</b> &mdash; the nadir gap under
         every line. Close it by bringing the spacing to <b>${Math.round(p.range_m)} m</b> or less, or by
         flying lower: the gap is ${Math.round(p.nadir_m)} m each side.`
      : `<b class="good">No holes.</b> ${Math.round(c.twice * 100)}% of the box is seen twice or more,
         the rest once.`;

    const warn = p.teardrops && p.overshoot_m > 120
      ? `<p class="pl-warn"><b>The turns need room.</b> A 180&deg; between neighbouring lines wants
         ${Math.round(p.turn_room_m)} m across track and the spacing is ${Math.round(p.spacing_m)} m, so
         the boat loops out <b>${Math.round(p.overshoot_m)} m</b> past the ends of the lines, ${p.turns}
         times. Turn tighter, or run every k-th line and fill in.</p>`
      : '';

    box.innerHTML =
      `<dl class="pl-kv">${rows.map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`).join('')}` +
      `<dt>Turns</dt><dd>${p.teardrops ? 'teardrop' : '180°'}, ${Math.round(p.overshoot_m)} m past the ends</dd></dl>` +
      bar + `<p class="pl-note">${cov}</p>` + warn +
      `<table class="pl-tg"><thead><tr><th>Target</th><th>Looks</th><th>Aspect</th><th></th></tr></thead><tbody>` +
      p.targets.map((t) =>
        `<tr><td>${esc(t.name)}</td><td class="n">${t.looks}</td>` +
        `<td>${t.looks ? (t.both_aspects ? 'both' : 'one side') : '&mdash;'}</td>` +
        `<td class="r"><span class="pl-pill ${t.verdict}">${t.verdict}</span></td></tr>`).join('') +
      `</tbody></table>` +
      `<p class="pl-note pl-digest">${esc(p.digest)}</p>`;
  }

  // ---- wiring --------------------------------------------------------------

  _bind() {
    for (const id of ['pl-range', 'pl-alt', 'pl-nadk', 'pl-spacing', 'pl-lay', 'pl-gps',
                      'pl-runin', 'pl-speed', 'pl-turn', 'pl-dead', 'pl-margin',
                      'pl-step']) {
      const el = $(id);
      if (el) el.addEventListener('input', () => { this.readSettings(); this._queue(); });
    }
    for (const g of ['pl-regime', 'pl-order', 'pl-dir']) {
      const el = $(g);
      if (el) el.addEventListener('change', () => { this.readSettings(); this._queue(); });
    }
    for (const b of document.querySelectorAll('#pl-rot button')) {
      b.addEventListener('click', () => {
        const step = Number(b.dataset.d);
        const list = this.quadrant.map((q) => q.azimuth_deg);
        if (!list.length) return;
        const i = list.findIndex((a) => Math.abs(a - this.az) < 1e-6);
        const j = Math.min(list.length - 1, Math.max(0, (i < 0 ? 0 : i) + step));
        this.select(list[j]);
      });
    }
    $('pl-t-all')?.addEventListener('click', () =>
      { this.chosen = this.contacts.map((c) => c.id); this.refresh(); });
    $('pl-t-datums')?.addEventListener('click', () =>
      { this.chosen = this.contacts.filter((c) => c.source === 'datum').map((c) => c.id); this.refresh(); });
    $('pl-t-none')?.addEventListener('click', () => { this.chosen = []; this.refresh(); });
    for (const b of document.querySelectorAll('#pl-form button')) {
      b.addEventListener('click', () => {
        this.form = b.dataset.v;
        for (const o of document.querySelectorAll('#pl-form button')) {
          o.classList.toggle('on', o.dataset.v === this.form);
        }
      });
      b.classList.toggle('on', b.dataset.v === this.form);
    }
    for (const [id, key] of [['pl-L-cov', 'cov'], ['pl-L-turn', 'turn'],
                             ['pl-L-run', 'run'], ['pl-L-num', 'num']]) {
      const el = $(id);
      if (!el) continue;
      el.addEventListener('change', () => {
        this.layers[key] = el.checked;
        this.onlayers({ ...this.layers });
      });
    }
    $('pl-fit')?.addEventListener('click', () => {
      if (this.solved?.bounds) this.onbounds(this.solved.bounds);
    });
    $('pl-export')?.addEventListener('click', () => this.exportAll());
    // Choosing one plan is the gesture that means "this is the one we are
    // running", so it goes onto the chart as a layer as well as into the
    // exports directory -- which is also how it reaches the report, with no
    // second rendering path of its own.
    $('pl-export-one')?.addEventListener('click', async () => {
      const r = await this.exportAll([this.az]);
      const f = r?.files?.[0];
      if (f) await this.onimport(f);
    });
  }

  /// Settings changes arrive per keystroke; the solve is one request behind.
  _queue() {
    clearTimeout(this._t);
    this._t = setTimeout(() => this.refresh().catch((e) => this.onmsg(String(e.message || e), 'warn')), 250);
  }

  async exportAll(azimuths) {
    try {
      const r = await api.exportPlan({
        azimuths: azimuths || this.quadrant.map((q) => q.azimuth_deg),
        track: this.form !== 'route',
        routes: this.form !== 'trace',
        targets: $('pl-wpt')?.checked ?? true,
        line_waypoints: $('pl-marks')?.checked || false,
      });
      const n = r.files.length;
      this.onmsg(`${n} plan${n === 1 ? '' : 's'} written to ${r.dir}`);
      return r;
    } catch (e) {
      this.onmsg(String(e.message || e), 'warn');
      return null;
    }
  }
}

// ---- small helpers ---------------------------------------------------------

function hm(s) {
  const m = Math.round(s / 60);
  return m >= 60 ? `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}` : `${m}m`;
}
function hms(s) {
  const m = Math.round(s / 60);
  return m >= 60 ? `${Math.floor(m / 60)} h ${String(m % 60).padStart(2, '0')} min` : `${m} min`;
}
function verdictWords(v) {
  return v === 'good' ? 'every target seen twice, from both sides'
    : v === 'thin' ? 'seen, but not well enough to classify from'
    : 'a target is not covered at all';
}
function esc(s) {
  return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}
