# Internals

*[README](../README.md) · [Where things are](positions.md) · [From ping to picture](imagery.md) · [Projects and the report](projects.md) · [Internals](internals.md)*

How the program is put together: where things live on disk, how the two views
stay in step, what the tests actually prove, and the decisions that are easier
to read once than to rediscover.

```
crates/
  swath-core/      parsing, navigation, geodesy, imagery, the API router
  swath-cli/       the `swath` command, and the server the viewer runs on
  swath-app/       the desktop shell (Tauri)
ui/                the frontend: plain ES modules, no build step
fixtures/          computed reference values -- no survey data lives here
scripts/           build environment and restart helpers
docs/              this, and the notes it points at
```

Building and running it is in the [README](../README.md). The geometry is in
[Where things are](positions.md), the signal chain in [From ping to
picture](imagery.md), and the project and report in [Projects and the
report](projects.md).

## Layout

One recording per folder under `data/`, everything derived from it under a
matching folder in `out/`, and projects — which combine recordings — under
`projects/`.

```
data/<name>/*.jsf                 the recording, untouched
data/*.tif  data/*.gpx            grids and tracks brought in from outside
out/<name>/ping_index.swi         the ping index
out/<name>/mosaic_<sub>_<key>.swm the georeferenced mosaic
out/layers/<key>.swl              an imported grid, resampled onto the chart
out/tilecache/                    chart tiles, shared between surveys
out/last-project                  what to reopen on start
out/fixtures/                     golden files cut from these recordings
projects/<name>/project.json      datasets, the layer stack, report settings
projects/<name>/marks.geojson     contacts, as GeoJSON
projects/<name>/snaps/<id>.png    the snapshot taken when each was marked
projects/<name>/report/<chart>.png chart views, drawn by the viewer for the report
```

`<key>` is a digest of the settings the product was built with. It is in the
file name so that two decisions are two files: change the layback and the
mosaic that comes back off disk is the one painted with the layback now in
force, not the one from an hour ago. The viewer puts the same digest in every
tile URL, so the browser's cache is invalidated by the same change.

An imported grid is referenced where it lives rather than copied into the
project — these files run to hundreds of megabytes and are usually shared — and
the resampled copy goes in `out/layers/`, keyed by the source's path, size and
modification time. Two projects over the same ground share one copy; editing
the source resamples it.

Contacts are GeoJSON so anything downstream can read them without a converter.

Those three extensions were `.wpi`, `.wpm` and `.wpl` before the application was
renamed. Both spellings are read, and only the new one is written, so a
workspace built by an older build keeps working without being converted.

## Linked views

Scrolling the waterfall pans the chart to the pings on screen; moving the chart
scrolls the waterfall to the pings that ran over the ground now in view. The
stretch of track the waterfall is showing is drawn over the fish track, so
"where am I on this line" is one glance.

The chart-to-waterfall direction goes through `GET /api/dataset/<name>/nearest`,
which answers with an ordinal in the channel's selection — the same axis the
waterfall scrolls on. It sweeps coarsely and then refines in a narrow window,
deliberately narrow: a survey line doubles back on itself, and a wider refine
would let the search jump to the neighbouring pass, which is a different place
on the image and the same place on the seabed.

If the chart is nowhere near the recording the waterfall stays where it is.
Dragging away to look at something else is not a request to lose your place.

## Scrolling the waterfall

At one row per ping a day's recording is half a million rows, so the server
renders it in blocks of 2048 and the viewer holds several either side of what
is on screen. Scrolling within them is a canvas blit; crossing into a new one
is a fetch that started a screen and a half ago.

Two things make it quick rather than merely buffered:

- **The image is not in the JSON.** `POST /api/waterfall` returns metadata and a
  URL; the PNG comes over that URL and is decoded the way the browser decodes
  any image. It used to come back inline as a base64 data URI, which put a
  megabyte and a half of text through `JSON.parse` on the main thread for every
  screenful.
- **Blocks are keyed by everything that determines their pixels**, navigation
  included, so scrolling back over ground already read costs nothing, and a
  re-solve cannot hand back a block still carrying the old fixes.

A block is about half a second of work at 1024 px wide and four screens tall at
true scale. Twenty-four are kept in memory.

## Testing against the reference

A Python reader was the oracle. It wrote golden files from the real
recordings; `crates/swath-core/tests/parity.rs` holds the Rust to them.

It is no longer in the tree, so the fixtures are **frozen**. Nothing can
regenerate them: `swath fixtures` writes the same schema out of *this* code,
which is what you want for reading a failure side by side and worthless as a
source of truth — regenerating the gate from the thing under test asserts only
that it agrees with itself. A failure is a bug here until proved otherwise.

