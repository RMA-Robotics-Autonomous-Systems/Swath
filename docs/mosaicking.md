# Sidescan mosaicking — what the field does, and what this repo does

Written because the first mosaic here looked unconvincing. It was, and the
reason was structural rather than a bug: it averaged every overlapping pass
together. This is a note on how the field actually does it, and what changed.

## The measurement that started it

Correlating a waterfall chunk against the mosaic covering the same seabed,
after box-blurring both to suppress speckle (which does not repeat between
looks and so cannot be correlated at pixel level):

| | mean correlation at zero shift |
|---|---|
| single-pass mosaic, built only from that chunk's own pings | **0.59**, peak at (0, 0) |
| the full averaged mosaic | 0.38, peaks wandering up to 10 m |

The single-pass number is the control. It says the geometry — slant-range
correction, layback, course, projection — is right, because a mosaic made from
one pass reproduces that pass. The full mosaic scoring far worse means the
damage happens when passes are combined.

## What the field does

### Do not average overlapping passes

[MB-System's `mbmosaic`](https://www3.mbari.org/data/mbsystem/html/mbmosaic.html),
the reference open-source implementation, assigns every sample a **priority**
and by default gives each cell "the value of the highest priority sample found
in that bin". Averaging is opt-in (`-F`), and even then only across samples
within a chosen band below the best priority, as a Gaussian weighted mean.

The reason is physical. Two passes see the same patch from different ranges and
opposite directions. Shadows fall the other way, the angular response differs,
and the speckle is uncorrelated. Averaging those is not noise reduction — it is
destroying the shadows that make a sidescan image readable.

Priorities in `mbmosaic` multiply three terms:

- **Grazing angle** (`-Y`), `arctan(across / altitude)`. Eight built-in tables;
  1–4 favour the outer swath, 5–8 favour nadir. The manual is blunt about why:
  "the nadir region of the sidescan swath is generally of little use because it
  is dominated by specular reflection."
- **Look azimuth** (`-U`), `p = cos(f · (Ap − Aa))`, zero beyond ±90°. Keeps the
  illumination coming from a consistent direction so shadows agree.
- **Heading**, same form, on the platform heading.

### Correct radiometry before combining, against angle

The distortion to remove is the combination of residual time-varying gain,
beam pattern, angular response and altitude variation
([Zhao et al. 2017](https://doi.org/10.3390/rs9060575)). The standard fix is an
**angle-varying gain** curve: the empirical backscatter as a function of
grazing angle, divided out. Backscatter follows roughly Lambertian behaviour,
with deviations small between about 40° and 80° of incidence and larger below
that ([Lambert's cosine law and sidescan modelling](https://www.diva-portal.org/smash/get/diva2:1473933/FULLTEXT01.pdf)).

Doing this against *grazing angle* rather than against range matters when the
fish altitude changes — as it does here, 10 to 18 m — because the same ground
range is a different angle at a different altitude.

### Feather the seams that remain

Even with best-look selection there are boundaries where the winning pass
changes. Practice is an automatic seam-removal step plus interactive
feathering, or a weight-based blend across the join, to hide the tone step.

### Register the lines to each other

The hardest remaining error is navigation: adjacent lines disagree by metres,
so the same feature appears twice in slightly different places. The literature
attacks this with feature matching between overlapping strips — A-KAZE or SURF
descriptors, RANSAC to reject outliers, then an optimal seam line, sometimes
constrained by the track positions so the correction cannot drift
([Zhang et al.](https://www.researchgate.net/publication/325798123_Side-Scan_Sonar_Image_Mosaic_Using_Couple_Feature_Points_with_Constraint_of_Track_Line_Positions);
[a recent geometrically consistent framework](https://arxiv.org/pdf/2509.11255)).

Where overlap is good this pays off twice: mosaics from overlapping swaths can
be gridded at a **smaller** pixel size than a single swath supports, and the
edges of backscatter features sharpen as overlap increases
([Lucieer et al.](https://www.sciencedirect.com/science/article/pii/S0025322720302310)).

## What is implemented here

In [tools/mosaic.py](../tools/mosaic.py):

- **Angle-varying gain.** The median backscatter is measured against grazing
  angle over 90 bins, sampled across the whole recording, and divided out.
  Replaces the earlier profile against normalised range, which ignored the
  altitude changes.
- **Grazing-angle priority tables** (`--table outer|nadir|flat`), shaped after
  `mbmosaic`: zero under about 12°, rising through 35°, flat across the useful
  35–75° band, tapering at extreme range.
- **Priority-weighted selection** rather than averaging. Weight is
  `priority ** exponent` with `--exponent` defaulting to 8, so the best-placed
  look dominates a cell while the transition stays smooth — an approximation of
  best-look selection that feathers its own seams.
- **Look-azimuth priority** (`--look-bearing`, `--look-factor`), the `mbmosaic`
  cosine rule, off by default because this survey runs reciprocal courses and
  enabling it discards half the data.

Result on the same correlation test: mean zero-shift correlation **0.38 → 0.52**,
and the worst chunk went from −0.17 to +0.25. Coverage rose 39.8% → 43.9%,
because nadir is now down-weighted rather than hard-blanked, so it still gets
used where nothing better exists.

## Altitude: use the sonar's, not your own pick

The slant-range correction needs the height of the fish above the seabed. This
repo originally picked the first strong return from each trace. That works on
the 580 kHz channel and fails on 1550 kHz, whose backscatter is roughly
sixteen times weaker: measured over one stretch the picker returned a median of
14.7 m but a **10th percentile of 1.0 m**, latching onto water-column noise. A
wrong altitude corrupts the geometry and leaves a water band in the image, and
that is what "the high frequency data looks corrupted" was.

The JSF header already carries the sonar's own bottom track — altitude in
millimetres at bytes 144-147, gated by bit 6 of the validity flag at bytes
30-31. It is far better: valid on 100% of pings here, smooth, and the two
channels agree on it to a **median of 0.01 m**.

It is not infallible, so it is cross-checked: `altitude + depth` is the water
depth. The yardstick has to be **local** — a 40 s running median — because the
ground here runs from 13.8 m to 25.8 m, and testing against one survey-wide
median throws away good altitudes wherever the water is genuinely shallower. It
rejected 119 of 120 pings through the 30 s close-up pass at 09:32, which is
some of the best data in the survey. Local: 0.71% of pings rejected against
1.84% global, and the rejects are real — where the gate fires, the two
subsystems' independent bottom trackers disagree with a p95 of 12.4 m, against
1.4 m where it does not.

Note what is rejected: one *field*, the recorded altitude, used only for the
slant-range correction. No ping is ever dropped from the imagery. Where the
altitude is refused, the bottom is picked from the trace instead.

## Inter-line registration: measured, and not possible on this survey

`tools/register.py` implements the navigation adjustment described above —
mosaic each line separately, cross-correlate the seabed texture where two lines
overlap, solve for the per-line shift that satisfies every pair at once. It
works. It just finds nothing here.

**The correlator is exact.** Fed a line against itself with an injected shift it
returns the shift to 0.00 px with a peak-to-background score of 38–52.

**Real line pairs score 3.5–4.7.** Across 21 usable 512 px tiles on the best
pair, not one reaches 5. The highest-scoring tiles disagree with each other by
3–8 m in random directions, and their "shifts" pile up against the edge of the
search window — the signature of a correlation surface with no peak, where the
maximum lands wherever noise happens to be highest.

The reason is in the imagery. High-passed, the autocorrelation is gone by
0.9 m: at that scale it is pure speckle, which decorrelates completely between
two passes at different geometry. The raw image does carry content out to about
20 m — but admitting it *lowers* the match score (3.8 at a 0.9 m high-pass
radius, 2.3 at 43 m), because that large-scale content is dominated by each
line's own residual across-track gain profile. That is tied to the sonar
geometry, not to the ground, and adjacent lines run reciprocal courses, so
including it adds a strong common-mode pattern that actively misleads the
match.

So the residual softness in this mosaic is **not** measurably inter-line
misregistration, and cannot be corrected by texture matching. This corrects an
earlier claim here that registration was the single biggest remaining gain.

The tool refuses to write an adjustment it cannot support: a pair whose tiles
disagree by more than a quarter of the search range is thrown out, and with
fewer than two surviving pairs it writes nothing. A survey with real
morphology — ripples, boulders, wrecks, sediment boundaries — should register
normally, and the same tool will do it.

## Attitude and layback

The fish carries a full attitude sensor and every ping records it: heading,
pitch and roll, validity bits set on 100% of pings, at the 16 Hz ping rate.
It is real data — lag-1 autocorrelation 0.996, and pitch tracks the completely
independent pressure sensor's rate of depth change at **r = −0.960**.

The heading is the *fish's*, not the vessel's: through turns it lags the
vessel's course by 20 s (correlation 0.82 at that lag against 0.34 at zero).
Only something on the end of a tow cable does that.

**Layback comes from a tow model, not from geometry.** The cable is a curve
set by its own weight and drag, so the horizontal offset depends on speed as
much as on scope — at 2 kn the fish hangs almost under the tow point, at 6 kn it
streams out nearly straight. `tools/layback.py` carries the model fitted to
every configuration in EdgeTech's *Towing Characteristics for the 4125*, 80
configurations and 780 observations, agreeing with their published figures to
about 1 % of cable deployed.

Treating the cable as a straight line — `sqrt(cable² − depth²)` — is badly
wrong here: with 40 m out it gives 38 m of layback where the model gives 13 m.

The scope itself need not be recorded. For a given cable and speed, fish depth
rises monotonically with scope, and the fish carries a pressure sensor, so
`scope="auto"` reads the scope back out of the depth and follows the winch even
when nobody logged it. It is less stable than a fixed scope wherever the fish
dives, so a known scope is preferred when there is one.

The fish is then placed by walking *back down the vessel's own track* by that
distance, not by offsetting along a bearing, so the geometry stays right
through turns.

## Layback, measured from the seabed

The tow model, the cable length and the rig all turned out to be ways of
*estimating* layback. The survey measures it directly.

Where the fish's track crosses its own earlier path, the water depth under it —
`altitude + depth`, recorded every ping — must be the same both times. A layback
error slides every sounding along-track, so the crossings begin to disagree, and
here the seabed slopes about 9.6 m per 100 m, which makes the disagreement large
enough to see. Sweeping the layback and watching the crossings agree gives:

| layback | 0 m | 8 m | 16 m | **21 m** | 26 m | 32 m |
|---|---|---|---|---|---|---|
| median depth disagreement | 0.309 | 0.274 | 0.201 | **0.173** | 0.192 | 0.207 |

**Layback = 21.1 m, bootstrap 21.0 ± 1.4 m**, over about 1300 crossing pairs at
least five minutes apart. It is a proper minimum, not a trend — the curve rises
again beyond it. Against the 8 m that had been assumed, the bootstrap separates
them completely (0.274 ± 0.007 against 0.174 ± 0.006, P = 100 %).

No tow model, no cable length and no knowledge of the rig go into that number.
It is the seabed disagreeing with itself.

Two caveats. Most of the crossings are between the first and second half of the
survey, so this constrains the average rather than any drift; the first half on
its own has 99 internal pairs and a sharp minimum at 20 m, which agrees. And the
tow model cannot reproduce it: to place the fish 21.1 m aft at the measured
1.76 m/s it needs 27 m of the lightest cable, and then predicts the fish 2.6 m
deeper than the pressure sensor says. The real cable is flatter than the model —
more drag, or less weight, than EdgeTech's curves carry at this very short
scope, which is far below the 50 m shortest case they tabulated. Prefer the
measurement.

## Recovering what was never recorded

The current, the speed through the water and the cable out are all absent from
the file, and all three follow from what is there.

The towfish weathervanes into its own relative flow, so its compass points
along its velocity *through* the water, while the positions give velocity over
ground. The two differ by the current:

    V_og(t) = W(t) · h(t) + c(t)

Over a window short enough that the current holds but long enough to contain
both leg headings, that is two equations per second for three unknowns, and it
is the heading reversal every leg that makes it well conditioned.
`tools/solve_tow.py` solves it, and solves for the compass bias at the same
time by taking the value that leaves the smallest residual.

Turns must be excluded. The fish's heading lags the vessel's by about 20 s, so
through a turn its compass points where the vessel used to be going; dropping
turns takes the residual from 0.44 m/s to **0.20 m/s**.

On this survey it recovers a tidal stream setting **016°** at 0.11–0.89 m/s,
steady in direction across two hours, with the vessel making 1.1–2.2 m/s
through the water. That is why the legs look so unequal over ground: 1.03 m/s
southbound against 2.56 m/s northbound is one vessel speed with 0.8 m/s of tide
subtracted and added.

The check is that correcting for it makes the layback behave. Driven by speed
over ground the tow model swings between 4.5 m and 15.3 m of layback as the
vessel turns; driven by the recovered speed through water it settles at
**12.7 m, p10–p90 10.6–14.0 m**. Nothing forced that tightening.

Scope then follows from the fish's depth. It comes out at 18 m median — but its
*variation* over the survey should not be trusted: it correlates with the
recovered speed (r = −0.48) more strongly than with water depth (r = +0.29),
which is the signature of a small error in the model's speed dependence being
absorbed as scope rather than of a winch being used. Take the median; ignore
the drift.

**On this survey the tow model contradicts the stated cable length.** With 40 m
out it puts the fish at 23–36 m depth in 24 m of water — on the seabed. The
recorded depths correspond to roughly 13–18 m of scope and a layback near 13 m.
Two things confound the reading and neither is resolved: there is a strong
along-track current (southbound legs make 1.03 m/s over ground against 2.56 m/s
northbound, and the fish flies on speed through water), and the fish's depth
changes a great deal through the survey. Treat the scope as measured from the
pressure sensor, not as the number on the winch.

**The swath is a fan in the fish's athwartships plane.** Yaw points that plane,
pitch tilts it fore and aft, roll rolls it. Each ray is dropped onto a flat
seabed one altitude below the fish and the hit point read off. Two consequences
worth knowing:

- Pitch translates the whole swath along-track by `altitude × tan(pitch)` — the
  same displacement at every range, not a range-proportional one. At 13.7 m
  altitude and 13° of pitch that is 3.0 m.
- Roll and yaw rotate the fan, so a sample's along-track position depends on
  its across-track position; the swath is no longer a straight line abeam.

**The compass bias is measured, not assumed.** A cross-current crabs the fish
one way on one heading and the other way on the reciprocal, so it cancels in
the mean of the two; whatever offset survives that average is the compass. Here
that is **−4.52°**, with a further ±2.8° of genuine cross-current crab that
should be kept because it is real.

Enable it with `--attitude --cable 40 --heading-offset auto`.

## What is still missing


- **Explicit seam handling.** The exponent blend hides tone steps but there is
  no optimal-seam-line search.
- **Speckle reduction** before mosaicking.
- **Terrain.** Slant-range correction assumes a flat seabed at the tracked
  altitude, which is fine here (relief is small) but not in general.
- **Attitude.** Pitch, roll and yaw are recorded and are not applied.

## Sources

- [MB-System `mbmosaic` manual](https://www3.mbari.org/data/mbsystem/html/mbmosaic.html)
- [Zhao et al., *A New Radiometric Correction Method for Side-Scan Sonar Images in Consideration of Seabed Sediment Variation*](https://doi.org/10.3390/rs9060575)
- [*Lambert's Cosine Law and Sidescan Sonar Modeling*](https://www.diva-portal.org/smash/get/diva2:1473933/FULLTEXT01.pdf)
- [Kongsberg, *Backscattering and Seabed Image Reflectivity*](https://www.kongsberg.com/globalassets/kongsberg-discovery/commerce/seafloor-mapping/em2040-mkii/em_technical_note_web_backscatteringseabedimagereflectivity.pdf)
- [Zhang et al., *Side-Scan Sonar Image Mosaic Using Couple Feature Points with Constraint of Track Line Positions*](https://www.researchgate.net/publication/325798123_Side-Scan_Sonar_Image_Mosaic_Using_Couple_Feature_Points_with_Constraint_of_Track_Line_Positions)
- [*A Geometrically Consistent Matching Framework for Side-Scan Sonar Mapping*](https://arxiv.org/pdf/2509.11255)
- [*Detecting shifts of submarine sediment boundaries using side-scan mosaics and GIS analyses*](https://www.sciencedirect.com/science/article/pii/S0025322720302310)
- [Beaudoin et al., *Geometric and Radiometric Correction of Multibeam Backscatter*](http://www.omg.unb.ca/omg/papers/Beaudoin_Multibeam_Backscatter_Reson_8101_Systems.pdf)
