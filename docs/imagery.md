# From ping to picture

*[README](../README.md) · [Where things are](positions.md) · [From ping to picture](imagery.md) · [Projects and the report](projects.md) · [Internals](internals.md)*

What the sonar hands over, what has to be undone before it means anything,
and how the result is drawn on a chart beside everything else.

## The water column

There is no blanking step and no "waterline" control, because the axis does the
work.

The waterfall's default axis is **ground range**. For an output column at
ground distance `d` the renderer samples the trace at slant `sqrt(d² + alt²)`,
so column zero — nadir — samples at exactly `alt`, the first bottom return.
Everything before that, which is the water column, is never on the axis at all.
Switch the axis to **slant range** and it comes back: the trace is sampled
directly, so sample zero is the transducer and the water column is the dark
band either side of nadir that a sonar operator expects to see.

`alt` is not the altitude the file reports. On these recordings the JSF
altitude sits about 1.1 m short of where the first return actually is, and a
1.1 m error there throws the seabed under the fish out to 5.9 m of apparent
ground range — a phantom six-metre channel down the middle of the image. So the
recorded value is used as a prior and `signal::refine_bottom` finds the real
edge in a window around it. The mosaic uses the same function on the same
prior, because a contact marked in one view has to land where the other view
puts it.

The axis is carried through every inverse, not just the render. A column
offset is a *slant* range in slant mode and a ground range in ground mode, and
only the ground one is a place on the seabed — so `Waterfall` records which
axis it was drawn on, and `pixel_to_world`, `world_to_pixel` and the browser's
two copies of that arithmetic all branch on it. They did not, once, and
switching the control slid every feature across-track by `sqrt(g²+alt²) − g`:
nine metres on this data, worst at nadir and vanishing at the swath edge. The
mosaic is unconditionally ground range, so the chart was always right and the
slant waterfall was always the one that disagreed.

In slant mode a click inside the water column resolves to nothing rather than
to the fish's own position. There is no seabed there, and answering with a
coordinate anyway is a wrong answer dressed as a right one.

The mosaic has its own version of the question, and the answer used to be
hidden in a lookup table. Every priority table scores the first twelve degrees
off nadir at zero, which is the conventional choice — the flat-seabed
assumption is at its worst directly under the fish and the return there is
specular — and it blanks a strip two altitudes wide, about seven metres here,
along every single pass. `NADIR_FLOOR` gives that band a priority floor instead
of a zero, so it is painted at 1e-11 of an outer look’s weight: it fills where
nothing else reaches and loses to any other pass over the same ground. Coverage
one metre off the track line goes from 48% to 100%.

That floor is not optional, and briefly making it so was a mistake. Whether the
band is drawn is a question about imagery; where a sample lands is a question
about geometry, and in `build_stroke` the two never meet — the loop walks
ground range outward and reads the trace at `sqrt(g²+alt²)`, so a sample’s
across-track position is fixed before its priority is looked up. Suppressing
the band removed cells and moved none: on `070926_measures_b2` every cell
painted without it was also painted with it at the same raster address, and the
two rasters cross-correlate at r = 0.998 with the peak at zero shift and 0.86
one pixel away. So the setting could buy nothing but a hole down the middle of
every pass, in the one place where there is no second look to fall back on.
`the_priority_table_shades_the_mosaic_but_does_not_move_it` pins the general
form of that: change the table and the shading changes, the coverage does not.

Removing the setting had one sharp edge. A mosaic is named after a digest of
the settings it was painted with, and taking the field out put that digest back
to exactly what it had been before the field existed — so a raster painted
before the nadir floor was ever added answered to the current name, 5 282 872
cells against the 6 022 935 the current painter lays down, with nothing to say
so. `mosaic::PAINTER` is now in the digest and in the file header, and
`MosaicHeader::matches` checks both. Any future change to what `build` puts on
the ground is one constant away from invalidating every cached raster and tile
URL that depends on it.

Where the water column *is* worth a choice is the waterfall, and the Axis
control is already that choice. Slant range is what the transducer measured,
water column and all; ground range is the same trace laid flat on the seabed,
which has no room for it.

