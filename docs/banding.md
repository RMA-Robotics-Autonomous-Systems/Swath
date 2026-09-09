# Along-track banding — what it actually is

Handoff note. The mosaic bands along track, the bands stop part-way across the
swath, and three earlier explanations in this repo were wrong or incomplete.
This is what the measurements say, so that the next person does not have to buy
them again.

Everything below is `070926_measures_star`, subsystem 20 (553–608 kHz), unless
another recording is named. Levels are the 50th percentile of the ground-mapped
swath outside the nadir band, per ping, and "smoothed" means a ±6-ping running
mean — enough to put single-look speckle below the thing being measured and not
enough to hide it.

## Where things stand

Branch `new-process`, HEAD `fe30fcd`. 43 Rust tests and 173 browser checks pass.
The working tree is clean apart from live app state, which is not ours to
commit — see [Standing constraints](#standing-constraints).

## The decomposition

Half of the banding is common to the ping and half is a see-saw between the two
sides.

| | smoothed swing (p95/p5) |
|---|---|
| both sides averaged | 1.30× |
| port alone | 1.31× |
| starboard alone | 1.38× |
| r(port, starboard) | +0.50 |

That r = 0.50 is the whole finding. **A band is not the ping getting brighter.
It is one side getting brighter while the other gets darker**, and that is why a
band crosses part of the swath and stops. Any correction that scales the ping —
which is what `MosaicConfig::agc` does today — cannot touch it by construction,
because it multiplies both sides by the same number.

## The see-saw is the fish rolling

Over 8000 pings, 12–42 m of ground:

```
roll        min -0.95   p5 +0.34   p50 +1.70   p95 +3.25   max +4.88 deg
imbalance   p5 -0.197   p50 -0.116  p95 -0.003        (port-stbd)/(port+stbd)
r(imbalance, roll) = -0.75      3.3% of brightness per degree of roll
```

The array sits 33° down with a 50° vertical beam. Its measured shape, range
compensated against a fixed 25 m reference so only the beam is left:

| degrees off vertical | 20 | 26 | 32 | 35 | 47 | 53 | 62 | 71 |
|---|---|---|---|---|---|---|---|---|
| level | 8.9 | 10.1 | 14.2 | 14.7 | **14.9** | 13.5 | 9.5 | 5.9 |

Flat from 32° to 51°, falling off hard either side. A degree of roll tips that
whole pattern: one side's fan swings down toward nadir and brightens, the other
swings up into its own falling edge and dims, in the same ping.

The imbalance decorrelates over about 20 pings — 4 m — and is still at r = 0.13
a hundred pings out, so it is not a jitter that averaging will fix.

Two more things fall out of the same numbers, both worth saying out loud:

- **The imbalance never changes sign.** Starboard is 12% brighter than port on
  the median ping of this recording and brighter on essentially all of them.
- **The roll never changes sign either**, on 98% of pings. The fish flies with a
  standing list of about 1.7°. That is a mount to check on the next mobilisation,
  not something to correct in software.

## The decisive test: the two bands

Subsystems 20 and 21 — 580 kHz and 240 kHz — ping the same water at the same
instant through different transducers and different receive chains. Their ADC
gain fields are uncorrelated (**r = 0.000, never once identical**). Their banding
is not:

| smoothing | common level | port/starboard imbalance |
|---|---|---|
| none | +0.774 | +0.403 |
| ±6 pings | **+0.808** | +0.593 |
| ±20 | +0.723 | +0.656 |
| ±60 | +0.729 | +0.705 |

Control — the same band against itself offset by 300 pings — is **+0.18**.

So the bands are not a channel, not an amplifier, not our painter. What two
independent sonars looking at the same ground at the same moment can share is
the seabed and the platform, and the split above says which is which: the common
half is the seabed, the see-saw half is the platform.

## Discover's own AGC is running, and it clips

The type-80 header carries a gain factor at byte 120. It is parsed into
`jsf::Ping::gain` and read by nothing downstream. Over 4000 pings it takes 164
distinct values, and by decile:

```
gain 5449  ->  level 5.05
gain 6974  ->  level 4.81
gain 8004  ->  level 4.62
gain 8082  ->  level 4.44     <- the top three deciles are all exactly 8082
gain 8082  ->  level 4.21
```

`r(level, gain) = -0.46`. The gain rises as the seabed darkens, which is an AGC
doing its job, and it **saturates at 8082 for the top 40% of pings**, which is an
AGC out of headroom. Two consequences:

- The recorded contrast is already partly flattened. Dividing the gain back out
  makes the level *more* variable, not less (1.78× against 1.20×), so this is not
  a correction to apply — it is a property of the data to know about.
- Port and starboard within one subsystem always share a gain value, which is
  what makes the two-band test above clean.

## What our own code contributes

`build_stroke` references the time-varied gain to the ping's own altitude
([mosaic.rs:679](../crates/swath-core/src/mosaic.rs#L679)), so every
wobble in flying height scales the whole ping:

| TVG reference | smoothed swing | r(level, altitude) |
|---|---|---|
| no gain at all | 1.337× | −0.19 |
| this ping's altitude *(current)* | 1.391× | **−0.43** |
| the recording's median altitude | 1.338× | +0.10 |
| median altitude, strength 1.0 | 1.345× | +0.21 |

Small in swing, but it ties brightness to flying height for no reason, which is
what makes it *look* systematic. The reference should be a property of the
recording, not of the ping.

## What was ruled out

- **The painter.** The bands are visible in the ground-mapped swath before
  anything is splatted into a raster.
- **Overlapping passes fighting over cells.** The bands are inside a single pass.
- **The electronics.** See the two-band test.
- **Speed and ping rate.** The sonar pings at a dead-steady 0.068–0.070 s while
  the boat varies 2.0–3.4 m/s, so along-track sample spacing runs 0.14–0.24 m —
  but `r(level, speed) = +0.02`.
- **Normalising by grazing angle instead of ground range.** Tested directly:
  swing 1.466× by angle against 1.457× by range. It does not help. This is the
  same conclusion as *"EGN will not fix this"*, reached from the other direction,
  and it is now measured rather than argued.
- **A grazing-angle slide.** The beam does slide across the ground as the fish
  rises and falls, but brightness moves with altitude in the *same* direction all
  the way out (−0.21 at nadir to −0.43 at 15 m, weakening but never flipping), so
  it is not the profile see-sawing about its own crest.

Jointly, ADC gain and altitude account for r² = 0.21 of the common level and take
its smoothed swing from 1.303× to 1.252×. The remainder is the seabed.

## What this supersedes

[`survey/README.md`](../survey/README.md), *"Along-track banding, and what it is
not"*. Its measurements stand — the 30% speckle, the 96→146 grey swath mean, the
r = 0.245 shared component, the warning that sampling one across-track distance
hides all of it. What is wrong is the emphasis: it calls the shared quarter
*"the part that reads as a band crossing the whole swath"* and leaves the rest as
*"either the seabed or something specific to one side"*. That hedge is now
resolved, and the side-specific part is both the larger half and the one with a
name. **Rewrite that section against this note before the next report goes out.**

## What to do next, ranked

1. **A gain per side, not per ping.** `Stroke` carries one `level`
   ([mosaic.rs:609](../crates/swath-core/src/mosaic.rs#L609)); it needs
   two, and `equalise` ([mosaic.rs:752](../crates/swath-core/src/mosaic.rs#L752))
   needs to run twice. Measured, at half strength over 12–42 m of ground, the
   imbalance goes 19.4 → 14.3 points on a 41-ping window and 19.4 → 13.3 on a
   101-ping window; at full strength it goes by construction. Keep the window
   long — a per-side gain also erases genuine port/starboard differences in the
   seabed, and 41 pings is only 8 m.
2. **Reference the TVG to the recording, not the ping.** One line at
   [mosaic.rs:679](../crates/swath-core/src/mosaic.rs#L679), plus a median
   altitude on `MosaicConfig`. Bumps `PAINTER`.
3. **Survey-wide stretch** — slice 1 of the approved plan, and still the first
   thing worth doing for how the mosaic *looks*. `Stretch::from_data` runs per
   mosaic ([mosaic.rs:430](../crates/swath-core/src/mosaic.rs#L430)), so
   grey means nothing between recordings, and a narrow black/white point in the
   viewer turns a 1.5× swing into a full-scale colour swing. Some of what reads
   as banding in a viridis screenshot is this.
4. The rest of the approved plan: a dynamic-programming bottom tracker, stored
   bottom lines with a manual override, EGN as a separable `g(r)·h(α)`, and
   reporting the gain model that was applied. The DTM slice was dropped by the
   operator. Phase 4 of the older plan — per-line manual offsets — is still
   unbuilt and needs a UI interaction that cannot be driven from here.

Roll is recorded per ping and the relationship is linear at 3.3% per degree, so a
direct roll correction is available and would be more honest than an AGC. It is
below the per-side gain in this list only because the per-side gain needs no new
calibration and cannot be wrong about the sign convention.

## Open with the operator

- Commit `355c36f` swept in work that was in flight and not written here: a
  `Server::new` refactor in `server.rs` and both `main.rs`, PNG channel-narrowing
  in `mosaic.rs`, and the tile-loading tests. `git reset --soft HEAD^` would put
  it back staged to untangle. Offered, not yet answered.
- Whether to blank the nadir by default. `nadir_blank_m` is now reachable from
  the settings panel and suggests `0.62 × altitude`, but the default is still 0.

## Standing constraints

- `projects/*/project.json`, `projects/wpa-rust/marks.geojson` and
  `projects/report/` are live app and user state. Never commit them.
- `data/` (19 GB), `*.jsf`, `*.xtf`, `out/*` and `target/` stay out of git.
- OSM tile policy forbids bulk downloading; `survey tiles` is paced and guarded.
- `tow.npz` and the old layback model are not to be used.
- Do not synthesise pointer or keyboard input. The operator is at the machine.

## Re-measuring

The harness was a scratch integration test at `crates/swath-core/tests/zz.rs`,
deleted after use — it was ad-hoc and would have rotted. The method, if it is
needed again:

Load the ping index, pair the channels with `waterfall::pair_channels`, and for
each ping ground-map the trace the way `build_stroke` does — `steady_prior` over
the recorded altitudes, `refine_bottom_within` for the pick, `sample_at` at
`sqrt(g² + alt²) / res`. Take a percentile of the samples beyond the nadir band
as that ping's level, once per side. Everything above is a correlation between
two such series, or between one of them and a header field. Two rules earned the
hard way:

- **Take the level over the swath, not at one across-track distance.** A single
  sample is speckle-dominated and reports r = 0.011 where the swath mean reports
  r = 0.245. An underpowered measurement here reads as a null result.
- **Carry a positive control.** The same series against itself at a 300-ping
  offset scored +0.18, which is what makes the +0.81 across bands mean something.

The pictures that came out of it — a beam profile, the residual swath with the
roll trace beside it, mosaic overviews of each recording — were written to
`out/scratch/` and are not tracked.
