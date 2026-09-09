//! Geodesy: ellipsoids, datum shifts and the projections a survey report needs.
//!
//! Pure Rust on purpose. The Python side reaches for pyproj, which is a binding
//! to C PROJ; carrying that into the Rust port would have left a C dependency
//! in the one place the rewrite was meant to remove them. The projections here
//! are the EPSG guidance-note formulas, and `tests/proj.rs` checks them against
//! pyproj-generated fixtures.

use std::f64::consts::PI;

pub const D2R: f64 = PI / 180.0;
pub const R2D: f64 = 180.0 / PI;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ellipsoid {
    pub a: f64,
    /// Inverse flattening.
    pub inv_f: f64,
}

impl Ellipsoid {
    pub const WGS84: Ellipsoid = Ellipsoid { a: 6378137.0, inv_f: 298.257223563 };
    pub const GRS80: Ellipsoid = Ellipsoid { a: 6378137.0, inv_f: 298.257222101 };
    pub const BESSEL1841: Ellipsoid = Ellipsoid { a: 6377397.155, inv_f: 299.1528128 };
    pub const INTL1924: Ellipsoid = Ellipsoid { a: 6378388.0, inv_f: 297.0 };

    pub fn f(&self) -> f64 {
        1.0 / self.inv_f
    }
    pub fn e2(&self) -> f64 {
        let f = self.f();
        2.0 * f - f * f
    }
    pub fn e(&self) -> f64 {
        self.e2().sqrt()
    }
    pub fn b(&self) -> f64 {
        self.a * (1.0 - self.f())
    }
}

/// Seven-parameter Helmert, position-vector convention (EPSG method 9606).
///
/// Rotations are in arc-seconds and the scale in parts per million, which is
/// how EPSG publishes them, so a transform can be typed in from the registry
/// without unit juggling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Helmert {
    pub dx: f64,
    pub dy: f64,
    pub dz: f64,
    pub rx_sec: f64,
    pub ry_sec: f64,
    pub rz_sec: f64,
    pub ds_ppm: f64,
}

impl Helmert {
    pub const IDENTITY: Helmert =
        Helmert { dx: 0.0, dy: 0.0, dz: 0.0, rx_sec: 0.0, ry_sec: 0.0, rz_sec: 0.0, ds_ppm: 0.0 };

    /// Amersfoort (RD) -> WGS84, EPSG:1672. Quoted accuracy about 1 m; the
    /// official RDNAPTRANS grid does better but is not redistributable.
    pub const AMERSFOORT_TO_WGS84: Helmert = Helmert {
        dx: 565.417,
        dy: 50.3319,
        dz: 465.552,
        rx_sec: -0.398957,
        ry_sec: 0.343988,
        rz_sec: -1.8774,
        ds_ppm: 4.0725,
    };

    /// ED50 -> WGS84 for the North Sea, EPSG:1311. Three-parameter, ~2 m.
    pub const ED50_TO_WGS84: Helmert = Helmert {
        dx: -84.87,
        dy: -96.49,
        dz: -116.95,
        rx_sec: 0.0,
        ry_sec: 0.0,
        rz_sec: 0.0,
        ds_ppm: 0.0,
    };

    pub fn inverse(&self) -> Helmert {
        Helmert {
            dx: -self.dx,
            dy: -self.dy,
            dz: -self.dz,
            rx_sec: -self.rx_sec,
            ry_sec: -self.ry_sec,
            rz_sec: -self.rz_sec,
            ds_ppm: -self.ds_ppm,
        }
    }

    /// Apply to geocentric cartesian coordinates.
    pub fn apply(&self, x: f64, y: f64, z: f64) -> (f64, f64, f64) {
        let s = 1.0 + self.ds_ppm * 1e-6;
        let (rx, ry, rz) = (
            self.rx_sec * D2R / 3600.0,
            self.ry_sec * D2R / 3600.0,
            self.rz_sec * D2R / 3600.0,
        );
        (
            self.dx + s * (x - rz * y + ry * z),
            self.dy + s * (rz * x + y - rx * z),
            self.dz + s * (-ry * x + rx * y + z),
        )
    }
}

/// Geodetic (degrees, metres) -> geocentric cartesian.
pub fn geodetic_to_geocentric(el: Ellipsoid, lat: f64, lon: f64, h: f64) -> (f64, f64, f64) {
    let (p, l) = (lat * D2R, lon * D2R);
    let e2 = el.e2();
    let n = el.a / (1.0 - e2 * p.sin() * p.sin()).sqrt();
    (
        (n + h) * p.cos() * l.cos(),
        (n + h) * p.cos() * l.sin(),
        (n * (1.0 - e2) + h) * p.sin(),
    )
}