## What the sensor gives us, and what we do with it

Worth stating plainly, because the mosaic looks like a map of the seabed and is
not one.

The sonar records `data_format = 0` — envelope data, one 16-bit magnitude per
sample, scaled by `2^-weighting_factor`. Each sample is the **strength of the
echo at a fixed instant after transmit**: 10 240 ns apart on ss20, 8 320 ns on
ss21. Range is that time multiplied by the speed of sound. Nothing in the
recording is a depth. The mosaic is an acoustic reflectivity map — bright means
a strong return, dark means a weak one or a shadow — and two pixels of equal
grey can be at very different depths.

Depth enters in exactly three places, all of them thin: the altitude, which is
the time of the first bottom return and exists only to turn slant range into
ground range; the towfish depth, from a pressure sensor; and their sum, which
is a depth profile along the track line and nothing more.

### The sound speed

Times are truth. Every metre-valued field in the files is Discover's, computed
at whatever it was configured with, and here that is provably 1500 m/s — every
distinct range setting lands on a round number at 1500 and on nothing at 1524:

```
  samples  interval    two-way    @1500 m/s   @1524 m/s
      652   10240 ns    6.68 ms      5.01 m      5.09 m
     3896   10240 ns   39.90 ms     29.92 m     30.40 m
     6500   10240 ns   66.56 ms     49.92 m     50.72 m
```

The operator typed 5, 30, 50. So `C_RECORDED` un-converts Discover's own
numbers and `MosaicConfig::sound_speed_m_s` — measured, 1524 m/s here — turns
times into distances of our own. `PingRecord::resolution_m` takes the speed as
an argument rather than reading a constant, so no caller can convert a time to
a distance without saying which water it was in.

It scales the whole across-track axis and no angle: the range to a sample and
the altitude beneath the fish move together, so their ratio — the grazing angle
— does not. 1500 against a real 1524 drew the swath 1.6% narrow, half swath
47.29 m instead of 48.05 m, altitude 15.97 m instead of 16.23 m. That is 0.75 m
at the swath edge, always short.

### The beam, and why the nadir band is fill

The array is depressed 33° with a 50° vertical beam, so it lights the seabed
between **32° and 82° off vertical** — at 16 m altitude, from 10 m to 48 m
either side. Straight down is 32° outside the main lobe, which is exactly what
`NADIR_FLOOR` is filling in, and the report says so rather than presenting it as
coverage. The measured brightness profile finds that edge on its own: it peaks
at 10–15 m, not at nadir.

### Time-varied gain

Spreading and absorption take 39 dB out of a 47 m swath at 580 kHz and 44 dB out
of 30 m at 1550 kHz — the second is why the high channel appears to fade out, at
622 dB/km against 164. `MosaicConfig::tvg` was declared, defaulted and digested
for months without being read, so the mosaic had no across-track correction at
all and every pass painted a bright band down its own middle:

```
no gain    15 m: 128.7   25 m:  87.4   35 m:  50.5   45 m:  29.4   peak/edge = 4.37
gain 0.7   15 m: 140.0   25 m: 139.0   35 m: 119.2   45 m:  99.7   peak/edge = 1.40
```

The correction is physical rather than fitted — `30·log₁₀(R)` for spreading plus
`2αR` for absorption, with α from Francois–Garrison — because the mosaic's job
is combining passes, and a per-block flattener fitted to whatever is in front of
it makes passes *less* comparable, not more. It needs a temperature; the
measured sound speed is the thermometer, inverted through Mackenzie. Full
correction is the right answer and also amplifies whatever is at the swath edge
by thirty-odd decibels, noise included, which is what the strength setting and
the range limit are for. The waterfall keeps the empirical flattener: it is a
viewing tool, not a deliverable.

### Finding the seabed

