//! The ping index: one record per (ping, subsystem, channel) with the byte
//! offset of its message, so any ping can be reached without rescanning.
//!
//! This is the spine of the whole application. The Python writes the same
//! record set to a compressed `.npz`; here it is a flat little-endian file with
//! a JSON header, which memory-maps directly into `&[PingRecord]`.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::jsf::{self, JsfFile};

pub const MAGIC: &[u8; 8] = b"SWTIDX01";

/// The spelling this file used before the application was renamed.
///
/// Only ever read. Everything under `out/` is derived and could in principle
/// be rebuilt, but rebuilding it means re-reading every recording, so a rename
/// is not a good enough reason to invalidate a workspace. Written files carry
/// `SWTIDX01`; both are accepted on the way in.
pub const MAGIC_LEGACY: &[u8; 8] = b"WPAIDX01";

/// One indexed ping. `#[repr(C)]` with explicit padding so the on-disk layout
/// is the in-memory layout and the file can be mapped rather than parsed.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[repr(C)]
pub struct PingRecord {
    pub time: f64,
    pub lat: f64,
    pub lon: f64,
    pub offset: u64,
    pub ping: u32,
    pub interval_ns: u32,
    pub heading: f32,
    pub depth: f32,
    pub altitude: f32,
    pub pitch: f32,
    pub roll: f32,
    pub f0: f32,
    pub f1: f32,
    pub nsamples: u16,
    pub validity: u16,
    pub file_id: u16,
    pub subsystem: u8,
    pub channel: u8,
    pub weight: i8,
    pub fmt: i8,
    pub _pad: [u8; 6],
}

const REC_SIZE: usize = std::mem::size_of::<PingRecord>();

impl PingRecord {
    /// Slant range in metres covered by this ping's trace, at `c` m/s.
    pub fn slant_range_m(&self, c: f64) -> f64 {
        self.nsamples as f64 * self.interval_ns as f64 * 1e-9 * c / 2.0
    }
    /// Metres of slant range per sample, at `c` m/s.
    ///
    /// The sound speed is a parameter and not a constant because it is a
    /// property of the water on the day, and getting it wrong scales every
    /// across-track distance. It is passed rather than stored so that no
    /// caller can convert a time into a distance without having said which
    /// water it was in.
    pub fn resolution_m(&self, c: f64) -> f64 {
        self.interval_ns as f64 * 1e-9 * c / 2.0
    }
    /// Metres per sample at the speed Discover used.
    ///
    /// Only for reading back the metre-valued fields Discover itself wrote --
    /// chiefly the bottom-tracked altitude, whose sample index is what the
    /// detector actually wants. Never for placing imagery.
    pub fn recorded_resolution_m(&self) -> f64 {
        self.interval_ns as f64 * 1e-9 * crate::C_RECORDED / 2.0
    }
    /// The recorded altitude as a trace sample index, or None when the sonar
    /// did not fill it in.
    pub fn altitude_samples(&self) -> Option<f32> {
        let res = self.recorded_resolution_m();
        (self.altitude_valid() && res > 0.0).then(|| self.altitude as f64 / res).map(|v| v as f32)
    }
    pub fn altitude_valid(&self) -> bool {
        (self.validity & jsf::V_ALTITUDE) != 0 && self.altitude > 0.0
    }

    /// Build a record from an XTF ping.
    ///
    /// XTF has no validity bitmap, so the flags the rest of the code reads are
    /// synthesised from whether the field carries a plausible value at all --
    /// which is the same question the bitmap answers, asked of the data.
    pub fn from_xtf(p: &crate::xtf::XtfPing, file_id: u16) -> PingRecord {
        let mut validity = 0u16;
        if p.altitude_m > 0.0 {
            validity |= jsf::V_ALTITUDE;
        }
        if p.lat.is_finite() && p.lon.is_finite() {
            validity |= jsf::V_POSITION;
        }
        if p.heading.is_finite() {
            validity |= jsf::V_HEADING;
        }
        let interval_ns = if p.nsamples > 0 && p.slant_range_m > 0.0 {
            (p.slant_range_m / p.nsamples as f64 * 2.0 / crate::C_RECORDED * 1e9) as u32
        } else {
            0
        };
        PingRecord {
            time: p.time,
            lat: p.lat,
            lon: p.lon,
            offset: p.offset,
            ping: p.ping_number,
            interval_ns,
            heading: p.heading as f32,
            depth: p.depth_m as f32,
            altitude: p.altitude_m as f32,
            pitch: p.pitch as f32,
            roll: p.roll as f32,
            f0: 0.0,
            f1: 0.0,
            nsamples: p.nsamples.min(u16::MAX as usize) as u16,
            validity,
            file_id,
            // XTF numbers its channels 0..n across both sides; the low bit is
            // the side and the rest selects the frequency set, which is the
            // same split JSF spells out as subsystem and channel.
            subsystem: 20 + (p.channel >> 1),
            channel: p.channel & 1,
            weight: 0,
            fmt: 0,
            _pad: [0; 6],
        }
    }
}