**The fixtures live in two places, because they are two kinds of thing.**

`fixtures/` in the repository holds `proj.json` and `tiff/`: computed
reference values — projected coordinates, synthetic rasters — that belong to
no survey and to nobody. They ship with the source and their tests run on any
machine that clones it.

`<workspace>/out/fixtures/` holds `parse.json`, `nav.json` and
`tiff-external.json`: byte offsets into particular recordings, the positions of
a particular boat on a particular afternoon, and the georeferencing of a grid
covering somebody's survey area. That is survey data, not software, so it lives
with the survey. Those tests skip without it — and could not run anyway, since
each also needs the file it describes.

```sh
cargo test
bun ui/test/run.js          # the browser side, headless
```

Tests that need a recording skip when they cannot find one. Point
`SWATH_WORKSPACE` at a workspace holding `data/` and `out/` to run those; the
default is the checkout itself.

54 Rust tests and 266 headless browser checks. The browser half is not a
formality: it owns the live path for a waterfall click — nothing calls the Rust
pick route — so the across-track arithmetic exists twice and both copies are
checked against the same numbers.

`cargo test` also holds the two views against each other; see [The
invariant](positions.md#the-invariant). The fixtures below are the other half —
this code against an implementation that was not this code.

The fixtures are not equally strong, and the tests say so:

| fixture | where | what it proves | tolerance |
| --- | --- | --- | --- |
| `parse.json` | workspace | the JSF reader agrees with the Python reader, which had been read against this data for months | exact on every integer field and the first eight samples; 1e-9 relative on the trace sum |
| `proj.json` | repo | the geodesy agrees with PROJ, an implementation this code has never met | sub-mm on UTM, LAEA and Web Mercator; the datum-shift grids to the residual of their own published Helmert |
| `nav.json` | workspace | the layback pipeline was transcribed correctly | 1 cm on the boat, 5 cm on the fish |
| `tiff/*.tif` + `tiff.json` | repo | the GeoTIFF reader agrees with GDAL on fourteen encodings | exact on integers, 1e-6 relative on floats, and every pixel of every file summed |
| `tiff-external.json` | workspace | the same reader on a real 373 MB DTM, 60 probes | as above |

Only the first two are oracles. `nav.json` is one specification implemented
twice by hand: it catches transcription slips, not wrong ideas, because both
sides would share those.

The harness earned itself back on the first run. The transverse Mercator
*forward* matched PROJ to a micron while the *inverse* was 524 m out — a wrong
Newton derivative, invisible to any test that only projected forwards.

Current numbers:

```
parse parity: 1282 pings across the sample
EPSG:4326   worst disagreement with PROJ 0.0000 m
EPSG:3857   worst disagreement with PROJ 0.0000 m
EPSG:32631  worst disagreement with PROJ 0.0000 m
EPSG:25831  worst disagreement with PROJ 0.0001 m
EPSG:3035   worst disagreement with PROJ 0.0000 m
EPSG:28992  worst disagreement with PROJ 0.0048 m
EPSG:23031  worst disagreement with PROJ 0.6710 m
070926_measures:      501 fixes · boat worst 0.0000 m · fish worst 0.0000 m
070926_measures_b2:   502 fixes · boat worst 0.0000 m · fish worst 0.0000 m
070926_measures_star: 502 fixes · boat worst 0.0000 m · fish worst 0.0000 m
wpa20260906:          501 fixes · boat worst 0.0000 m · fish worst 0.0000 m
```

The fixtures cannot be regenerated any more, and never could be regenerated to
make a failing test pass.

## Chart tiles

The viewer proxies its base map through a disk cache at `out/tilecache/`, in
the same `<layer>/<z>/<x>/<y>.png` layout the Python viewer used — so the tiles
already fetched are still there and still served. Five sources are allowed and
nothing else is fetched: `osm`, `osmde`, `topo`, `seamark`, `contour`.

Over open water the useful layer is not the base map. OpenStreetMap has nothing
to draw 5 km offshore, so from about z14 up the chart is a flat blue rectangle;
6% of the cached OSM tiles carry any content and none of it above z15. The
**seamark overlay is the one that matters** — the traffic separation scheme,
the ferry route, buoyage, and the charted area boundary the `measures` surveys
sit on. It is on by default for that reason. OpenSeaMap's `contour` layer has
no data over this area at any zoom, which is theirs to have, not a fault here.

To prepare an area for a survey with no connection:

```sh
swath tiles                          # every indexed dataset, z10-18
swath tiles wpa20260906 --zoom 10-19 # one area, deeper
```

It fetches only what is missing, one request per 250 ms, with a real
User-Agent, and refuses more than 4000 tiles in a run without `--yes`. The
OpenStreetMap Foundation's tile usage policy names bulk downloading as
unacceptable use; this is sized to prepare one survey area for one boat, and
it should not be turned into a way to mirror a region.

## No C dependencies

The Python reaches for pyproj, which is a binding to C PROJ. Carrying that
across would have left a C dependency in the one place the rewrite was meant to
remove them, so `geo.rs` implements the EPSG guidance-note formulas directly:
transverse Mercator by the Krüger series, the oblique stereographic double
projection the Dutch grid uses, Lambert azimuthal equal area, and seven-
parameter Helmert datum shifts. `crs.rs` is the registry.

That is the claim `proj.json` exists to check, and it holds to sub-millimetre
everywhere the datum is shared.

Adding a coordinate system is a `Projection` variant and an arm in `crs::get`.
Present: WGS 84, Web Mercator, every UTM zone, ETRS89/UTM, Amersfoort RD New,
ETRS89-LAEA Europe, ED50/UTM 31N.

## What is not ported

Three Python modules never came across: a tow model solving layback and depth
from cable, speed and fittings; an inter-line navigation adjustment that
measured the shift between passes and refused to invent one; and a solver that
recovered the current, the speed through water and the cable scope, none of
which the recording carries. They were not hard to translate — they were still
moving. The roll question was open and the registration work unfinished, and
porting a moving target means carrying the churn in two languages.

They stopped where they were rather than being finished, and they are the real
loss in this repository's history. `git log` has them if they are ever wanted
again.

The Python JSF reader is gone too. It was never dead code — it was the thing
the Rust was measured against — and the frozen golden files are what remains
of it. The two cut from real recordings sit in the workspace rather than here;
the repository carries no survey data.

## Things worth knowing

**The sweep frequency wraps.** JSF stores the start and end of the chirp as a
`u16` in units of 10 Hz, which tops out at 655.35 kHz. A 1550 kHz channel is
recorded as 184–294 kHz, and the ping says nothing about the overflow. The
bandwidth is still exact; only the absolute frequency is displaced, by a whole
multiple of 655.36 kHz. `index::recover_band_centres` puts it back from the XTF
written beside it, matching modulo the wrap so a value that agrees is the same
number and one that does not is somebody else's recording.

**The mosaic paints strips, not lines.** Each ping is drawn as a polyline
across the swath, and the fish advances about 0.15 m between pings against
0.18 m cells at zoom 19 — so consecutive lines land a pixel apart and rounding
drops whichever cells fall between them. That was 18% of the painted area on
this survey, 279 000 of them single pixels, showing up as speckle through
otherwise good imagery. `paint` now fills between a ping and the one before it
at matching across-track distance, refusing to bridge where the fish jumped: a
turn, a data gap, or the seam between two files is not something to
interpolate over.

**The debounced save owns the only copy.** Layer and project edits are written
600 ms after the last change, so for that window the browser holds state the
server has not seen. Anything that re-reads the project has to flush first, and
`refresh` adopts the server's copy only when it is a *different* project --
otherwise creating a project and ticking a recording into it in one dialog read
back the empty project the create call had written, discarded the recording, and
then saved the discard.

**Contacts cannot be recreated.** They are a person's observation of the seabed,
and the loader used to drop any feature it could not parse and then write the
file back without it — `unwrap_or_default()` on a failed read turned into a
silent deletion on the next save. It now refuses to open rather than load a
subset, a set that failed to load is sealed against being written back, and
every save leaves one generation of `.bak` behind.

**The milliseconds matter.** The sonar pings at ~14 Hz and the position updates
at 1 Hz. Whole-second timestamps put every ping in a second at one fix, and the
2.5 m of seabed between consecutive fixes never gets drawn. The sub-second part
comes from the milliseconds-today field at body offset 200.

**Two fixes can share an epoch.** The two subsystems interleave, so subsystem 21
can stamp a fix a millisecond after subsystem 20 stamped the next one. Left in,
that puts a real 2.6 m of travel over a 1 ms gap and the speed comes out in the
thousands of metres per second. `nav::MIN_FIX_GAP_S` drops them.

**The recorded altitude is short.** On these recordings the JSF altitude field
sits about 1.1 m above — that is, short of — where the first return actually
is. Slant-range correction maps a true nadir return to `sqrt(true² − assumed²)`
of ground range, so 14.8 m assumed against 15.9 m actual throws the seabed
under the fish out to 5.9 m off-track, and the waterfall grows a phantom
six-metre channel down the middle. `signal::refine_bottom` uses the recorded
value as a prior and finds the real edge in a window around it.