/// Geocentric cartesian -> geodetic. Bowring's method, then two refinements;
/// converged to well under a millimetre for terrestrial heights.
pub fn geocentric_to_geodetic(el: Ellipsoid, x: f64, y: f64, z: f64) -> (f64, f64, f64) {
    let e2 = el.e2();
    let b = el.b();
    let ep2 = (el.a * el.a - b * b) / (b * b);
    let r = (x * x + y * y).sqrt();
    let lon = y.atan2(x) * R2D;
    if r < 1e-12 {
        return (if z >= 0.0 { 90.0 } else { -90.0 }, lon, z.abs() - b);
    }
    let theta = (z * el.a).atan2(r * b);
    let mut lat = (z + ep2 * b * theta.sin().powi(3)).atan2(r - e2 * el.a * theta.cos().powi(3));
    let mut h = 0.0;
    for _ in 0..3 {
        let n = el.a / (1.0 - e2 * lat.sin() * lat.sin()).sqrt();
        h = r / lat.cos() - n;
        lat = (z / r).atan2(1.0 - e2 * n / (n + h));
    }
    (lat * R2D, lon, h)
}

/// Datum shift between two geodetic frames via geocentric cartesian.
pub fn datum_shift(
    from: Ellipsoid,
    to: Ellipsoid,
    h: Helmert,
    lat: f64,
    lon: f64,
) -> (f64, f64) {
    let (x, y, z) = geodetic_to_geocentric(from, lat, lon, 0.0);
    let (x, y, z) = h.apply(x, y, z);
    let (lat, lon, _) = geocentric_to_geodetic(to, x, y, z);
    (lat, lon)
}

// ---- Transverse Mercator (Krüger series) -----------------------------------

/// Transverse Mercator, the Krüger series to sixth order. Sub-millimetre out to
/// about 4 degrees from the central meridian, which covers any UTM zone.
#[derive(Clone, Copy, Debug)]
pub struct TransverseMercator {
    pub el: Ellipsoid,
    pub lat0: f64,
    pub lon0: f64,
    pub k0: f64,
    pub fe: f64,
    pub fn_: f64,
}

impl TransverseMercator {
    pub fn utm(zone: u8, north: bool) -> TransverseMercator {
        TransverseMercator {
            el: Ellipsoid::WGS84,
            lat0: 0.0,
            lon0: zone as f64 * 6.0 - 183.0,
            k0: 0.9996,
            fe: 500000.0,
            fn_: if north { 0.0 } else { 10000000.0 },
        }
    }

    fn series(&self) -> (f64, [f64; 6], [f64; 6]) {
        let f = self.el.f();
        let n = f / (2.0 - f);
        let (n2, n3, n4, n5, n6) = (n * n, n * n * n, n.powi(4), n.powi(5), n.powi(6));
        // Rectifying radius
        let a_bar = self.el.a / (1.0 + n)
            * (1.0 + n2 / 4.0 + n4 / 64.0 + n6 / 256.0);
        // alpha: geodetic -> projected
        let alpha = [
            n / 2.0 - 2.0 / 3.0 * n2 + 5.0 / 16.0 * n3 + 41.0 / 180.0 * n4
                - 127.0 / 288.0 * n5 + 7891.0 / 37800.0 * n6,
            13.0 / 48.0 * n2 - 3.0 / 5.0 * n3 + 557.0 / 1440.0 * n4 + 281.0 / 630.0 * n5
                - 1983433.0 / 1935360.0 * n6,
            61.0 / 240.0 * n3 - 103.0 / 140.0 * n4 + 15061.0 / 26880.0 * n5
                + 167603.0 / 181440.0 * n6,
            49561.0 / 161280.0 * n4 - 179.0 / 168.0 * n5 + 6601661.0 / 7257600.0 * n6,
            34729.0 / 80640.0 * n5 - 3418889.0 / 1995840.0 * n6,
            212378941.0 / 319334400.0 * n6,
        ];
        // beta: projected -> geodetic
        let beta = [
            n / 2.0 - 2.0 / 3.0 * n2 + 37.0 / 96.0 * n3 - n4 / 360.0 - 81.0 / 512.0 * n5
                + 96199.0 / 604800.0 * n6,
            n2 / 48.0 + n3 / 15.0 - 437.0 / 1440.0 * n4 + 46.0 / 105.0 * n5
                - 1118711.0 / 3870720.0 * n6,
            17.0 / 480.0 * n3 - 37.0 / 840.0 * n4 - 209.0 / 4480.0 * n5
                + 5569.0 / 90720.0 * n6,
            4397.0 / 161280.0 * n4 - 11.0 / 504.0 * n5 - 830251.0 / 7257600.0 * n6,
            4583.0 / 161280.0 * n5 - 108847.0 / 3991680.0 * n6,
            20648693.0 / 638668800.0 * n6,
        ];
        (a_bar, alpha, beta)
    }