/// One subsystem and the chirp it transmits.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Band {
    pub subsystem: u8,
    /// Start and end of the sweep as recorded, Hz. Zero when the format does
    /// not carry them. These wrap: see `recover_band_centres`.
    pub f0: f32,
    pub f1: f32,
    /// Centre frequency, Hz, once the wrap has been undone. Zero when nothing
    /// corroborated it -- and then it is better to say nothing than to print a
    /// number that is wrong by a megahertz.
    #[serde(default)]
    pub centre_hz: f64,
    pub pings: usize,
}

impl Band {
    /// Width of the sweep, Hz. Survives the wrap: both ends are displaced by
    /// the same multiple, so the difference between them is untouched.
    pub fn bandwidth_hz(&self) -> f64 {
        (self.f1 - self.f0).abs() as f64
    }

    /// The centre as recorded, which is the true centre modulo the wrap.
    pub fn recorded_centre_hz(&self) -> f64 {
        (self.f0 as f64 + self.f1 as f64) / 2.0
    }
}

/// The JSF sweep field is a u16 in units of 10 Hz, so it cannot express
/// anything above 655.35 kHz. A 1550 kHz channel is written as 239.28 kHz --
/// wrapped twice -- and nothing in the ping says by how much.
pub const FREQ_WRAP_HZ: f64 = 65_536.0 * 10.0;

/// Restore the high bits of each band's centre frequency from frequencies
/// observed elsewhere.
///
/// `observed` are centre frequencies from a source that can hold them, in
/// practice the XTF the same acquisition wrote beside the JSF, whose channel
/// descriptors carry it as a float. A candidate is accepted only when it
/// agrees with what the ping recorded *modulo the wrap*: that is the same
/// number with its high bits back, and it identifies which band it belongs to
/// at the same time, which matters because the two are in separate files with
/// no shared channel numbering. Anything that does not agree came from some
/// other recording and is ignored.
pub fn recover_band_centres(bands: &mut [Band], observed: &[f64]) {
    // Far tighter than the 655 kHz wrap, loose enough for a writer that rounds
    // its own label to the nearest few kilohertz.
    const TOL_HZ: f64 = 25_000.0;
    for b in bands.iter_mut() {
        let recorded = b.recorded_centre_hz();
        if recorded <= 0.0 {
            continue;
        }
        b.centre_hz = recorded;
        for &o in observed {
            if o < recorded - TOL_HZ {
                continue; // the truth cannot be below what was recorded
            }
            let k = ((o - recorded) / FREQ_WRAP_HZ).round();
            if (o - (recorded + k * FREQ_WRAP_HZ)).abs() <= TOL_HZ {
                b.centre_hz = recorded + k * FREQ_WRAP_HZ;
                break;
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexHeader {
    pub version: u32,
    pub files: Vec<String>,
    pub count: usize,
    /// Set when a file stopped early, so the UI can say so rather than silently
    /// showing a short survey.
    #[serde(default)]
    pub warnings: Vec<String>,
}

pub struct PingIndex {
    pub header: IndexHeader,
    pub records: Vec<PingRecord>,
    pub path: Option<PathBuf>,
}

/// Read the fields the index keeps straight out of a type-80 body, without
/// decoding the trace. Mirrors `index_file` in `tools/build_index.py`.
fn index_record(hdr: &jsf::MessageHeader, body: &[u8], file_id: u16) -> Option<PingRecord> {
    if body.len() < jsf::SONAR_HDR_SIZE {
        return None;
    }
    let g16 = |o: usize| -> i16 { i16::from_le_bytes([body[o], body[o + 1]]) };
    let gu16 = |o: usize| -> u16 { u16::from_le_bytes([body[o], body[o + 1]]) };
    let g32 = |o: usize| -> i32 {
        i32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]])
    };
    let gu32 = |o: usize| -> u32 {
        u32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]])
    };

    let cu = g16(88);
    // 1/10000 arc-minute when the unit code is 2, micro-degrees when it is 3.
    let scale = if cu == 2 { 1e4 * 60.0 } else { 1e6 };
    // Whole seconds at offset 0, milliseconds-today at 200. They are the same
    // clock, so only the sub-second part of the second field is wanted. Without
    // it the ~14 pings in each second all index as one instant, every one of
    // them is placed at the same fix, and the mosaic comes out a wireframe with
    // 2.5 m of unpainted seabed between the strokes.
    let millis = if body.len() >= 204 { gu32(200) } else { 0 };
    Some(PingRecord {
        time: g32(0) as f64 + (millis % 1000) as f64 / 1000.0,
        lat: g32(84) as f64 / scale,
        lon: g32(80) as f64 / scale,
        offset: hdr.offset,
        ping: gu32(8),
        interval_ns: gu32(116),
        heading: gu16(172) as f32 / 100.0,
        depth: g32(136) as f32 / 1000.0,
        altitude: g32(144) as f32 / 1000.0,
        pitch: g16(174) as f32 * 180.0 / 32768.0,
        roll: g16(176) as f32 * 180.0 / 32768.0,
        f0: gu16(126) as f32 * 10.0,
        f1: gu16(128) as f32 * 10.0,
        nsamples: gu16(114),
        validity: gu16(30),
        file_id,
        subsystem: hdr.subsystem,
        channel: hdr.channel,
        weight: g16(168) as i8,
        fmt: g16(34) as i8,
        _pad: [0; 6],
    })
}

