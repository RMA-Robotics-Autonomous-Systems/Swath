// Thin wrapper over the router. Every call goes to the same endpoints whether
// the app is running in the desktop shell or in a browser, because the shell
// is pointed at this same server.

async function req(method, path, body, opts = {}) {
  const init = { method, headers: {} };
  if (body !== undefined) {
    if (body instanceof Uint8Array || body instanceof ArrayBuffer) {
      init.body = body;
      init.headers['Content-Type'] = 'application/octet-stream';
    } else {
      init.body = JSON.stringify(body);
      init.headers['Content-Type'] = 'application/json';
    }
  }
  const r = await fetch(path, init);
  if (opts.raw) return r;
  const text = await r.text();
  let data;
  try { data = text ? JSON.parse(text) : null; } catch { data = { error: text }; }
  if (!r.ok) throw new Error((data && data.error) || `${r.status} ${path}`);
  return data;
}

export const api = {
  state:        ()             => req('GET', '/api/state'),
  openProject:  (name)         => req('POST', '/api/project/open', { name }),
  projects:     ()             => req('GET', '/api/projects'),
  renameProject:(from, to)     => req('POST', '/api/project/rename', { from, to }),
  copyProject:  (from, to)     => req('POST', '/api/project/duplicate', { from, to }),
  // `withData` is the difference between losing a list and losing a recording:
  // the server refuses without it while the project holds files of its own.
  deleteProject:(name, withData = false) =>
                  req('POST', '/api/project/delete', { name, with_data: withData }),
  newProject:   (name)         => req('POST', '/api/project/new', { name }),
  saveProject:  (p)            => req('POST', '/api/project/save', p),

  // `mosaic` is a partial: send the settings that were chosen and the server
  // fills in the rest, so the viewer does not have to carry a second copy of
  // every build default.
  loadDataset:  (name, nav, mosaic) =>
                  req('POST', '/api/dataset/load', { name, nav, mosaic }),
  unloadDataset:(name)         => req('POST', '/api/dataset/unload', { name }),
  // No channel: there is one fish and one boat, and every band was towed by
  // the same one, so the track is a property of the recording.
  track:        (name, max = 4000) =>
                  req('GET', `/api/dataset/${encodeURIComponent(name)}/track?max=${max}`),
  lines:        (name)         => req('GET', `/api/dataset/${encodeURIComponent(name)}/lines`),
  // `time` identifies the ping outright and should be preferred wherever it is
  // known: a position picks among every row that has the contact somewhere in
  // its swath, which on a survey that crosses itself can be a different pass.
  nearest:      (name, sub, lat, lon, time) =>
                  req('GET', `/api/dataset/${encodeURIComponent(name)}/nearest` +
                             `?subsystem=${sub}&lat=${lat}&lon=${lon}` +
                             (time != null ? `&time=${time}` : '')),

  waterfall:    (r)            => req('POST', '/api/waterfall', r),
  pick:         (key, x, y)    => req('POST', '/api/waterfall/pick', { key, x, y }),

  buildMosaic:  (dataset, subsystem, force = false) =>
                  req('POST', '/api/mosaic/build', { dataset, subsystem, force }),

  // `want=recordings` asks for folders that hold sonar rather than files that
  // can be imported as a layer. The same picker serves both.
  browse:       (path, want)   => req('GET', `/api/browse?path=${encodeURIComponent(path || '')}` +
                                            (want ? `&want=${encodeURIComponent(want)}` : '')),
  importLayer:  (path, label)  => req('POST', '/api/layers/import', { path, label }),
  // Bring a folder of recordings into the open project, by copy, move or link.
  importDataset:(path, name, mode) =>
                  req('POST', '/api/dataset/import', { path, name, mode }),
  removeDataset:(name)         => req('POST', '/api/dataset/remove', { name }),
  freeDatasetName: (want)      =>
                  req('GET', `/api/dataset/name?want=${encodeURIComponent(want || '')}`),
  layerFeatures:(id)           => req('GET', `/api/layers/${encodeURIComponent(id)}/features`),
  layerSample:  (id, lat, lon) =>
                  req('GET', `/api/layers/${encodeURIComponent(id)}/sample?lat=${lat}&lon=${lon}`),
  ramps:        ()             => req('GET', '/api/ramps'),
  reportImage:  (name, bytes)  =>
                  req('POST', `/api/report/image/${encodeURIComponent(name)}`, bytes),
  // Writes the page and asks the desktop to open it. The desktop shell has no
  // tab to open, so the file is the deliverable and the open is a courtesy.
  openReport:   ()             => req('POST', '/api/report/open'),

  // The plan lives on the project, so these carry no state of their own: the
  // server already knows which contacts the search is for.
  plan:         ()             => req('GET', '/api/plan'),
  savePlan:     (p)            => req('POST', '/api/plan', p),
  solvePlan:    (az)           => req('GET', `/api/plan/solve?az=${az}`),
  exportPlan:   (o)            => req('POST', '/api/plan/export', o),

  contacts:     ()             => req('GET', '/api/contacts'),
  saveContact:  (c)            => req('POST', '/api/contacts', c),
  deleteContact:(id)           => req('DELETE', `/api/contacts/${encodeURIComponent(id)}`),
  // `kind` is one of map, waterfall, lf, hf. It used to be passed through only
  // when it was 'waterfall', so a band crop went up with no kind at all -- and
  // the server reads a missing kind as the chart crop. Every low- and
  // high-frequency snapshot ever taken was therefore filed as the chart one,
  // overwriting it, and the report's band panes stayed empty because nothing
  // was ever stored in them.
  putSnapshot:  (id, bytes, kind) =>
                  req('POST', `/api/contacts/${encodeURIComponent(id)}/snapshot`
                              + (kind && kind !== 'map'
                                 ? `?kind=${encodeURIComponent(kind)}` : ''), bytes),

  convert:      (lat, lon, epsg) =>
                  req('GET', `/api/convert?lat=${lat}&lon=${lon}` + (epsg ? `&epsg=${epsg.join(',')}` : '')),
  crs:          (lat, lon)     => req('GET', `/api/crs?lat=${lat}&lon=${lon}`),
  measure:      (a, b)         =>
                  req('GET', `/api/measure?lat1=${a[0]}&lon1=${a[1]}&lat2=${b[0]}&lon2=${b[1]}`),
};