    /// Meridian arc from the equator to `lat` (radians), scaled by the
    /// rectifying radius -- the same conformal-latitude route `forward` takes,
    /// evaluated on the central meridian where eta is zero.
    fn meridian_arc(&self, lat: f64) -> f64 {
        if lat.abs() < 1e-15 {
            return 0.0;
        }
        let (a_bar, alpha, _) = self.series();
        let e = self.el.e();
        let tau = lat.tan();
        let sigma = (e * (e * tau / (1.0 + tau * tau).sqrt()).atanh()).sinh();
        let tau_p = tau * (1.0 + sigma * sigma).sqrt() - sigma * (1.0 + tau * tau).sqrt();
        let xi_p = tau_p.atan2(1.0);
        let mut xi = xi_p;
        for (j, &al) in alpha.iter().enumerate() {
            let k = 2.0 * (j as f64 + 1.0);
            xi += al * (k * xi_p).sin();
        }
        a_bar * xi
    }

    pub fn forward(&self, lat_deg: f64, lon_deg: f64) -> (f64, f64) {
        let (a_bar, alpha, _) = self.series();
        let e = self.el.e();
        let lat = lat_deg * D2R;
        let mut dlon = (lon_deg - self.lon0) * D2R;
        // keep the longitude difference in (-pi, pi]
        while dlon > PI {
            dlon -= 2.0 * PI;
        }
        while dlon <= -PI {
            dlon += 2.0 * PI;
        }
        let tau = lat.tan();
        let sigma = (e * (e * tau / (1.0 + tau * tau).sqrt()).atanh()).sinh();
        let tau_p = tau * (1.0 + sigma * sigma).sqrt() - sigma * (1.0 + tau * tau).sqrt();
        let xi_p = tau_p.atan2(dlon.cos());
        let eta_p = (dlon.sin() / (tau_p * tau_p + dlon.cos() * dlon.cos()).sqrt()).asinh();
        let (mut xi, mut eta) = (xi_p, eta_p);
        for (j, &al) in alpha.iter().enumerate() {
            let k = 2.0 * (j as f64 + 1.0);
            xi += al * (k * xi_p).sin() * (k * eta_p).cosh();
            eta += al * (k * xi_p).cos() * (k * eta_p).sinh();
        }
        let easting = self.fe + self.k0 * a_bar * eta;
        let northing = self.fn_ + self.k0 * (a_bar * xi - self.meridian_arc(self.lat0 * D2R));
        (easting, northing)
    }

    pub fn inverse(&self, easting: f64, northing: f64) -> (f64, f64) {
        let (a_bar, _, beta) = self.series();
        let e = self.el.e();
        let eta = (easting - self.fe) / (self.k0 * a_bar);
        let xi = (northing - self.fn_ + self.k0 * self.meridian_arc(self.lat0 * D2R))
            / (self.k0 * a_bar);
        let (mut xi_p, mut eta_p) = (xi, eta);
        for (j, &be) in beta.iter().enumerate() {
            let k = 2.0 * (j as f64 + 1.0);
            xi_p -= be * (k * xi).sin() * (k * eta).cosh();
            eta_p -= be * (k * xi).cos() * (k * eta).sinh();
        }
        let tau_p = xi_p.sin() / (eta_p.sinh() * eta_p.sinh() + xi_p.cos() * xi_p.cos()).sqrt();
        // Newton on tau, following GeographicLib's `tauf`. The obvious
        // derivative is not the right one -- the conformal latitude enters
        // twice -- and getting it wrong costs hundreds of metres while leaving
        // the forward direction looking perfect, which is exactly what the
        // round-trip test in tests/parity.rs is there to catch.
        let e2m = 1.0 - e * e;
        let mut tau = tau_p / e2m;
        for _ in 0..8 {
            let tau1 = (1.0 + tau * tau).sqrt();
            let sig = (e * (e * tau / tau1).atanh()).sinh();
            let taupa = (1.0 + sig * sig).sqrt() * tau - sig * tau1;
            let dtau = (tau_p - taupa) * (1.0 + e2m * tau * tau)
                / (e2m * tau1 * (1.0 + taupa * taupa).sqrt());
            tau += dtau;
            if dtau.abs() < 1e-14 * (1.0 + tau.abs()) {
                break;
            }
        }
        let lat = tau.atan() * R2D;
        let lon = self.lon0 + eta_p.sinh().atan2(xi_p.cos()) * R2D;
        (lat, lon)
    }
}