impl PingIndex {
    /// Scan a set of JSF files and build the index. Files are read in parallel;
    /// the result is sorted by time, stably, so channels interleave the way the
    /// recording did.
    pub fn build(paths: &[PathBuf]) -> io::Result<PingIndex> {
        let parts: Vec<(Vec<PingRecord>, Option<String>)> = paths
            .par_iter()
            .enumerate()
            .map(|(i, p)| {
                let mut recs = Vec::new();
                let mut warn = None;
                if crate::project::has_ext(p, "xtf") {
                    match crate::xtf::XtfFile::open(p) {
                        Ok(xf) => xf.pings(|ping| {
                            recs.push(PingRecord::from_xtf(&ping, i as u16));
                            true
                        }),
                        Err(e) => warn = Some(format!("{}: {}", p.display(), e)),
                    }
                    return (recs, warn);
                }
                match JsfFile::open(p) {
                    Ok(jf) => {
                        let r = jf.walk(|hdr, body| {
                            if hdr.mtype == 80 {
                                if let Some(rec) = index_record(hdr, body, i as u16) {
                                    recs.push(rec);
                                }
                            }
                            true
                        });
                        if let Err(e) = r {
                            warn = Some(format!("{}: {}", p.display(), e));
                        }
                    }
                    Err(e) => warn = Some(format!("{}: {}", p.display(), e)),
                }
                (recs, warn)
            })
            .collect();

        let mut records = Vec::with_capacity(parts.iter().map(|(r, _)| r.len()).sum());
        let mut warnings = Vec::new();
        for (r, w) in parts {
            records.extend(r);
            if let Some(w) = w {
                warnings.push(w);
            }
        }
        records.sort_by(|a, b| a.time.total_cmp(&b.time));

        Ok(PingIndex {
            header: IndexHeader {
                version: 1,
                files: paths.iter().map(|p| p.display().to_string()).collect(),
                count: records.len(),
                warnings,
            },
            records,
            path: None,
        })
    }

    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let mut f = File::create(path.as_ref())?;
        let hdr = serde_json::to_vec(&self.header)?;
        f.write_all(MAGIC)?;
        f.write_all(&(hdr.len() as u32).to_le_bytes())?;
        f.write_all(&hdr)?;
        // SAFETY: PingRecord is repr(C), Copy, and contains no padding beyond
        // the explicit `_pad`, so its byte image is well defined.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                self.records.as_ptr() as *const u8,
                self.records.len() * REC_SIZE,
            )
        };
        f.write_all(bytes)?;
        Ok(())
    }

    pub fn load(path: impl AsRef<Path>) -> io::Result<PingIndex> {
        let path = path.as_ref().to_path_buf();
        let mut f = File::open(&path)?;
        let mut magic = [0u8; 8];
        f.read_exact(&mut magic)?;
        if &magic != MAGIC && &magic != MAGIC_LEGACY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a swath ping index"));
        }
        let mut n = [0u8; 4];
        f.read_exact(&mut n)?;
        let mut hdr = vec![0u8; u32::from_le_bytes(n) as usize];
        f.read_exact(&mut hdr)?;
        let header: IndexHeader = serde_json::from_slice(&hdr)?;
        let mut rest = Vec::new();
        f.read_to_end(&mut rest)?;
        if rest.len() % REC_SIZE != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated index"));
        }
        let count = rest.len() / REC_SIZE;
        let mut records = vec![PingRecord::default(); count];
        // SAFETY: same repr(C) layout as `save` wrote, length checked above.
        unsafe {
            std::ptr::copy_nonoverlapping(
                rest.as_ptr(),
                records.as_mut_ptr() as *mut u8,
                rest.len(),
            );
        }
        Ok(PingIndex { header, records, path: Some(path) })
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Distinct (subsystem, channel) pairs present, in ascending order.
    pub fn channels(&self) -> Vec<(u8, u8)> {
        let mut v: Vec<(u8, u8)> =
            self.records.iter().map(|r| (r.subsystem, r.channel)).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn subsystems(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self.records.iter().map(|r| r.subsystem).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The frequency bands present, one per subsystem.
    ///
    /// A subsystem is a set of transducers, and what an operator actually calls
    /// it is its frequency -- "the high frequency channel", not "subsystem 20".
    /// The sweep is in the ping header and has always been indexed; it just had
    /// nowhere to go. XTF records none, so a zero here means "unknown" and the
    /// caller falls back to the number.
    pub fn bands(&self) -> Vec<Band> {
        self.subsystems()
            .into_iter()
            .map(|s| {
                // Median rather than first: one corrupt header should not name
                // the channel for the whole recording.
                let mut f: Vec<(f32, f32)> = self
                    .records
                    .iter()
                    .filter(|r| r.subsystem == s && r.f0 > 0.0)
                    .map(|r| (r.f0, r.f1))
                    .collect();
                f.sort_by(|a, b| a.0.total_cmp(&b.0));
                let (f0, f1) = f.get(f.len() / 2).copied().unwrap_or((0.0, 0.0));
                Band { subsystem: s, f0, f1, centre_hz: 0.0, pings: self.count_of(s) }
            })
            .collect()
    }

    fn count_of(&self, subsystem: u8) -> usize {
        self.records.iter().filter(|r| r.subsystem == subsystem).count()
    }

    /// Row positions of every ping on one channel, in time order.
    pub fn select(&self, subsystem: u8, channel: Option<u8>) -> Vec<u32> {
        self.records
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.subsystem == subsystem && channel.map_or(true, |c| r.channel == c)
            })
            .map(|(i, _)| i as u32)
            .collect()
    }

    pub fn bounds(&self) -> Option<Bounds> {
        let mut b = Bounds::EMPTY;
        for r in &self.records {
            if r.lat.is_finite() && r.lon.is_finite() && (r.lat != 0.0 || r.lon != 0.0) {
                b.extend(r.lat, r.lon);
            }
        }
        if b.is_empty() {
            None
        } else {
            Some(b)
        }
    }

    pub fn time_range(&self) -> Option<(f64, f64)> {
        if self.records.is_empty() {
            return None;
        }
        Some((self.records[0].time, self.records[self.records.len() - 1].time))
    }

    /// Open the source file that record `i` came from.
    pub fn file_of(&self, rec: &PingRecord) -> io::Result<JsfFile> {
        let p = self
            .header
            .files
            .get(rec.file_id as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad file_id"))?;
        JsfFile::open(p)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Bounds {
    pub min_lat: f64,
    pub min_lon: f64,
    pub max_lat: f64,
    pub max_lon: f64,
}

impl Bounds {
    pub const EMPTY: Bounds = Bounds {
        min_lat: f64::INFINITY,
        min_lon: f64::INFINITY,
        max_lat: f64::NEG_INFINITY,
        max_lon: f64::NEG_INFINITY,
    };
    pub fn is_empty(&self) -> bool {
        self.min_lat > self.max_lat
    }
    pub fn extend(&mut self, lat: f64, lon: f64) {
        self.min_lat = self.min_lat.min(lat);
        self.max_lat = self.max_lat.max(lat);
        self.min_lon = self.min_lon.min(lon);
        self.max_lon = self.max_lon.max(lon);
    }
    pub fn union(&self, o: &Bounds) -> Bounds {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Bounds {
            min_lat: self.min_lat.min(o.min_lat),
            min_lon: self.min_lon.min(o.min_lon),
            max_lat: self.max_lat.max(o.max_lat),
            max_lon: self.max_lon.max(o.max_lon),
        }
    }
    pub fn pad_m(&self, metres: f64) -> Bounds {
        let (m_lat, m_lon) = crate::geo::local_scale((self.min_lat + self.max_lat) / 2.0);
        Bounds {
            min_lat: self.min_lat - metres / m_lat,
            max_lat: self.max_lat + metres / m_lat,
            min_lon: self.min_lon - metres / m_lon,
            max_lon: self.max_lon + metres / m_lon,
        }
    }
    pub fn centre(&self) -> (f64, f64) {
        ((self.min_lat + self.max_lat) / 2.0, (self.min_lon + self.max_lon) / 2.0)
    }
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }
}
