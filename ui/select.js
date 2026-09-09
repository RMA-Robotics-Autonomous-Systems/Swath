// Our own popup for every `<select>` on the page.
//
// The closed control is still the native element -- `appearance: none` in the
// stylesheet already took its look away from the platform -- and only the list
// that drops out of it is replaced. That is the part that was wrong: the native
// menulist opens as its own window, positioned by the compositor, and on a
// fractionally-scaled display it landed at the top of the screen instead of
// under the control. A page cannot reach that. It is also the part a page can
// draw better, because a native popup on Linux takes no styling at all.
//
// The `<select>` remains the state. Its value, its options, its `change` event
// and every handler bound to it are untouched, so nothing else in the
// application knows this exists -- including selects built at runtime, which is
// why this listens on the document rather than upgrading elements.

let pop = null;

/// Selects this replaces. A multiple or sized select is a list box, not a
/// dropdown, and the browser draws it in the page where it belongs.
function isDropdown(el) {
  return el && el.tagName === 'SELECT' && !el.disabled && !el.multiple && el.size <= 1;
}

export function installSelects(doc = document) {
  // `pointerdown` rather than `mousedown`, so a touch opens this list too:
  // preventing the pointer event is also what stops the compatibility mouse
  // events, and with them the native popup.
  doc.addEventListener('pointerdown', (e) => {
    const sel = e.target.closest ? e.target.closest('select') : null;
    if (!isDropdown(sel)) return;
    e.preventDefault();
    if (pop && pop.select === sel) { close(); return; }
    open(sel);
  }, true);

  // Belt and braces for anything that reaches the control as a mouse event
  // without a pointer event first. It only suppresses; the opening is above, so
  // one press cannot open the list twice.
  doc.addEventListener('mousedown', (e) => {
    const sel = e.target.closest ? e.target.closest('select') : null;
    if (isDropdown(sel)) e.preventDefault();
  }, true);

  // A press that starts anywhere else closes it. The select itself is excluded
  // because that press is the one that just opened it.
  doc.addEventListener('pointerdown', (e) => {
    if (!pop) return;
    if (e.target === pop.select || pop.el.contains(e.target)) return;
    close();
  }, true);

  doc.addEventListener('keydown', onKey, true);
  window.addEventListener('resize', close);
  // Capture, so a scroll of any pane moves the control out from under the
  // popup and the popup goes with it.
  window.addEventListener('scroll', close, true);
}

function open(sel) {
  close();
  const el = document.createElement('div');
  el.className = 'sel-pop';
  const rows = [...sel.options].map((o, i) => {
    const row = document.createElement('div');
    row.className = 'sel-opt';
    row.textContent = o.textContent;
    if (o.title) row.title = o.title;
    if (o.disabled) row.classList.add('off');
    if (i === sel.selectedIndex) row.classList.add('on');
    // `mouseup` rather than `click`, so both ways of using a dropdown work: a
    // press that drags down the list and releases on a row, and a click that
    // opens it followed by a second click on a row.
    row.addEventListener('mouseup', (e) => {
      e.preventDefault();
      if (!o.disabled) choose(i);
    });
    row.addEventListener('mousemove', () => highlight(i));
    el.appendChild(row);
    return row;
  });
  // A modal dialog is in the top layer, and a popup parented to the body would
  // be painted underneath it.
  (sel.closest('dialog') || document.body).appendChild(el);

  pop = { select: sel, el, rows, at: sel.selectedIndex };
  place(sel, el);
  highlight(sel.selectedIndex);
  sel.focus({ preventScroll: true });
}

/// Under the control if it fits, above it if it does not, and never taller than
/// the room it has.
function place(sel, el) {
  const r = sel.getBoundingClientRect();
  const gap = 2;
  const below = window.innerHeight - r.bottom - gap - 6;
  const above = r.top - gap - 6;
  const up = el.offsetHeight > below && above > below;
  el.style.minWidth = `${Math.round(r.width)}px`;
  el.style.maxHeight = `${Math.max(80, Math.round(Math.min(320, up ? above : below)))}px`;
  el.style.left = `${Math.round(Math.min(r.left, window.innerWidth - el.offsetWidth - 6))}px`;
  if (up) el.style.bottom = `${Math.round(window.innerHeight - r.top + gap)}px`;
  else el.style.top = `${Math.round(r.bottom + gap)}px`;
}

function highlight(i) {
  if (!pop || i < 0 || i >= pop.rows.length) return;
  pop.rows.forEach((r, j) => r.classList.toggle('at', j === i));
  pop.at = i;
  pop.rows[i].scrollIntoView({ block: 'nearest' });
}

function choose(i) {
  if (!pop) return;
  const sel = pop.select;
  close();
  if (i === sel.selectedIndex) return;
  sel.selectedIndex = i;
  // The whole point of keeping the native element: everything downstream is
  // listening for this.
  sel.dispatchEvent(new Event('input', { bubbles: true }));
  sel.dispatchEvent(new Event('change', { bubbles: true }));
}

/// The next option that can be chosen, skipping disabled ones and stopping at
/// the ends rather than wrapping -- which is what a native dropdown does, and
/// what stops a held arrow key from cycling forever.
///
/// Takes the options rather than reading them, so it can be tested without a
/// document.
export function nextEnabled(options, from, dir) {
  for (let i = from + dir; i >= 0 && i < options.length; i += dir) {
    if (!options[i].disabled) return i;
  }
  // Where we started, unless that is itself unchoosable -- an empty list, or a
  // group label sitting where the selection is.
  return from >= 0 && from < options.length && !options[from].disabled ? from : -1;
}

function step(from, dir) {
  return nextEnabled(pop.select.options, from, dir);
}

function onKey(e) {
  if (!pop) return;
  const keys = ['Escape', 'ArrowDown', 'ArrowUp', 'Home', 'End', 'Enter', ' ', 'Tab'];
  if (!keys.includes(e.key)) return;
  e.preventDefault();
  e.stopPropagation();
  if (e.key === 'Escape' || e.key === 'Tab') return close();
  if (e.key === 'Enter' || e.key === ' ') return pop.at >= 0 ? choose(pop.at) : close();
  if (e.key === 'ArrowDown') return highlight(step(pop.at, 1));
  if (e.key === 'ArrowUp') return highlight(step(pop.at, -1));
  if (e.key === 'Home') return highlight(step(-1, 1));
  if (e.key === 'End') return highlight(step(pop.rows.length, -1));
}

export function close() {
  if (!pop) return;
  pop.el.remove();
  pop = null;
}

/// For the tests: is a popup open, and on what?
export function openOn() {
  return pop ? pop.select : null;
}
