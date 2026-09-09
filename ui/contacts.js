// What colour a contact is drawn in.
//
// One amber pin for everything was fine while a project held six marks. It is
// not fine on a search: the operator wants to know, at a glance across a chart
// with forty marks on it, which of them are the ordnance and which are the
// tyres. So the colour comes from the classification -- the one field that
// already says what the thing is -- rather than from a colour picker nobody
// would keep consistent between two days' work.
//
// Derived, not stored. Retype the class and the pin follows, on the chart, in
// the list and on the waterfall, with nothing to migrate and no way for the
// swatch beside a contact to disagree with the word next to it. The colour is
// still written onto the contact when it is saved, so the GeoJSON a client
// opens in their own software carries it too.

/// The groups, in the order they are asked.
///
/// Order is half the rule, and it is not arbitrary. `non-mine-like` has to be
/// looked at before `mine-like`, and `mine-like` before `mine`, or a NOMBO and
/// a MILCO both come out the colour of live ordnance -- the one mistake on
/// this list that would actually cost somebody something.
export const CLASS_GROUPS = [
  {
    key: 'cleared',
    label: 'not a target',
    colour: '#78889a',
    words: ['nombo', 'non mine', 'nonmine', 'not a mine', 'no contact', 'nothing',
            'cleared', 'clear', 'false', 'discounted', 'rejected', 'noise', 'artefact'],
  },
  {
    key: 'suspect',
    label: 'mine-like',
    colour: '#ff7a1a',
    words: ['mine like', 'minelike', 'milco', 'milec', 'mine echo', 'suspect',
            'suspected', 'possible', 'probable', 'contact of interest', 'poi',
            'unknown'],
  },
  {
    key: 'ordnance',
    label: 'ordnance',
    colour: '#ff3b30',
    words: ['mine', 'uxo', 'ordnance', 'munition', 'shell', 'bomb', 'torpedo',
            'depth charge', 'explosive', 'eod', 'projectile'],
  },
  {
    key: 'wreck',
    label: 'wreck',
    colour: '#c77dff',
    words: ['wreck', 'hull', 'vessel', 'aircraft', 'plane', 'barge', 'boat'],
  },
  {
    key: 'utility',
    label: 'man-made line',
    colour: '#3ec8ff',
    words: ['pipe', 'pipeline', 'cable', 'chain', 'rope', 'net', 'netting',
            'mooring', 'anchor', 'umbilical', 'wire'],
  },
  {
    key: 'debris',
    label: 'debris',
    colour: '#f0b429',
    words: ['debris', 'scrap', 'junk', 'rubbish', 'litter', 'tyre', 'tire',
            'drum', 'container', 'trolley', 'metal', 'object'],
  },
  {
    key: 'geology',
    label: 'geology',
    colour: '#9fb0bf',
    words: ['boulder', 'rock', 'rocky', 'stone', 'outcrop', 'reef', 'cobble'],
  },
  {
    key: 'seabed',
    label: 'seabed',
    colour: '#b3814d',
    words: ['scour', 'sand', 'sandwave', 'sand wave', 'ripple', 'sediment',
            'mud', 'clay', 'gravel', 'trawl', 'trawl mark', 'depression',
            'pockmark', 'dredge', 'seabed feature', 'feature'],
  },
  {
    key: 'biology',
    label: 'biology',
    colour: '#5fd38d',
    words: ['weed', 'seaweed', 'kelp', 'vegetation', 'fish', 'shoal', 'coral',
            'biological', 'biology'],
  },
  {
    key: 'area',
    label: 'area',
    colour: '#7aa7ff',
    words: ['area', 'area of interest', 'aoi', 'box', 'zone', 'datum', 'search'],
  },
];

/// The pin for a contact with no classification at all, which is also what the
/// viewer has always drawn. Changing it would repaint every old project.
export const UNCLASSED = '#f0b429';

/// A class that matches nothing known still gets its own colour, and the same
/// one every time. Deliberately nothing near the ordnance red: a word this
/// table has never heard of must not be able to come out looking like a mine.
const FALLBACK = ['#4fa3e8', '#8f7ae0', '#35b8a6', '#7cc45a', '#c9a227', '#5ec8c0'];

/// Punctuation and case are not distinctions here: `MILCO`, `milco` and
/// `mine-like (MILCO)` are one answer.
function norm(s) {
  return String(s || '').toLowerCase().replace(/[^a-z0-9]+/g, ' ').trim();
}

/// A plural is the same word.
function stem(w) {
  return w.length > 3 && w.endsWith('s') ? w.slice(0, -1) : w;
}

/// Whole words, not substrings.
///
/// `magnetic anomaly` contains the letters of `net`, and a substring test put
/// it on the chart as a mooring. A single word matches a whole word of the
/// class; a phrase has to appear as written.
function matches(k, w) {
  if (w.includes(' ')) return k.includes(w);
  return k.split(' ').some(t => stem(t) === stem(w));
}

/// The group a class name falls in, or null.
export function classGroup(name) {
  const k = norm(name);
  if (!k) return null;
  for (const g of CLASS_GROUPS) {
    if (g.words.some(w => matches(k, w))) return g;
  }
  return null;
}

/// Stable across sessions and across machines, which is the only property that
/// matters: the same word has to come back the same colour tomorrow.
function hash(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

/// The colour for a class name on its own -- for a legend, or for the swatch
/// beside the field while it is being typed.
export function classColour(name) {
  const g = classGroup(name);
  if (g) return g.colour;
  const k = norm(name);
  return k ? FALLBACK[hash(k) % FALLBACK.length] : UNCLASSED;
}

/// The colour to draw a contact in.
///
/// An unclassified contact keeps whatever colour it was given -- a project that
/// arrived from somewhere else with colours in it is not overruled for the sake
/// of a field it never filled in.
export function contactColour(c) {
  if (!c) return UNCLASSED;
  return norm(c.class) ? classColour(c.class) : (c.colour || UNCLASSED);
}

/// What the swatch means, in words, for a tooltip.
export function classTitle(name) {
  const n = String(name || '').trim();
  if (!n) return 'unclassified';
  const g = classGroup(n);
  return g && norm(g.label) !== norm(n) ? `${n} — ${g.label}` : n;
}
