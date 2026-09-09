# Planning a search

Given a handful of positions to look at and a sonar to look with, there is
exactly one decision worth making: which way to steer. Everything else follows
from it by arithmetic, and the arithmetic is in
[`plan.rs`](../crates/swath-core/src/plan.rs) with no I/O in it, so the viewer,
the command line and the tests all get the same answers.

## The targets are contacts

There is no separate list of things to search for. A position worth searching
for and a position worth reporting are the same kind of thing, and what you go
looking for today is what you mark tomorrow — so **any contact can be a target**,
and the Plan pane lists them all with a tick beside each. What is ticked is what
the lines are laid out for.

Two ways in, and neither is more correct than the other:

- **A datum** — a position given to you, typed in with *Datum…* beside Contacts.
- **A contact you already have** — something found last week that is worth
  another look. Tick it in the Plan pane; nothing about it has to change.

A project that has never been planned starts by searching for every datum, which
is what an operator who has just typed three positions in would expect. After
that the list is explicit, so unticking the last target leaves the plan empty
rather than quietly reverting to the datums.

Each target carries the `radius_m` the plan has to cover — the uncertainty
circle, not the dot in the middle. It is editable in the Plan pane beside the
tick, because that is where the question comes up; a contact with no radius is
planned for as 50 m rather than as a mathematical point.

`source: datum` is still worth having, and for the same reason the other two
sources are. A map mark is a
position and does not move; a waterfall mark is a ping and a sample resolved
through the navigation, and it *does* move when the navigation is re-solved. A
datum was given to us rather than found, so nothing about re-solving anything
should touch it.

Datums are typed in, not clicked — *Datum…* beside Contacts, in degrees and
decimal minutes as a plotter shows them (`N5125.2300`, `E00309.3400`) or in
decimal degrees. The dialog says what it understood before it will save it,
because a transposed digit is a day at sea in the wrong place.

## Spacing is three regimes, not a percentage

With a usable half swath of `range − nadir` either side, three thresholds fall
out of the geometry, and each one means something different on the seabed:

```
  spacing <= 2*(range - nadir)   every patch of seabed seen once,
                                 and a hole under every line
  spacing <= range               neighbours cover each other's nadir gap,
                                 so nothing is unseen
  spacing <= range - nadir       every point seen twice, from opposite
                                 sides -- the search standard
```

The last reduces to the textbook `spacing = range` when the nadir gap is zero,
which is the check that this is the usual derivation and not a new one. The
settings panel prices all three in metres and names them for what they buy,
because "65 % overlap" tells an operator nothing about whether the thing they
are looking for will be in the picture.

Coverage is then computed exactly rather than estimated. Every line spans the
same along-track extent, so what happens across track happens everywhere in the
box, and a one-dimensional sweep across it is the whole answer — including where
the holes are, which the chart draws in red rather than leaving as absence.

**A line array that stops at the box edge is one look short there.** The
outermost line's own nadir gap sits on the boundary with nothing outside it to
fill in, so where the regime promises two looks the plan runs one line beyond
each side to keep that promise at the edge. It costs two lines and it is the
difference between "double coverage" being true and being nearly true.

## The lines are stretched for the fish, not the boat

The waypoints steer the **GPS antenna**. The fish is `gps_to_towpoint_m +
layback_m` astern of it — 54 m on the default rig — so a line that runs from one
edge of the box to the other puts the boat over the box and the fish short of it
at both ends.

Each line is therefore stretched, and not symmetrically:

```
  run-out  = the full antenna-to-fish offset, exactly
  run-in   = the settling distance, less that offset
  line     = the box, plus one run-in. Nothing else.
```

The run-in is not padding. A towed body needs about three cable lengths to
settle behind a turn, and until it has, the constant-offset placement in
[`nav`](../crates/swath-core/src/nav.rs) is wrong by an amount that decays with
distance sailed — 5.4 m within 25 m of a turn, 1.8 m by 100 m, 0.7 m past 400 m
on these recordings. The run-in buys that settling, which is why recording
starts partway along the line rather than at its start, and why the GPX carries
three points per line rather than two:

| | |
| --- | --- |
| `L01S` | the boat settles onto the heading here |
| `L01A` | the fish has reached the box; recording starts |
| `L01E` | the fish leaves the box |

## The turns are solved, not assumed

