//! A small registry of coordinate reference systems, keyed by EPSG code.
//!
//! Deliberately small. A survey report needs the handful of grids the client
//! actually asks for -- WGS84, the local UTM zone, the national grid, whatever
//! the old charts were drawn on -- not the whole EPSG database. Adding one is a
//! line in `ALL` plus a `Projection` variant.

use serde::{Deserialize, Serialize};

use crate::geo::{
    self, Ellipsoid, Helmert, LambertAzimuthalEqualArea, ObliqueStereographic,
    TransverseMercator,
};

#[derive(Clone, Copy, Debug)]
enum Projection {
    /// Geographic; "easting/northing" are longitude/latitude in degrees.
    Geographic,
    Tm(TransverseMercator),
    Stereo(ObliqueStereographic),
    Laea(LambertAzimuthalEqualArea),
    WebMercator,
}

#[derive(Clone, Copy, Debug)]
pub struct Crs {
    pub epsg: u32,
    pub name: &'static str,
    /// Axis labels in report order, e.g. ("Easting", "Northing").
    pub axes: (&'static str, &'static str),
    pub unit: &'static str,
    /// Decimal places worth printing for this unit.
    pub decimals: usize,
    proj: Projection,
    /// Datum shift from WGS84 into this CRS's own datum.
    to_datum: Option<(Ellipsoid, Helmert)>,
}

impl Crs {
    /// WGS84 lat/lon -> this CRS. Returns (x, y) in the CRS's own axis order,
    /// which for a geographic CRS is (longitude, latitude).
    pub fn from_wgs84(&self, lat: f64, lon: f64) -> (f64, f64) {
        let (lat, lon) = match self.to_datum {
            Some((el, h)) => geo::datum_shift(Ellipsoid::WGS84, el, h, lat, lon),
            None => (lat, lon),
        };
        match self.proj {
            Projection::Geographic => (lon, lat),
            Projection::Tm(p) => p.forward(lat, lon),
            Projection::Stereo(p) => p.forward(lat, lon),
            Projection::Laea(p) => p.forward(lat, lon),
            Projection::WebMercator => {
                let (x, y) = geo::lonlat_to_px(lon, lat, 0.0);
                let s = 2.0 * std::f64::consts::PI * 6378137.0 / 256.0;
                ((x - 128.0) * s, (128.0 - y) * s)
            }
        }
    }

    /// This CRS -> WGS84 lat/lon.
    pub fn to_wgs84(&self, x: f64, y: f64) -> (f64, f64) {
        let (lat, lon) = match self.proj {
            Projection::Geographic => (y, x),
            Projection::Tm(p) => p.inverse(x, y),
            Projection::Stereo(p) => p.inverse(x, y),
            Projection::Laea(p) => p.inverse(x, y),
            Projection::WebMercator => {
                let s = 2.0 * std::f64::consts::PI * 6378137.0 / 256.0;
                let (lon, lat) = geo::px_to_lonlat(x / s + 128.0, 128.0 - y / s, 0.0);
                (lat, lon)
            }
        };
        match self.to_datum {
            Some((el, h)) => geo::datum_shift(el, Ellipsoid::WGS84, h.inverse(), lat, lon),
            None => (lat, lon),
        }
    }

    pub fn is_geographic(&self) -> bool {
        matches!(self.proj, Projection::Geographic)
    }

    /// Format one position for a report cell.
    pub fn format(&self, lat: f64, lon: f64) -> (String, String) {
        let (x, y) = self.from_wgs84(lat, lon);
        (format!("{x:.*}", self.decimals), format!("{y:.*}", self.decimals))
    }
}

/// The fixed part of the registry. UTM zones are generated on demand by
/// `get`, so every zone works without 120 entries here.
pub const ALL: &[Crs] = &[
    Crs {
        epsg: 4326,
        name: "WGS 84 (geographic)",
        axes: ("Longitude", "Latitude"),
        unit: "deg",
        decimals: 8,
        proj: Projection::Geographic,
        to_datum: None,
    },
    Crs {
        epsg: 3857,
        name: "WGS 84 / Pseudo-Mercator",
        axes: ("Easting", "Northing"),
        unit: "m",
        decimals: 2,
        proj: Projection::WebMercator,
        to_datum: None,
    },
];

