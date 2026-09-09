// Just enough DOM for the view classes to be constructed and driven off-screen.
//
// The point is to test the arithmetic that turns a scroll position into a row
// and a row into a position on the seabed, which is the part a screenshot
// cannot check. Nothing here pretends to draw: the 2D context records nothing
// and returns nothing, because what is being tested is where things would be
// drawn, not what they look like.

const noop = () => {};

function ctx2d() {
  return new Proxy({}, {
    get(_, k) {
      if (k === 'measureText') return () => ({ width: 0 });
      if (k === 'canvas') return null;
      return noop;
    },
    set() { return true; },
  });
}

/// A canvas that remembers what was bound to it, so a test can drive the
/// pointer the way a window would -- including the sequences a window is not
/// obliged to complete, which is where the interaction bugs live.
export function fakeCanvas(w, h) {
  const handlers = new Map();
  return {
    width: w, height: h,
    parentElement: { classList: { add: noop, remove: noop, toggle: noop } },
    getContext: ctx2d,
    getBoundingClientRect: () => ({ width: w, height: h, top: 0, left: 0 }),
    addEventListener: (type, fn) => {
      if (!handlers.has(type)) handlers.set(type, []);
      handlers.get(type).push(fn);
    },
    removeEventListener: (type, fn) => {
      const l = handlers.get(type);
      if (l) handlers.set(type, l.filter(f => f !== fn));
    },
    setPointerCapture: noop,
    releasePointerCapture: noop,
    /// Deliver an event, filling in the fields a real one always carries.
    fire(type, ev = {}) {
      for (const fn of handlers.get(type) || []) {
        fn({ type, offsetX: 0, offsetY: 0, pointerId: 1, button: 0,
             buttons: type === 'pointerdown' ? 1 : 0,
             preventDefault: noop, ...ev });
      }
    },
  };
}

export function installGlobals() {
  globalThis.window = globalThis.window || { devicePixelRatio: 1, addEventListener: noop };
  globalThis.requestAnimationFrame = globalThis.requestAnimationFrame || ((f) => { f(); return 1; });
  globalThis.cancelAnimationFrame = globalThis.cancelAnimationFrame || noop;
}

/// A block as the server returns one, with rows on a straight north-going line.
export function fakeBlock({ start, count, stride, rows, halfWidth = 50, spacing = 0.2,
                            axis = 'ground' }) {
  const n = Math.floor(count / stride);
  const info = [];
  for (let i = 0; i < n; i++) {
    const g = (start / stride) + i;          // global row index
    info.push({
      time: 1_700_000_000 + g * 0.1,
      fish_lat: 52.5 + g * spacing / 111_132.0,
      fish_lon: 4.0,
      boat_lat: 52.5 + g * spacing / 111_132.0 + 0.0004,
      boat_lon: 4.0,
      bearing: 0,
      altitude: 15,
      depth: 24,
      roll: 0,
      clean: true,
      half_width_m: halfWidth,
      advance_m: spacing,
      ping_row: start + i * stride,
    });
  }
  return {
    key: `k${start}`, start, count, stride, axis,
    width: 1024, height: n, rows: info,
    total_pings: rows, metres_per_px: (halfWidth * 2) / 1024,
    image: { width: 1024, height: n },
  };
}
