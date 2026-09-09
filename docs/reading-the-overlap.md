# Reading the overlap — separating the passes again

A handoff. The question that started it: the chart is hard to read where
coverage overlaps, and would a georeferenced waterfall — or one WebGL panel
holding both views — fix it.

Short answer: there are **two** overlap problems, they have different causes,
and only one of them is a compositing question. The plan below does the cheap
one first because it also tests the diagnosis for the expensive one.

## Two problems, not one

### A. Between layers — the viewer flattens the stack

`projects/wpa20260906` carries eight mosaic layers: four recordings by two
channels. They are painted in tree order in
[`MapView.draw`](../ui/map.js), one `drawTileLayer` per layer at
`globalAlpha = l.opacity`.

Unpainted ground is not the problem. `Mosaic::tile` writes `alpha = 255` only
where `cover[i] > 0`, so a mosaic tile is transparent everywhere it has no
data and layers occlude each other **only on genuine overlap**. That is the
right behaviour and it should stay.

What is wrong is the rule applied there. Full opacity means the top layer wins
outright and the ones under it may as well not be loaded; reduced opacity
averages pictures that disagree, which is the same mistake the mosaic builder
used to make in the raster, moved into the compositor. Neither answers the
question the operator is actually asking, which is *which pass am I looking
at*.

This one is fixable in the viewer, today.

### B. Within a mosaic — the passes are already averaged

`Loaded.mosaics` is keyed on `u8`, the subsystem, so there is exactly one
raster per `(dataset, channel)`. `Mosaic::build` walks every ping of that
channel into one `sum`/`wgt` grid, with `shape_priority(p, exponent)` — default
exponent 8 — deciding the blend. By the time a tile reaches the browser the ten
run lines are one picture.

No compositing rule can undo that. And three things the weighted average hides
are exactly what makes the result unreadable:

- **Shadows cancel.** Reciprocal courses throw shadows in opposite directions.
  Blending them destroys the cue that says "target, and this tall" — the point
  [mosaicking.md](mosaicking.md) already makes about averaging, applied to what
  survives the priority weighting rather than to a plain mean.
- **Nav disagreement is hidden, not shown.** Positions are good to 5–10 m, so a
  feature lands twice and blurs into one soft blob instead of two honest
  answers.
- **The winner flips cell to cell** where grazing angles are close, giving
  texture that is a property of the lawnmower pattern rather than of the seabed.

Note what is *not* on that list: inter-line misregistration correctable by
texture matching. `tools/register.py` implements it and it was measured on this
survey — real line pairs score 3.5–4.7 against 38–52 for the injected-shift
control, with no peak in the correlation surface. See "Inter-line registration:
measured, and not possible on this survey" in [mosaicking.md](mosaicking.md).
Do not re-litigate that; a survey with real morphology should register normally
and the same tool will do it.

## The plan, in the order it should be done

### 1. Heading-split mosaics — a day, no new rendering

`MosaicConfig` already has `look_window_deg` and `look_centre_deg`, both marked
EXPERIMENT and both defaulting to off. Build two mosaics per channel, one per
reciprocal heading, and put both in the tree. The stack already does z-order and
the hint in the panel already says so: "Top of the list draws over the rest."

Everything needed is plumbing: expose the two fields in the mosaic settings
dialog, and let a recording contribute two mosaic nodes instead of one. The
fingerprint in `mosaic::key` already covers them, so the two builds get distinct
files and distinct tile URLs with no cache work.

**This is the experiment.** If two heading-split rasters read decisively better
than one merged raster, stages 2–4 are worth their cost. If they do not, the
diagnosis above is wrong and the rest of this document should be thrown away.

### 2. Per-line mosaics, and soloing the line you are reading

`nav::detect_lines` already returns `Segment { kind, t0, t1, index0, index1,
course, length_m }`, and `GET /api/dataset/{name}/lines` already serves them —
though it drops `index0`/`index1` on the way out, which is precisely what a
per-line build needs. Add them.

Then:

- `Mosaic::build` filters `sel` by a segment's record range. The bounds pass
  right below it already derives the raster extent from the selected records,
  so a per-line mosaic tightens to its own line for free.
- `Loaded.mosaics` keys on `(u8, Option<usize>)` and `Workspace::mosaic_path`
  grows a line component; `mosaics_for`/`prune_mosaics` follow the prefix
  change.
- The tile route `GET /api/mosaic/{name}/{sub}/{z}/{x}/{y}` gains a line
  segment, or takes it in the query beside the style.
- The tree gets one node per line, `parent` set to the channel's mosaic node.
  Visibility inheritance, opacity and drag-reorder all already work on nested
  nodes.
- `showViewSpan` in `app.js` already walks the rows the waterfall is showing.
  Resolve those rows' `time` to a segment, raise that line's layer, dim the
  rest. **That is the Z-index idea, and at this point it is about fifteen lines
  of JavaScript.**

