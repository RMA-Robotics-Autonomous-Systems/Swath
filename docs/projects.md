# Projects and the report

*[README](../README.md) · [Where things are](positions.md) · [From ping to picture](imagery.md) · [Projects and the report](projects.md) · [Internals](internals.md)*

The working surface — what a project holds, how the layer tree behaves — and
the deliverable it turns into.

## The project tree

One panel, in draw order, first on top:

```
PROJECT                      ↻  Recording…  File…
  ▾ ◉ 070926_measures_b2                        ⚙   ← navigation lives here
      ◉ ss20 mosaic                    sonar    ⚙   ← colour scheme, contrast
      ◉ ss20 track                     track    ⚙   ← colour, boat or fish
      ○ ss21 mosaic                    sonar    ⚙
  ◉ Multibeam DTM                      grid     ⚙   ← ramp, stretch, shading
  ◉ survey lines.gpx                   gpx      ⚙
```

A recording is a container, not something drawn; its mosaics and tracks are its
children and move with it. Children reorder inside their own parent and never
change parent by dragging — a mosaic belongs to its recording, and letting a
drag say otherwise would only ever be a mistake.

**The checkbox means display, and only display.** Unticking a recording takes
its whole subtree off the chart; the children keep their own settings, dimmed,
waiting for the parent to come back on. Nothing is loaded, unloaded or
discarded by a checkbox. Ticking a mosaic that has never been painted still
starts the build, but behind the tick rather than in front of it: the box goes
on and stays on, the row says `building…`, and a failure is reported instead of
silently unticking the box the operator just ticked.

**Navigation is per recording**, on the recording's own row, because the layback
is a property of how that day was rigged. It is also what places the imagery, so
applying it re-solves the fixes, re-fetches the track, moves the waterfall's row
positions and repaints that recording's mosaics — all of them, in one action.

Everything else is a settings dialog on the row that owns it. A mosaic has a
colour scheme and a contrast stretch; an imported grid has a ramp, a value range
and relief shading; a track has a colour and a choice of boat or fish. None of
that fits in a sidebar row, and putting it there is what produced three panels
competing for the same 300 pixels.

The list is stored flat, as a pre-order flattening — a node's children follow it
contiguously — so it round-trips through the project file without a schema for
trees, and so the stored order *is* the draw order. Projects written before the
tree existed are migrated on open: an orphaned mosaic is adopted by its
recording rather than dropped, because the operator arranged that order.

Tiles paint in stack order; vectors paint over all of them. A track drawn under
the imagery it describes is of no use to anyone, so the stack orders like
against like rather than pretending one list can decide both.

## Starting a project

`+` opens one dialog that asks for everything a project needs: the client,
vessel, operator and job number that go on the cover, the grids the report
should tabulate positions in, and which recordings the job covers — each with
its own layback, model and swath bearing, set before it is first solved. It used
to be a `prompt()` for a name, which left every decision that matters to be
found later in three different panels.

The same dialog, minus the name, is the project settings.

## The report

HTML with a print stylesheet, and "export PDF" is the browser's own
print-to-file. reportlab has no Rust equivalent worth the name, but the viewer
is already a webview and a webview can print — which removes the last dependency
the port could not have satisfied, and means the report can be read in the app
before it is exported.

**The report keeps its own list of what to draw.** The layer tree is a working
view — a bathymetry grid switched on to check a depth stays on — and a chart in
a deliverable is not that. Pressing **Report** opens a dialog holding the
outline: every chart, and under each one the layers it could show. Nothing is
drawn until it is agreed.

Parts, in this order:

1. **Positioning** — the caveat, on the cover. Every report of this survey has
   to carry it: the imagery is placed by a layback with no reference grid behind
   it, and a reader who takes a contact position as metre-accurate will send a
   ROV to the wrong patch of seabed.
2. **Summary** — the size of the job in one table: recordings, elapsed time,
   lines, line kilometres, pings, extent, contacts, imported layers, frequency
   bands and the coordinate systems positions are tabulated in.
3. **Coverage, one chart per frequency band**, with every recording on it. Two
   frequencies are two different answers about the same seabed; painting them
   into one image only shows whichever won the z-order.
4. **One section per recording** — its numbers and its full navigation solution
   beside the picture that navigation produced, then a chart per band with its
   own legend, then which contacts were marked on it.
5. **Contacts** — the register in every requested coordinate system, then one
   sheet per contact with its snapshot.

The bands are named from the frequency the sonar actually transmitted, rather
than from `ss20` and `ss21`. Getting that number is not simply a matter of
reading it: **the JSF field wraps.** The sweep is a `u16` in units of 10 Hz, so
it cannot express anything above 655.35 kHz, and this survey's 1550 kHz channel
is written as 184–294 kHz with nothing in the ping to say by how much it
overflowed. The bandwidth survives — both ends are displaced equally, so their
difference is untouched — but the absolute value does not.

The XTF the same acquisition wrote alongside carries the centre frequency as a
float in its channel descriptors, and `index::recover_band_centres` matches the
two *modulo the wrap*: a candidate that agrees is the same number with its high
bits restored, and one that does not belongs to some other recording. That also
solves the mapping, since the high and low channels are in separate files with
no shared numbering. The result is 580 kHz and 1550 kHz, which is what the XTF
names them. Where nothing corroborates it, the label falls back to the channel
number rather than printing a figure that is wrong by a megahertz.

Defaults: mosaic and boat track on a coverage chart; mosaic, boat and fish
track on a recording's own; OpenStreetMap with seamarks; imported grids and GPX
off until they are asked for. That last one is the reason the dialog exists — a
DTM a hundred times the size of the survey decides the frame and buries the
imagery, and the previous report had no way to say no.

The chart views are drawn by the *viewer*, not by a second renderer in the
server: same projection, same layer stack, same colours, so the report shows
what the operator was looking at. They are posted to the project and kept there,
which means `swath report` run afterwards still has them. A project whose spec
is empty gets a report that says so, rather than one silently missing its maps.

Each chart is fitted continuously into a frame shaped like its own data. Walking
integer zooms downward until the bounds fit overshoots by up to a factor of two
— every step halves the span — and a fixed landscape frame letterboxes anything
that is not 3:2. Together those two put this survey on a quarter of its own
picture:

```
070926_measures        old: 43% wide x 76% tall = 32% of the frame -> 96%
070926_measures_b2     old: 34% wide x 86% tall = 29% of the frame -> 96%
070926_measures_star   old: 36% wide x 70% tall = 25% of the frame -> 96%
```

Each contact sheet carries **two** crops, chart and sonar. They answer different
questions: the chart says where the thing is and what surrounds it, the
waterfall says what the return and its shadow actually look like, which is what
a classification is argued from. Neither carries a reticle — the crop is built
around the contact's own world pixel, so the centre of the image *is* the
contact, and a marker there covers the thing the sheet exists to show.
