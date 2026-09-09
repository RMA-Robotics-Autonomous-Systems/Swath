# swath

A sidescan sonar survey viewer, in Rust.

Load a day's recordings, see where the boat went and where the towfish actually
was, read the waterfall and the georeferenced mosaic side by side, and mark a
contact from either one so it lands in the same place on both. Then hand out a
report with the positions in whatever coordinate systems the client asked for.

It reads EdgeTech JSF and XTF, imports GeoTIFF grids and GPX, and runs as a
local web app or a desktop window. There is no build step for the frontend and
no C library underneath: `cargo build` is the whole install.

---

## Build

```sh
cargo build --release        # the library and the `swath` command
```

That is everything for the browser version. The desktop shell additionally
links webkit2gtk; where those `-dev` packages cannot be installed system-wide,
`scripts/build-env.sh` points the build at a user-owned sysroot:

```sh
source scripts/build-env.sh
cargo build --release -p swath-app
```

## The workspace

**swath works on a folder, and that folder is not the source tree.** A
workspace holds three directories, and the app fills in the last two itself:

```
data/<recording>/*.jsf     the recordings, untouched — you put these here
out/<recording>/           everything derived from them, rebuildable
projects/<name>/           the jobs: contacts, layer stacks, report settings
```

Point the app at one with `--root`, or just start it from inside:

```sh
cd /path/to/workspace
/path/to/swath/target/release/swath serve --port 8731    # http://127.0.0.1:8731/
/path/to/swath/target/release/swath-app                  # the same, in a window
```

Both start the same server and serve the same frontend; the shell is a window
around it. The port is optional — without one the kernel picks it and the
server says which, along with the two paths it decided on:

```
swath 0.1.0 — http://127.0.0.1:8731
  workspace: /path/to/workspace
  frontend:  /path/to/swath/ui
```

**Read those two lines when something is missing.** A viewer that comes up
looking perfectly healthy and completely empty is almost always a workspace
pointed one directory off — there is nothing in it, and nothing about that
looks broken.

## First run

1. **Add a recording.** *Recording…* in the left panel takes a folder from
   anywhere on the machine — the survey drive, a share, `data/` — as a copy, a
   move, or a link that leaves the files where they are.
2. **Wait.** Indexing and mosaicking start on their own, in the background. The
   layer row says `building…` and reports a failure rather than quietly
   switching itself off.
3. **Look.** Tick a mosaic to put it on the chart. Tick two and the waterfall
   shows both bands over the same seabed, sharing rows and scroll — 580 kHz
   beside 1550 kHz, same ground.
4. **Mark contacts.** `M`, then click, on the chart or the waterfall. Either
   way it lands in the same place on both, carrying the water depth and fish
   altitude at that spot and a sonar snapshot taken as it was marked. Extent is
   recorded when you draw one, and left empty when you do not.
5. **Report.** *Report* opens the outline, asks which layers each chart should
   carry, and renders HTML with a print stylesheet. "Export PDF" is the
   browser's own print-to-file.

Nothing above needs the command line. It is all there when you want it:

```sh
swath index <recording>                   # build the ping index
swath mosaic <recording> [--subsystem 20] # build the georeferenced mosaic
swath layer <file.tif|file.gpx>           # import a grid or a track
swath info <recording>                    # what is in a recording
swath report <project> --out report.html  # the survey report
swath tiles [<recording>...]              # warm the chart tile cache, for offline
swath fixtures <dir>                      # dump what this code computes, for diffing
```

## What it is careful about

**Positions are honest about themselves.** The fish is placed astern of the tow
point on the smoothed course over ground, which is good for **5–10 m on a
straight run and worse in turns by an unbounded amount** — and the viewer says
so on its navigation panel, the report says so on its cover. Three tow models
are selectable and the spread between them *is* the uncertainty. Nothing here
invents a precision it cannot support. See [Where things are](docs/positions.md).

**The two views cannot drift apart.** A contact marked on the waterfall and the
same contact marked on the chart must land in the same place, so both go
through one transform and its inverse rather than two implementations that
agree today. There is a test that holds them to it.

**The imagery says what was done to it.** Water column removal, time-varied
gain, along-track equalisation, the nadir band — each is a decision, each is
recorded in the settings digest that names the file it produced, and changing
one gives you a different file rather than quietly repainting the old one. See
[From ping to picture](docs/imagery.md).

**A wrong picture is worse than no picture.** The GeoTIFF reader is ours rather
than GDAL's, so an imported layer's position can be held to an oracle, and it
refuses encodings it cannot handle *by name* instead of returning something
plausible. The projections are ours for the same reason and are checked against
PROJ to sub-millimetre. It reads GeoTIFF; it does not write one.

## Documentation

| | |
| --- | --- |
| [Where things are](docs/positions.md) | the transform, how far to trust it, and the invariant that holds the views together |
| [From ping to picture](docs/imagery.md) | water column, gain, the seabed, the nadir band, and the layer stack |
| [Projects and the report](docs/projects.md) | the layer tree, what a project owns, and the deliverable |
| [Internals](docs/internals.md) | layout on disk, linked views, what the tests prove, and the awkward details |
| [Mosaicking](docs/mosaicking.md) | how the field does it, and what this does |
| [Along-track banding](docs/banding.md) | what the banding is, after three wrong explanations |
| [Reading the overlap](docs/reading-the-overlap.md) | separating the passes where coverage crosses itself |

## Tests

```sh
cargo test                  # 54 tests
bun ui/test/run.js          # 266 headless browser checks
```

The browser half is not a formality: it owns the live path for a waterfall
click, so the across-track arithmetic exists twice and both copies are held to
the same numbers.

Tests that need a real recording skip when they cannot find one. Point
`SWATH_WORKSPACE` at a workspace holding `data/` and `out/` to run those.

The repository carries no survey data. The fixtures in `fixtures/` are computed
reference values — a projection grid and synthetic rasters — that belong to no
job. The golden files cut from real recordings live with the recordings, in
`<workspace>/out/fixtures/`, and their tests skip when they are not there.

## Status

Used on real surveys, by the person who wrote it. The parts that are unfinished
are named as unfinished rather than left for you to discover — the tow model's
bearing through a turn is genuinely unobserved, and inter-line registration is
not in this tree at all.

Chart tiles come from OpenStreetMap and OpenSeaMap and are cached to disk. The
fetcher is paced to two connections and will not bulk-download; if you are
going to lean on it, read their tile usage policies first.

## Licence

MIT — see [LICENSE](LICENSE). Map tiles are not covered by it: they belong to
OpenStreetMap and OpenSeaMap and carry their own terms.