/// Look up a CRS by EPSG code. UTM zones (326xx north, 327xx south) and the
/// two national grids this survey area needs are built here.
pub fn get(epsg: u32) -> Option<Crs> {
    if let Some(c) = ALL.iter().find(|c| c.epsg == epsg) {
        return Some(*c);
    }
    match epsg {
        // WGS 84 / UTM
        32601..=32660 => Some(utm_crs(epsg, (epsg - 32600) as u8, true, "WGS 84 / UTM zone")),
        32701..=32760 => Some(utm_crs(epsg, (epsg - 32700) as u8, false, "WGS 84 / UTM zone")),
        // ETRS89 / UTM -- same ellipsoid to well within survey tolerance
        25828..=25838 => Some(utm_crs(epsg, (epsg - 25800) as u8, true, "ETRS89 / UTM zone")),
        // Amersfoort / RD New
        28992 => Some(Crs {
            epsg: 28992,
            name: "Amersfoort / RD New",
            axes: ("X", "Y"),
            unit: "m",
            decimals: 3,
            proj: Projection::Stereo(ObliqueStereographic::rd()),
            to_datum: Some((Ellipsoid::BESSEL1841, Helmert::AMERSFOORT_TO_WGS84.inverse())),
        }),
        // ETRS89-extended / LAEA Europe
        3035 => Some(Crs {
            epsg: 3035,
            name: "ETRS89-extended / LAEA Europe",
            axes: ("Easting", "Northing"),
            unit: "m",
            decimals: 2,
            proj: Projection::Laea(LambertAzimuthalEqualArea::etrs89_laea()),
            to_datum: None,
        }),
        // ED50 / UTM zone 31N -- what the older North Sea charts are on
        23031 => Some(Crs {
            epsg: 23031,
            name: "ED50 / UTM zone 31N",
            axes: ("Easting", "Northing"),
            unit: "m",
            decimals: 2,
            proj: Projection::Tm(TransverseMercator {
                el: Ellipsoid::INTL1924,
                lat0: 0.0,
                lon0: 3.0,
                k0: 0.9996,
                fe: 500000.0,
                fn_: 0.0,
            }),
            to_datum: Some((Ellipsoid::INTL1924, Helmert::ED50_TO_WGS84.inverse())),
        }),
        _ => None,
    }
}

fn utm_crs(epsg: u32, zone: u8, north: bool, _label: &'static str) -> Crs {
    Crs {
        epsg,
        name: utm_name(zone, north),
        axes: ("Easting", "Northing"),
        unit: "m",
        decimals: 2,
        proj: Projection::Tm(TransverseMercator::utm(zone, north)),
        to_datum: None,
    }
}