The ground-range axis reads the trace at `sqrt(g² + alt²)`, so the altitude is
not a label on the picture — it is the picture's geometry. An altitude short by
`d` makes the inner `sqrt(A² − alt²)` of every swath sample *above* the seabed,
which is water, which is black. On `070926_measures_star` the detector was
picking 15.3 m where the return is at 21.5 m and the sonar's own tracker said
20.0, and the result was eight to fourteen metres of black at nadir, coming and
going from one ping to the next.

Three changes, all of them in `BOTTOM_*` in `waterfall.rs` and shared with the
mosaic so the two views cannot disagree about where the seabed is:

- The threshold rose from 0.25 of the window's contrast to **0.55**. A quarter
  is crossed by water-column returns several metres before the seabed.
- The prior is the sonar's own bottom track, **median-filtered over nine pings**
  before it is used. It comes from the ping index, so this costs nothing — not a
  trace is decoded. The seabed does not move five metres between pings a
  decimetre apart.
- The search window's *lower* bound tightened from 0.75 to **0.90**; the upper
  bound stayed at 1.45.

That last asymmetry was the whole lesson. Tightening both ends looked tidier and
was wrong: `wpa20260906`'s recorded altitude runs about 20% short of the return,
so a ceiling of 1.18 could not reach the seabed at all and *doubled* the
darkness under the fish. The bug was picking too early, so the low bound is the
one that matters.

Measured over five blocks of six hundred pings on three recordings, the inner
15 m of swath goes from 9.4 / 10.8 / 9.4% dark to **6.2 / 7.1 / 6.9%**. Against
the trace's own first strong return, the old settings picked more than a metre
short on 16 pings in 600; these pick short on one.

Steadiness is deliberately *not* the test. A detector that simply returned its
prior would be perfectly steady and perfectly wrong, and the old settings in
fact wandered less than these do while being metres short.

### Along-track banding, and what it is not

The mosaic bands along track, and the first three explanations were wrong. What
the measurements actually say, on `070926_measures_star`:

- The **raw traces are steady**: 2.3% mean step between consecutive pings, not
  one pair in 1500 jumping more than 30%. The weighting factor is normalised
  correctly — levels across w = 7…10 come out 6.21, 6.08, 5.97, 5.57.
- The mosaic carries about **30% speckle**, which is single-look Rayleigh
  statistics and not a fault.
- Under it the swath mean swings **96 to 146 grey** over tens of pings.
- About a quarter of that is **shared between port and starboard** — r = 0.245
  at a nine-ping scale, against a 0.088 control. That share is the part that
  reads as a band crossing the whole swath.

Sampling one across-track distance instead of the swath mean hides this
completely: at 25 m alone the two sides correlate at r = 0.011, because speckle
is most of the variance and it is uncorrelated between sides by construction.
The first version of this measurement said there was nothing there.

`MosaicConfig::agc` pulls each ping onto the running median of its forty-one
neighbours. The waterfall has done this since it was written and the mosaic
never did, which is the whole reason one looked even and the other striped. It
takes the shared component from 0.245 to 0.143 at half strength.

It is only a partial fix, and the number that says so is the swing: 1.51× to
1.47×. Most of the swath-mean variation is *not* common mode, so it is either
the seabed or something specific to one side, and a per-ping scale factor
cannot touch it — nor should it. For the same reason the effect is much weaker
on `070926_measures_b2`, a single short line at a steady altitude, where the
shared component only falls from 0.317 to 0.270: on that recording most of what
the sides share is the seabed they are both crossing.

**EGN will not fix this.** It is a function of range and grazing angle, so it
cannot see along track at all. It is still worth having, for a different
reason — making the range response comparable between lines — but it is not the
answer to a stripe.

### Blanking the nadir

`nadir_blank_m` leaves that many metres either side of the track unpainted, and
it is deliberately a different decision from `NADIR_FLOOR`. The floor is
unconditional because painting the band adds coverage where there is none and
moves nothing. The blank is the operator saying they would rather have the hole,
which is fair: the sonar is looking at the seabed with the edge of its beam
there. The main lobe starts about `0.62 × altitude` out — 10 m on these
recordings — and the settings panel offers that figure from the recording's own
flying height.

