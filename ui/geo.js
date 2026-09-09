// Web Mercator, shared with the Rust side. Kept small and exact: the map, the
// scale bar and the snapshot all measure with these, so they agree.

export const TILE = 256;
export const D2R = Math.PI / 180;
export const R2D = 180 / Math.PI;

export function lonLatToPx(lon, lat, z) {
  const n = TILE * Math.pow(2, z);
  const s = Math.min(Math.max(Math.sin(lat * D2R), -0.9999999), 0.9999999);
  return [
    (lon + 180) / 360 * n,
    (0.5 - Math.log((1 + s) / (1 - s)) / (4 * Math.PI)) * n,
  ];
}

export function pxToLonLat(x, y, z) {
  const n = TILE * Math.pow(2, z);
  return [
    x / n * 360 - 180,
    Math.atan(Math.sinh(Math.PI * (1 - 2 * y / n))) * R2D,
  ];
}

/// Ground metres per Web Mercator pixel at this latitude.
export function metresPerPx(lat, z) {
  return 156543.03392804097 * Math.cos(lat * D2R) / Math.pow(2, z);
}

/// Metres per degree of latitude and longitude, on the local tangent plane.
export function localScale(lat) {
  const p = lat * D2R;
  return [
    111132.92 - 559.82 * Math.cos(2 * p) + 1.175 * Math.cos(4 * p) - 0.0023 * Math.cos(6 * p),
    111412.84 * Math.cos(p) - 93.5 * Math.cos(3 * p) + 0.118 * Math.cos(5 * p),
  ];
}

/// Distance on the local tangent plane, the exact inverse of `offset`.
///
/// Not a haversine: a sphere of mean radius understates the radius of the
/// parallel at 52 N by 0.32 %, so a point placed by `offset` and measured back
/// by haversine comes up 12 cm short across a swath and three metres short
/// across a kilometre. The Rust side uses the same series.
export function haversine(a, b) {
  const [mLat, mLon] = localScale((a[0] + b[0]) / 2);
  return Math.hypot((b[0] - a[0]) * mLat, (b[1] - a[1]) * mLon);
}

export function bearing(a, b) {
  const p1 = a[0] * D2R, p2 = b[0] * D2R, dl = (b[1] - a[1]) * D2R;
  const y = Math.sin(dl) * Math.cos(p2);
  const x = Math.cos(p1) * Math.sin(p2) - Math.sin(p1) * Math.cos(p2) * Math.cos(dl);
  return (Math.atan2(y, x) * R2D + 360) % 360;
}

/// Move `dist` metres on `brg` from a point.
export function offset(lat, lon, brg, dist) {
  const [mLat, mLon] = localScale(lat);
  const b = brg * D2R;
  return [lat + dist * Math.cos(b) / mLat, lon + dist * Math.sin(b) / mLon];
}

/// A round number of metres that fits in `maxPx`, for the scale bar.
export function niceDistance(metres) {
  const pow = Math.pow(10, Math.floor(Math.log10(metres)));
  for (const m of [5, 2, 1]) if (pow * m <= metres) return pow * m;
  return pow;
}

export function formatDistance(m) {
  if (m >= 1000) return `${(m / 1000).toFixed(m >= 10000 ? 0 : 2)} km`;
  if (m >= 1) return `${m.toFixed(m >= 100 ? 0 : 1)} m`;
  return `${(m * 100).toFixed(0)} cm`;
}