/// Zone for a longitude, ignoring the Norway and Svalbard exceptions (this
/// survey is nowhere near either).
pub fn utm_zone(lon: f64) -> u8 {
    (((lon + 180.0) / 6.0).floor() as i32).rem_euclid(60) as u8 + 1
}

// ---- Oblique (double) stereographic, for RD --------------------------------

/// EPSG method 9809, the double stereographic used by the Dutch RD grid.
#[derive(Clone, Copy, Debug)]
pub struct ObliqueStereographic {
    pub el: Ellipsoid,
    pub lat0: f64,
    pub lon0: f64,
    pub k0: f64,
    pub fe: f64,
    pub fn_: f64,
}

struct SphereConst {
    r: f64,
    n: f64,
    c: f64,
    chi0: f64,
}

impl ObliqueStereographic {
    /// Amersfoort / RD New, EPSG:28992.
    pub fn rd() -> ObliqueStereographic {
        ObliqueStereographic {
            el: Ellipsoid::BESSEL1841,
            lat0: 52.0 + 9.0 / 60.0 + 22.178 / 3600.0,
            lon0: 5.0 + 23.0 / 60.0 + 15.5 / 3600.0,
            k0: 0.9999079,
            fe: 155000.0,
            fn_: 463000.0,
        }
    }

    fn constants(&self) -> SphereConst {
        let e2 = self.el.e2();
        let e = self.el.e();
        let p0 = self.lat0 * D2R;
        let s2 = p0.sin() * p0.sin();
        let rho0 = self.el.a * (1.0 - e2) / (1.0 - e2 * s2).powf(1.5);
        let nu0 = self.el.a / (1.0 - e2 * s2).sqrt();
        let r = (rho0 * nu0).sqrt();
        let n = (1.0 + (e2 * p0.cos().powi(4)) / (1.0 - e2)).sqrt();
        let s1 = (1.0 + p0.sin()) / (1.0 - p0.sin());
        let s2b = (1.0 - e * p0.sin()) / (1.0 + e * p0.sin());
        let w1 = (s1 * s2b.powf(e)).powf(n);
        let sin_chi0 = (w1 - 1.0) / (w1 + 1.0);
        let c = (n + p0.sin()) * (1.0 - sin_chi0) / ((n - p0.sin()) * (1.0 + sin_chi0));
        let w2 = c * w1;
        let chi0 = ((w2 - 1.0) / (w2 + 1.0)).asin();
        SphereConst { r, n, c, chi0 }
    }

    pub fn forward(&self, lat_deg: f64, lon_deg: f64) -> (f64, f64) {
        let k = self.constants();
        let e = self.el.e();
        let p = lat_deg * D2R;
        let sa = (1.0 + p.sin()) / (1.0 - p.sin());
        let sb = (1.0 - e * p.sin()) / (1.0 + e * p.sin());
        let w = k.c * (sa * sb.powf(e)).powf(k.n);
        let chi = ((w - 1.0) / (w + 1.0)).asin();
        let dlam = k.n * (lon_deg - self.lon0) * D2R;
        let b = 1.0 + chi.sin() * k.chi0.sin() + chi.cos() * k.chi0.cos() * dlam.cos();
        let e_out = self.fe + 2.0 * k.r * self.k0 * chi.cos() * dlam.sin() / b;
        let n_out = self.fn_
            + 2.0 * k.r * self.k0 * (chi.sin() * k.chi0.cos() - chi.cos() * k.chi0.sin() * dlam.cos())
                / b;
        (e_out, n_out)
    }

