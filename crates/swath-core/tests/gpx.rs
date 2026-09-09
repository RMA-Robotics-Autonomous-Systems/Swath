//! The GPX scanner, on the shapes real files come in.
//!
//! These are written out by hand rather than generated, because the point is
//! the awkward cases: a self-closing point, an escaped name, a timestamp with
//! an offset, a file that is XML but not GPX.

use swath_core::gpx;

#[test]
fn reads_a_track_with_times_and_elevations() {
    let src = r#"<?xml version="1.0"?>
<gpx version="1.1" creator="test">
  <metadata><name>Day two</name></metadata>
  <trk>
    <name>Line 01</name>
    <trkseg>
      <trkpt lat="52.5601" lon="4.0612"><ele>-23.4</ele><time>2026-09-06T09:13:22Z</time></trkpt>
      <trkpt lat="52.5611" lon="4.0622"><ele>-23.9</ele><time>2026-09-06T09:13:32Z</time></trkpt>
      <trkpt lat="52.5621" lon="4.0632"/>
    </trkseg>
  </trk>
</gpx>"#;
    let g = gpx::parse(src).expect("parse");
    assert_eq!(g.tracks.len(), 1, "one track");
    let t = &g.tracks[0];
    assert_eq!(t.name, "Line 01");
    assert_eq!(t.points.len(), 3, "the self-closing point must not be lost");
    assert_eq!(t.points[0].ele, Some(-23.4));
    let dt = t.points[1].time.unwrap() - t.points[0].time.unwrap();
    assert!((dt - 10.0).abs() < 1e-6, "ten seconds apart, got {dt}");
    // 2026-09-06T09:13:22Z
    assert!(
        (t.points[0].time.unwrap() - 1_788_686_002.0).abs() < 1.0,
        "epoch seconds {:?}",
        t.points[0].time
    );
    assert!(g.bounds.min_lat <= 52.5601 && g.bounds.max_lat >= 52.5621);
    let len = g.length_m();
    assert!(len > 200.0 && len < 350.0, "two ~130 m hops, got {len:.1} m");
}

#[test]
fn reads_waypoints_and_routes_and_unescapes() {
    let src = r#"<gpx version="1.1">
  <wpt lat="52.55" lon="4.05"><name>Wreck &amp; scour</name><desc>needs ROV</desc></wpt>
  <wpt lat="52.56" lon="4.06"/>
  <rte><name>Planned</name>
    <rtept lat="52.50" lon="4.00"/><rtept lat="52.60" lon="4.10"/>
  </rte>
</gpx>"#;
    let g = gpx::parse(src).expect("parse");
    assert_eq!(g.waypoints.len(), 2);
    assert_eq!(g.waypoints[0].name, "Wreck & scour");
    assert_eq!(g.waypoints[0].desc, "needs ROV");
    assert_eq!(g.routes.len(), 1);
    assert_eq!(g.routes[0].points.len(), 2);
    assert_eq!(g.points(), 4);
    let fc = g.to_geojson();
    assert_eq!(fc["features"].as_array().unwrap().len(), 3, "route + two waypoints");
}

#[test]
fn refuses_xml_that_is_not_gpx() {
    // A KML dropped in by mistake should say so rather than silently importing
    // an empty layer that the operator then tries to find on the chart.
    let src = r#"<?xml version="1.0"?><kml><Document><name>x</name></Document></kml>"#;
    assert!(gpx::parse(src).is_err(), "a KML must not parse as GPX");
    assert!(gpx::parse("not xml at all").is_err());
}

#[test]
fn skips_points_that_are_not_on_the_earth() {
    let src = r#"<gpx><trk><trkseg>
      <trkpt lat="52.5" lon="4.0"/>
      <trkpt lat="999" lon="4.0"/>
      <trkpt lat="52.6" lon="4.1"/>
    </trkseg></trk></gpx>"#;
    let g = gpx::parse(src).expect("parse");
    assert_eq!(g.tracks[0].points.len(), 2, "the impossible latitude is dropped");
    assert!(g.bounds.max_lat < 90.0);
}