export const tileUrl = {
  base:   (layer, z, x, y) => `/api/basemap/${layer}/${z}/${x}/${y}.png`,
  // `rev` is the digest of the settings the mosaic was painted with. It does
  // nothing on the server; it is there so that re-solving the navigation gives
  // every tile a new URL, and the browser fetches the imagery that matches the
  // track now drawn over it instead of the one it cached an hour ago.
  mosaic: (ds, sub, z, x, y, rev, style) => {
    const q = new URLSearchParams();
    if (rev) q.set('v', rev);
    if (style) {
      if (style.ramp && style.ramp !== 'grey') q.set('ramp', style.ramp);
      if (style.reverse) q.set('reverse', '1');
      if (style.lo) q.set('lo', style.lo);
      if (style.hi != null && style.hi !== 255) q.set('hi', style.hi);
    }
    const s = q.toString();
    return `/api/mosaic/${encodeURIComponent(ds)}/${sub}/${z}/${x}/${y}.png` +
           (s ? `?${s}` : '');
  },
  // The whole style is in the query, so the URL names exactly one picture: a
  // change of ramp is a different URL rather than the same one gone stale.
  layer: (id, z, x, y, style) =>
    `/api/layer/${encodeURIComponent(id)}/${z}/${x}/${y}.png` + styleQuery(style),
};

export function styleQuery(st) {
  if (!st) return '';
  const q = new URLSearchParams();
  if (st.ramp) q.set('ramp', st.ramp);
  if (st.reverse) q.set('reverse', '1');
  if (st.min != null) q.set('min', st.min);
  if (st.max != null) q.set('max', st.max);
  if (st.shade != null) q.set('shade', st.shade);
  if (st.sun_azimuth_deg != null) q.set('azimuth', st.sun_azimuth_deg);
  if (st.sun_elevation_deg != null) q.set('elevation', st.sun_elevation_deg);
  if (st.shade_exaggeration != null) q.set('exaggeration', st.shade_exaggeration);
  const s = q.toString();
  return s ? `?${s}` : '';
}