/// Zone names are wanted as `&'static str` for the registry, and there are only
/// 120 of them, so they are interned once here rather than leaked per call.
fn utm_name(zone: u8, north: bool) -> &'static str {
    const NORTH: [&str; 61] = [
        "", "UTM zone 1N", "UTM zone 2N", "UTM zone 3N", "UTM zone 4N", "UTM zone 5N",
        "UTM zone 6N", "UTM zone 7N", "UTM zone 8N", "UTM zone 9N", "UTM zone 10N",
        "UTM zone 11N", "UTM zone 12N", "UTM zone 13N", "UTM zone 14N", "UTM zone 15N",
        "UTM zone 16N", "UTM zone 17N", "UTM zone 18N", "UTM zone 19N", "UTM zone 20N",
        "UTM zone 21N", "UTM zone 22N", "UTM zone 23N", "UTM zone 24N", "UTM zone 25N",
        "UTM zone 26N", "UTM zone 27N", "UTM zone 28N", "UTM zone 29N", "UTM zone 30N",
        "UTM zone 31N", "UTM zone 32N", "UTM zone 33N", "UTM zone 34N", "UTM zone 35N",
        "UTM zone 36N", "UTM zone 37N", "UTM zone 38N", "UTM zone 39N", "UTM zone 40N",
        "UTM zone 41N", "UTM zone 42N", "UTM zone 43N", "UTM zone 44N", "UTM zone 45N",
        "UTM zone 46N", "UTM zone 47N", "UTM zone 48N", "UTM zone 49N", "UTM zone 50N",
        "UTM zone 51N", "UTM zone 52N", "UTM zone 53N", "UTM zone 54N", "UTM zone 55N",
        "UTM zone 56N", "UTM zone 57N", "UTM zone 58N", "UTM zone 59N", "UTM zone 60N",
    ];
    const SOUTH: [&str; 61] = [
        "", "UTM zone 1S", "UTM zone 2S", "UTM zone 3S", "UTM zone 4S", "UTM zone 5S",
        "UTM zone 6S", "UTM zone 7S", "UTM zone 8S", "UTM zone 9S", "UTM zone 10S",
        "UTM zone 11S", "UTM zone 12S", "UTM zone 13S", "UTM zone 14S", "UTM zone 15S",
        "UTM zone 16S", "UTM zone 17S", "UTM zone 18S", "UTM zone 19S", "UTM zone 20S",
        "UTM zone 21S", "UTM zone 22S", "UTM zone 23S", "UTM zone 24S", "UTM zone 25S",
        "UTM zone 26S", "UTM zone 27S", "UTM zone 28S", "UTM zone 29S", "UTM zone 30S",
        "UTM zone 31S", "UTM zone 32S", "UTM zone 33S", "UTM zone 34S", "UTM zone 35S",
        "UTM zone 36S", "UTM zone 37S", "UTM zone 38S", "UTM zone 39S", "UTM zone 40S",
        "UTM zone 41S", "UTM zone 42S", "UTM zone 43S", "UTM zone 44S", "UTM zone 45S",
        "UTM zone 46S", "UTM zone 47S", "UTM zone 48S", "UTM zone 49S", "UTM zone 50S",
        "UTM zone 51S", "UTM zone 52S", "UTM zone 53S", "UTM zone 54S", "UTM zone 55S",
        "UTM zone 56S", "UTM zone 57S", "UTM zone 58S", "UTM zone 59S", "UTM zone 60S",
    ];
    let i = zone.clamp(1, 60) as usize;
    if north { NORTH[i] } else { SOUTH[i] }
}

/// The UTM EPSG code covering this position.
pub fn utm_epsg_for(lat: f64, lon: f64) -> u32 {
    let z = geo::utm_zone(lon) as u32;
    if lat >= 0.0 { 32600 + z } else { 32700 + z }
}

/// What to offer in a CRS picker for a survey at this position: always WGS84,
/// the local UTM zone, and the regional grids that apply.
pub fn suggestions(lat: f64, lon: f64) -> Vec<Crs> {
    let mut v = vec![get(4326).unwrap(), get(utm_epsg_for(lat, lon)).unwrap()];
    // Dutch continental shelf and coastal waters
    if (50.0..=54.0).contains(&lat) && (3.0..=8.0).contains(&lon) {
        v.push(get(28992).unwrap());
    }
    if (34.0..=72.0).contains(&lat) && (-25.0..=45.0).contains(&lon) {
        v.push(get(3035).unwrap());
        v.push(get(23031).unwrap());
    }
    v
}

/// A degrees/minutes/seconds rendering, for the reports that want one.
pub fn to_dms(deg: f64, is_lat: bool) -> String {
    let hemi = if is_lat {
        if deg >= 0.0 { 'N' } else { 'S' }
    } else if deg >= 0.0 {
        'E'
    } else {
        'W'
    };
    let a = deg.abs();
    let d = a.floor();
    let m = (a - d) * 60.0;
    let mi = m.floor();
    let s = (m - mi) * 60.0;
    format!("{:.0}\u{b0} {:02.0}' {:06.3}\" {}", d, mi, s, hemi)
}

/// Decimal degrees and minutes, the form most survey logs use.
pub fn to_dm(deg: f64, is_lat: bool) -> String {
    let hemi = if is_lat {
        if deg >= 0.0 { 'N' } else { 'S' }
    } else if deg >= 0.0 {
        'E'
    } else {
        'W'
    };
    let a = deg.abs();
    let d = a.floor();
    format!("{:.0}\u{b0} {:08.5}' {}", d, (a - d) * 60.0, hemi)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CrsInfo {
    pub epsg: u32,
    pub name: String,
    pub axes: [String; 2],
    pub unit: String,
}

impl From<Crs> for CrsInfo {
    fn from(c: Crs) -> CrsInfo {
        CrsInfo {
            epsg: c.epsg,
            name: c.name.to_string(),
            axes: [c.axes.0.to_string(), c.axes.1.to_string()],
            unit: c.unit.to_string(),
        }
    }
}