Coverage inside does not fall to zero, and should not: another pass reaching
the same ground from its own outer swath still paints it, which is a better look
at it than this pass's beam edge. Measured at a 10 m blank, coverage 5 m out
goes from 100% to 43% while 11 m and beyond is untouched.

### What the deliverable now says about itself

`nav::model_spread_m` builds the same recording under all three layback models
and reports how far apart they put the fish — 12.5 m at the median and 45.5 m at
the 95th percentile on `070926_measures_b2`. That is not an error bar; nothing
in the recording observes where the fish was. It is a floor under one, and it
goes in the report, beside every contact coordinate, and in the contact dialog
at mark time.

Overlap between passes is not a check on any of it. Sonar brightness depends on
the direction it was looked at from, so two passes on reciprocal headings do not
produce comparable pictures. One line correlated against itself matches at
r = 0.70 and falls off over a metre or two; the same test across headings
returns r = 0.04–0.07 with no peak anywhere in a ±36 m search. `wpa20260906` has
twelve lines and not one pair between 45° and 135° apart, so it cannot check
itself at all.

## Layers

The chart draws one ordered stack. Sonar mosaics and imported files are in the
same list and the list is the draw order, so a multibeam grid can go under the
imagery or over it — which is the whole point when the question is whether the
two agree.

| kind | what it reads | drawn as |
| --- | --- | --- |
| `recording` | nothing — a container for the two below | not drawn |
| `mosaic` | a recording's own backscatter | tiles, cut from `out/<name>/mosaic_*.swm`, coloured on the way out |
| `track` | the fish and boat positions | polylines |
| `raster` | GeoTIFF: `.tif`, `.tiff` | tiles, coloured on the way out |
| `vector` | GPX: tracks, routes, waypoints | lines and pins, drawn from the coordinates |

**The GeoTIFF reader is ours** (`tiff.rs`), for the same reason the projections
are: an imported layer's position has to be defensible, and a reader this code
owns can be held to an oracle. It reads what GDAL writes — strips or tiles,
uncompressed or LZW/Deflate/PackBits, horizontal and floating-point predictors,
8/16/32/64-bit integer and float samples, palettes, big-endian and BigTIFF —
and refuses what it cannot read by name rather than producing a plausible wrong
picture. JPEG-in-TIFF, CMYK and band-separate planar files are the notable
absences.

**A single-band grid keeps its numbers.** The resampled raster stores values,
quantised to 16 bits across the grid's own range — for a survey DTM spanning
ten metres that is a tenth of a millimetre — with `u16::MAX` reserved for "no
data". The colour ramp, the stretch and the relief shading are applied when a
tile is cut, so changing any of them costs nothing and the depth under the
cursor is still a depth rather than a colour read backwards off a ramp.

Eight ramps, `depth` by default. Relief shading is on by default too, with a
vertical exaggeration, because seabed relief is centimetres over metres of
ground and lit truthfully it would be invisible.

**The sonar mosaic takes a colour scheme too**, from the same eight, plus a
black and white point. The mosaic's value plane is 8-bit backscatter that was
stretched when it was painted, so a scheme is a 256-entry lookup applied as the
tile is cut: it costs nothing, needs no repaint, and rides in the tile URL so
the browser caches each scheme separately. Grey is the default because it is the
honest one — a ramp makes small differences in return strength easier to see and
equally easy to over-read.

```sh
swath layer data/merged_dtm_elevation.tif --preview /tmp/dtm.png
#   9902x9428 · 1 band · float32 · none
#   EPSG:4326  WGS 84
#   -> merged_dtm_elevation.swl (284.5 MB) 9531x14927 at z18, 56.8% filled, 9.2s
#   values -29.305 .. -22.007  (2-98%: -27.961 .. -23.661)
```

The base zoom is chosen so one output cell is no coarser than the finest axis
of the source. Web Mercator stretches latitude by 1/cos(φ), so a grid that is
square on the ground is not square on the chart — each source pixel is painted
over the rectangle it actually covers, which is what stops the result coming
out striped.