    pub fn inverse(&self, easting: f64, northing: f64) -> (f64, f64) {
        let k = self.constants();
        let e = self.el.e();
        let de = easting - self.fe;
        let dn = northing - self.fn_;
        let g = 2.0 * k.r * self.k0 * (PI / 4.0 - k.chi0 / 2.0).tan();
        let h = 4.0 * k.r * self.k0 * k.chi0.tan() + g;
        let i = de.atan2(h + dn);
        let j = de.atan2(g - dn) - i;
        let chi = k.chi0 + 2.0 * ((dn - de * (j / 2.0).tan()) / (2.0 * k.r * self.k0)).atan();
        let lam = j + 2.0 * i;
        let lon = self.lon0 + lam * R2D / k.n;
        // isometric latitude, then iterate to geodetic
        let psi = 0.5 * ((1.0 + chi.sin()) / (k.c * (1.0 - chi.sin()))).ln() / k.n;
        let mut p = 2.0 * psi.exp().atan() - PI / 2.0;
        for _ in 0..12 {
            let psi_i = ((p / 2.0 + PI / 4.0).tan()
                * ((1.0 - e * p.sin()) / (1.0 + e * p.sin())).powf(e / 2.0))
            .ln();
            let dp = -(psi_i - psi) * p.cos() * (1.0 - e * e * p.sin() * p.sin()) / (1.0 - e * e);
            p += dp;
            if dp.abs() < 1e-13 {
                break;
            }
        }
        (p * R2D, lon)
    }
}

// ---- Lambert Azimuthal Equal Area, for EPSG:3035 ---------------------------

#[derive(Clone, Copy, Debug)]
pub struct LambertAzimuthalEqualArea {
    pub el: Ellipsoid,
    pub lat0: f64,
    pub lon0: f64,
    pub fe: f64,
    pub fn_: f64,
}

impl LambertAzimuthalEqualArea {
    /// ETRS89-extended / LAEA Europe, EPSG:3035.
    pub fn etrs89_laea() -> LambertAzimuthalEqualArea {
        LambertAzimuthalEqualArea {
            el: Ellipsoid::GRS80,
            lat0: 52.0,
            lon0: 10.0,
            fe: 4321000.0,
            fn_: 3210000.0,
        }
    }

    fn q(&self, sinp: f64) -> f64 {
        let e2 = self.el.e2();
        let e = self.el.e();
        if e2 < 1e-15 {
            return 2.0 * sinp;
        }
        (1.0 - e2)
            * (sinp / (1.0 - e2 * sinp * sinp)
                - (1.0 / (2.0 * e)) * ((1.0 - e * sinp) / (1.0 + e * sinp)).ln())
    }

    pub fn forward(&self, lat_deg: f64, lon_deg: f64) -> (f64, f64) {
        let e2 = self.el.e2();
        let p0 = self.lat0 * D2R;
        let p = lat_deg * D2R;
        let dl = (lon_deg - self.lon0) * D2R;
        let qp = self.q(1.0);
        let q0 = self.q(p0.sin());
        let q = self.q(p.sin());
        let rq = self.el.a * (qp / 2.0).sqrt();
        let beta = (q / qp).clamp(-1.0, 1.0).asin();
        let beta0 = (q0 / qp).clamp(-1.0, 1.0).asin();
        let d = self.el.a * (p0.cos() / (1.0 - e2 * p0.sin() * p0.sin()).sqrt())
            / (rq * beta0.cos());
        let b = rq
            * (2.0
                / (1.0 + beta0.sin() * beta.sin() + beta0.cos() * beta.cos() * dl.cos()))
            .sqrt();
        (
            self.fe + b * d * beta.cos() * dl.sin(),
            self.fn_ + (b / d) * (beta0.cos() * beta.sin() - beta0.sin() * beta.cos() * dl.cos()),
        )
    }

