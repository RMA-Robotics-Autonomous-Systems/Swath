# Where things are

*[README](../README.md) · [Where things are](positions.md) · [From ping to picture](imagery.md) · [Projects and the report](projects.md) · [Internals](internals.md)*

The transform the whole viewer is built on, how far it can be trusted, and
the test that stops the two views drifting apart.

## The one idea

Everything in the viewer is one transform and its inverse.

```
ping_space  ---- nav ---->  world
(ping, sample)              (lat, lon)

fish   = nav.position(ping)
slant  = sample * bin_size
ground = sqrt(slant^2 - altitude^2)
world  = fish + ground * bearing(nav.bearing(ping) +/- 90)
```

The waterfall draws it one way and the mosaic draws it the other. A contact
marked in either view has to land in the same place, so both call the same
code — `nav::Nav::fix` for where the fish was, `signal::refine_bottom` for the
altitude, and the same across-track geometry on top.

## How good are the positions?

**5 to 10 metres on a straight run line. Worse in turns, and by an unbounded
amount.** The viewer says the first part on its navigation panel; the report
says both.

The fish is placed a constant distance astern of the tow point, along the
smoothed course over ground, with a 4 m lever arm from the GNSS antenna to the
tow point. That is deliberately the plain model. Six richer ones were tried
against a multibeam DTM of the same ground — tractrix, pure dead reckoning, a
complementary filter, a compass ODE, catenary-from-scope — and a flat offset
captured essentially all of the available improvement, with three of the others
making at least one dataset worse.

The cable length bounds the offset's *magnitude* to about a metre — and note
the default 44 m is that bound, not a best estimate. With 45 m of cable out and
the fish 5–19 m down, a perfectly taut straight cable gives 43.9–44.7 m. Any
sag makes the real layback smaller and nothing in the recording says how much.

What stays genuinely unobserved is the offset's *bearing* while the vessel is
turning. Three placements are available for a fish 48 m behind a vessel on an
arc of radius R:

| `model` | radius | |
| --- | --- | --- |
| `astern` | R, ahead of the wake | the default |
| `wake` | R | follows the vessel exactly |
| `tractrix` | √(R² − 48²) | taut cable; what a towed body does |

All three are selectable, in the viewer's Navigation panel or per dataset in
`project.json`. `tractrix` integrates the pursuit curve along the tow-point
path — the fish stays `layback_m` from the tow point and always moves straight
at it — and it is checked in `tests/tractrix.rs` against the closed form for a
steady turn, to 2 cm at R = 150 m.

Measured across these recordings, `astern` differs from `tractrix` by 12–24 m
at the median while turning, and by up to 83 m in the tightest turns. Between
23% and 45% of pings are turning faster than 1°/s.

**The gap does not vanish on the straights.** It decays with distance sailed
since the last turn, because a towed body needs about three cable lengths to
settle:

| since the last turn | median `astern` → `tractrix` |
| --- | --- |
| under 25 m | 5.36 m |
| 50–100 m | 1.80 m |
| 200–400 m | 1.26 m |
| over 400 m | 0.72 m |

On `wpa20260906`, 92% of pings are within 150 m of a turn. The steady state a
constant offset assumes is close to never reached.

None of the three is known to be right here. `astern` is the geometric
convenience and `tractrix` the taut-cable limit; real cable drag makes a fish
lag more than a pure pursuit curve, so the truth probably sits between `wake`
and `tractrix`. The spread between them is the honest uncertainty — and it is
why the default is still `astern`: switching it would move every existing
contact without any evidence that it moves them closer to where they are.

Closing this needs a reference grid to register against, or a sensor on the
fish that measures its bearing from the tow point.

The fish's compass is available as an alternative swath bearing. It is a real
absolute compass, not an integrated rate — its bias drifts about +0.17°/hour
and every accelerometer and gyro-rate slot in JSF message 2020 reads zero. It
is noisy enough at ping rate that it is smoothed over a couple of seconds
before it aims anything.

Roll beyond a threshold is **flagged, not filtered**. Whether roll corrupts the
geometry is not established: the port and starboard bottom ranges show no
correlation with it (r = −0.023), and a `cos(roll)` correction made the fit
worse. The waterfall stripes the affected rows and leaves the judgement to the
operator.

## The invariant

A contact marked on the waterfall and the same contact marked on the chart must
be the same place on the seabed. `tests/views.rs` holds that, and
`ui/test/run.js` holds the browser's half of it — a pixel's distance from the
fish has to equal the across-track distance the same pixel reports, computed by
two different paths:

```
waterfall pixel -> world: worst 0.0192 mm
waterfall round trip: worst column error 0.000 px
slant round trip: worst column error 0.000 px
ground vs slant, 60 probes: worst 0.0000 m apart
half width: 46.31 m ground, 49.92 m slant, altitude 18.53 m
waterfall positions with mosaic under them: 210/210 (100.0%)
```

It is easy to break silently. Three of the bugs it would have caught were found
while writing it, and the fourth was found by the tests it was missing:

- **A pixel resolved 12 cm short of its own range.** Positions were placed by
  walking a bearing over the ellipsoidal metres-per-degree series, then measured
  back with a haversine on a sphere of mean radius. Those disagree by 0.32 % at
  52 N — 12 cm across a swath, three metres across a kilometre of measured line.
  `geo::distance_m` is now the exact inverse of `geo::offset_m`.
- **A round trip landed 19 px out.** `world_to_pixel` searched for the row whose
  fish position was *nearest*, which is the wrong question: a target 40 m off
  track is nearly equidistant from every row for tens of metres either side, and
  on a survey that doubles back it can match a row from a different pass. It now
  picks the row that has the point abeam.
- **Slant range moved the seabed by nine metres.** Every test above built its
  request with `..Default::default()`, and `Ground` is the default, so slant
  range had never been rendered by anything except the application. The axis was
  consumed in one expression in the renderer and thrown away everywhere else,
  and each of the five inverses assumed ground range — which is *self*
  consistent, so the round trip passed while the picture and the transform
  disagreed. `the_axis_does_not_move_the_seabed` takes one distance down one
  trace, draws it into both axes, and asks whether it is still the same patch of
  seabed. It was 8.94 m from itself.

The last one is the shape of a whole class of these: a test that inverts what
the code just did will pass however wrong the code is. The invariant has to
cross something — two implementations, two axes, two views.