Cost is disk. With per-line bounds the total area is roughly the survey area
times the mean coverage, so call it 2–3× one mosaic; `mosaic_20.wpm` is 35 MB
on `070926_measures_b2`, so ~90 MB per channel. Acceptable. `prune_mosaics`
will need to reason per line so a rebuild does not evict a sibling.

### 3. Ribbons — the waterfall drawn on the chart

The transform is already written. `RowInfo` carries `fish_lat`, `fish_lon`,
`bearing`, `altitude`, `half_width_m` and `advance_m` per row, and
`Waterfall::pixel_to_world` is the per-vertex maths verbatim: a row is one quad
with endpoints at ground range `±half_width_m` on `bearing + 90`. A triangle
strip per line, the waterfall block as its texture.

**Justify it on interactivity, not on readability** — readability is stage 2's
job. What ribbons buy that a baked raster cannot:

- Layback, `gps_to_towpoint_m` and sound speed stop being a rebuild — minutes,
  and a new fingerprinted 35 MB file — and become uniforms. An operator can
  slide layback and watch two passes come into register on a wreck. Texture
  correlation cannot do that here (stage B above); a human looking at a target
  seen twice can.
- Gain, ramp and stretch likewise move to the GPU and become instant.

Three things that will bite, in the order they will bite:

- **Gain continuity is the real work, and it is not shader work.** `equalise`
  runs along track carrying a `tail` across chunk boundaries, and the contrast
  stretch is a percentile taken over the whole survey's painted cells. A
  waterfall block is gained per block. Texture blocks straight onto the chart
  and there is a visible level step at every block seam and every line
  boundary. Ribbons need the survey-wide stretch as a uniform and the
  along-track equalisation moved into row metadata rather than baked into
  pixels. Budget most of the stage here.
- **The slant axis is not linear across a quad.** Texture coordinates
  interpolate linearly, which is correct in ground mode and wrong in slant,
  where the mapping is `sqrt`. Either render ribbons only from a ground-axis
  waterfall, or do `Axis::to_ground` per fragment with altitude interpolated
  along the row. Cheap either way — but decide it rather than discover it.
- **Texture budget.** A 1 km line at 6–10 cm per ping is 10 000–16 000 rows,
  past the usual 8192 texture limit, so it stays block-based — which the
  waterfall already is — plus a stride-based LOD by zoom. Ten lines at full
  stride is on the order of 130 MB of grey texture, needing the eviction
  discipline `TileStore` learned the hard way; read the `TILE_CACHE` comment in
  [map.js](../ui/map.js) before writing the first `texImage2D`.

### 4. The morph, last

Once ribbons exist the morph between chart and waterfall is nearly free: same
texture, same index buffer, two position attributes — geographic, and unrolled
into row/across — lerped on one uniform in the vertex shader. Perhaps fifty
lines.

**Do not rebuild the UI around it.** The two views want different interaction
and different chrome: the waterfall scrolls by row and needs `aspectFactor`'s
true-scale stretch, the chart pans geographically with a scale bar and a north
arrow. Halfway through a morph neither set of controls means anything. It is a
way to travel between two panes that stay, not a replacement for having two.

## If stage 3 happens, how WebGL should enter

Keep `TileStore` exactly as it is and move only `drawTileLayer` to textured
quads. The loading logic — the two request budgets, the ancestor fallback, the
retry-then-rest schedule, and the blob-over-`ImageBitmap` decision that the
`TILE_CACHE` comment explains — does not change at all; `drawImage` becomes an
upload and a quad. On the order of 200 lines, one canvas, and the tree's
ordering semantics survive.

The tempting shortcut is a transparent WebGL canvas over the existing 2D one.
It is quicker and it permanently pins the sonar above every imported raster,
which breaks the promise the tree makes when it invites you to drag layers into
an order. Do not take it.

## Open questions

- **Does stage 1 actually read better?** Unmeasured. Everything downstream
  assumes yes.
- **What is the right default when several lines overlap and none is selected?**
  Best-look-wins per cell is what the raster does now. Newest-on-top, or
  highest-priority-line-on-top, are both defensible and neither has been tried.
- **Should the `cover` band be visible?** It is already carried in every mosaic
  and never shown. Painting overlap count as a layer would tell the operator
  where the ambiguity is, which is a smaller change than any of the above and
  might absorb some of the complaint on its own.
- **Per-line radiometry.** Stage 2 gives each line its own percentile stretch
  unless told otherwise, which will make adjacent lines differ in tone. The
  stretch should probably be computed once over the whole channel and passed
  into the per-line builds.

## State of the tree at handoff

Branch `new-process`, at `fe30fcd`. Nothing here has been started — this is a
plan, not a partial implementation. Uncommitted: three `project.json` files
with layer-stack changes, an untracked `4125/`, three untracked projects, a
`marks.geojson`, and `crates/swath-core/tests/zz.rs`.

One drift worth knowing: [mosaicking.md](mosaicking.md) documents
`tools/mosaic.py`, the Python prototype. The implementation the viewer uses is
`crates/swath-core/src/mosaic.rs`. The physics in that note still
holds — the file paths in it do not.