    pub fn inverse(&self, easting: f64, northing: f64) -> (f64, f64) {
        let e2 = self.el.e2();
        let e = self.el.e();
        let p0 = self.lat0 * D2R;
        let qp = self.q(1.0);
        let q0 = self.q(p0.sin());
        let rq = self.el.a * (qp / 2.0).sqrt();
        let beta0 = (q0 / qp).clamp(-1.0, 1.0).asin();
        let d = self.el.a * (p0.cos() / (1.0 - e2 * p0.sin() * p0.sin()).sqrt())
            / (rq * beta0.cos());
        let x = (easting - self.fe) / d;
        let y = (northing - self.fn_) * d;
        let rho = (x * x + y * y).sqrt();
        if rho < 1e-12 {
            return (self.lat0, self.lon0);
        }
        let c = 2.0 * (rho / (2.0 * rq)).clamp(-1.0, 1.0).asin();
        let beta = ((c.cos() * beta0.sin() + y * c.sin() * beta0.cos() / rho).clamp(-1.0, 1.0))
            .asin();
        let lon = self.lon0
            + (x * c.sin())
                .atan2(rho * beta0.cos() * c.cos() - y * beta0.sin() * c.sin())
                * R2D;
        // authalic -> geodetic
        let q = qp * beta.sin();
        let mut p = beta;
        for _ in 0..12 {
            let s = p.sin();
            let num = (1.0 - e2 * s * s).powi(2) / (2.0 * p.cos());
            let dq = q / (1.0 - e2)
                - s / (1.0 - e2 * s * s)
                + (1.0 / (2.0 * e)) * ((1.0 - e * s) / (1.0 + e * s)).ln();
            let dp = num * dq;
            p += dp;
            if dp.abs() < 1e-13 {
                break;
            }
        }
        (p * R2D, lon)
    }
}

// ---- Web Mercator ----------------------------------------------------------

pub const TILE: f64 = 256.0;

/// Web Mercator pixel coordinates at zoom `z`, 256 px tiles.
pub fn lonlat_to_px(lon: f64, lat: f64, z: f64) -> (f64, f64) {
    let n = TILE * 2f64.powf(z);
    let x = (lon + 180.0) / 360.0 * n;
    let s = (lat * D2R).sin().clamp(-0.9999999, 0.9999999);
    let y = (0.5 - ((1.0 + s) / (1.0 - s)).ln() / (4.0 * PI)) * n;
    (x, y)
}

pub fn px_to_lonlat(x: f64, y: f64, z: f64) -> (f64, f64) {
    let n = TILE * 2f64.powf(z);
    let lon = x / n * 360.0 - 180.0;
    let lat = (PI * (1.0 - 2.0 * y / n)).sinh().atan() * R2D;
    (lon, lat)
}

/// Ground resolution in metres per Web Mercator pixel at this latitude.
pub fn mercator_scale(lat: f64, z: f64) -> f64 {
    156543.03392804097 * (lat * D2R).cos() / 2f64.powf(z)
}

// ---- Geodesics on the ellipsoid -------------------------------------------

/// Distance on the local tangent plane, using the ellipsoidal metres-per-degree
/// series. This is the one to use for survey-scale work.
///
/// It is deliberately the exact inverse of `offset_m`, which matters more than
/// it sounds: a point placed by `offset_m` and then measured by a *different*
/// distance model comes back short. A sphere of mean radius understates the
/// radius of the parallel at 52 N by 0.32 %, which is 12 cm across a 38 m
/// swath and three metres across a kilometre of measured line.
pub fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (m_lat, m_lon) = local_scale((lat1 + lat2) / 2.0);
    ((lat2 - lat1) * m_lat).hypot((lon2 - lon1) * m_lon)
}

/// Great-circle distance on a sphere of mean radius. Kept for anything global;
/// for survey distances prefer `distance_m`, which follows the ellipsoid.
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371008.8;
    let (p1, p2) = (lat1 * D2R, lat2 * D2R);
    let dp = p2 - p1;
    let dl = (lon2 - lon1) * D2R;
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

pub fn initial_bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1 * D2R, lat2 * D2R);
    let dl = (lon2 - lon1) * D2R;
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    (y.atan2(x) * R2D).rem_euclid(360.0)
}

/// Metres per degree of latitude and of longitude at this latitude.
///
/// Used everywhere a local tangent plane is good enough, which for a survey a
/// few kilometres across is everywhere.
pub fn local_scale(lat: f64) -> (f64, f64) {
    let p = lat * D2R;
    let m_lat = 111132.92 - 559.82 * (2.0 * p).cos() + 1.175 * (4.0 * p).cos()
        - 0.0023 * (6.0 * p).cos();
    let m_lon = 111412.84 * p.cos() - 93.5 * (3.0 * p).cos() + 0.118 * (5.0 * p).cos();
    (m_lat, m_lon)
}

/// Move `dist` metres on bearing `brg` from a point, on the local tangent plane.
pub fn offset_m(lat: f64, lon: f64, brg_deg: f64, dist: f64) -> (f64, f64) {
    let (m_lat, m_lon) = local_scale(lat);
    let b = brg_deg * D2R;
    (lat + dist * b.cos() / m_lat, lon + dist * b.sin() / m_lon)
}