Lines run 1, 2, 3 with a 180° turn between each and the heading alternating every
time, which is what a boat actually does. A clean semicircle between neighbouring
lines needs `2 × turn radius` across track; with less than that the turn has to
loop out, and the plan says how far past the ends of the lines it reaches,
because that is water the helm needs to have.

The alternative is to run every k-th line with room to spare and fill the gaps in
on the way back, which costs one long transit between passes and no teardrops at
all. Both are offered; neither is guessed at. The path itself is the shortest of
six candidate curve-straight-curve and curve-curve-curve words, so the run-in's
along-track offset — the two poses are not simply side by side — is solved rather
than ignored.

No data is claimed inside a turn. Where the fish is through one is genuinely
unobserved in this rig, which is the same fact the run-in exists because of.

## Only 0 to 90 degrees

A box run at 100° is the same set of lines as one run at 10°, turned; past 180°
it repeats outright. So the quadrant is not a shortcut, it is the complete set of
plans the box has, and the app generates it at a chosen step — ten plans at 10°
by default.

Which of them to run is a judgement made on the day, and the app does not try to
make it. The plan with the fewest lines is whichever way the box is narrowest;
the plan you actually want is usually the one that runs with or into the sea you
have got, because beam seas roll the fish and smear the swath. That is a decision
for someone looking out of a window, so the quadrant is generated for every
orientation and the choice is left where it belongs. The list carries the lines,
the distance, the hours and whether anything is left unseen — which is what the
choice is made against.

## What comes out

`POST /api/plan/export`, or `swath plan <project> --export`, writes one GPX per
azimuth into `projects/<name>/exports/`, alongside a `search-plan.txt` index.

### The file is a trace

GPX has no arc. Its only geometry is the polyline — `rtept` and `trkpt` — so a
curve is points and nothing else. The question is which container they go in,
and the two are not interchangeable:

- A **route point** is a steering instruction. The plotter makes it a waypoint,
  sequences it, gives cross-track error against the leg into it and sounds an
  arrival alarm when you reach it. A route thick enough to draw a smooth turn is
  unusable to steer by — an alarm every few seconds — and on many plotters it
  overruns the route point limit as well.
- A **track** is a drawn line. No waypoints, no sequence, no alarms, and as many
  points as the curve needs.

Plenty of plotters will take a route *or* a track from one file but not both, so
the default is the track: for a helm steering by hand, the shape to follow is
worth more than legs that will not be steered along. One `<trk>` named
`<plan> path`, holding the whole run — every line and every curve between them —
plus the targets as waypoints, and nothing else.

The turns in it are thinned to a metre of the true arc, which is far finer than
anything can be held to and about a third of the points the chart draws them
with. A twenty-three line plan comes out as 354 track points.

*Write* offers the other two shapes for gear that wants them:

| | |
| --- | --- |
| **Trace** | one track, the whole path. The default. |
| **Route** | two points a line, `S` and `E`, for cross-track error. No curves — a route cannot carry them usefully. |
| **Both** | for a plotter that will take them together. |

Two more switches, both off by default. *Line marks* puts every line's start,
recording-on and end on the plotter as standalone waypoints — three per line, so
sixty-nine pins on a twenty-three line plan, which is worth it only when the
screen is the sole record of where recording starts. And `turn_points` on the
export API adds one waypoint at the apex of each turn inside the route, which is
for an autopilot that would otherwise cut the corner; steering by hand it buys
nothing but an alarm.

Each file carries in its metadata the settings that produced it — range,
altitude, nadir gap, spacing and regime, layback, run-in, speed, turn radius,
line count, distance and time — for the same reason the mosaic names the
settings it was painted with. A plan found on a laptop six months later says
what it came from.

Choosing one plan also imports it back as an ordinary vector layer, which is
what puts it on the chart and in the report. There is no second rendering path:
a finished plan is a GPX file, and this application already knows how to read,
draw and report one of those.

## What it does not know

- **No bathymetry.** Altitude is assumed held at the figure given. Over a slope
  both the nadir gap and the outer range move, and the spacing that just closed
  the gaps opens them again.
- **No current.** The fish is placed straight astern. A crab angle puts it off
  the line by `layback × sin(crab)` — 5 m at 44 m and 7°.
- **The turn is a drawing, not a prediction.** Constant radius, flat water.
- **No sea state.** The plans are generated for every orientation precisely
  because which one suits the weather is not knowable when the planning is done.
- **No tide, no traffic, no time window.**
