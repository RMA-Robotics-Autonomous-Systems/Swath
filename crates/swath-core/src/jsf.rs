//! EdgeTech JSF (JSTAR) reader.
//!
//! A direct port of `tools/jsf.py`. The 0x0080 sonar-data header layout was
//! verified empirically against this dataset, so the offsets below are copied
//! across literally rather than re-derived -- the Python is the oracle and the
//! differential tests in `tests/parity.rs` hold the two together.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

pub const MARKER: u16 = 0x1601;
pub const MSG_HDR_SIZE: usize = 16;
/// The sonar-data header that precedes the trace samples.
pub const SONAR_HDR_SIZE: usize = 240;

/// Message types seen in EdgeTech recordings.
pub fn msg_type_name(t: u16) -> &'static str {
    match t {
        40 => "raw serial",
        80 => "sonar data",
        82 => "sonar data (compressed)",
        86 => "sonar data 2",
        181 => "system information (old)",
        182 => "system information",
        426 => "file timestamp",
        428 => "file padding",
        2002 => "NMEA string",
        2020 => "pressure sensor",
        2040 => "miscellaneous analog",
        2060 => "pipe echo detect",
        2080 => "container timestamp",
        2101 => "situation",
        2111 => "cable counter",
        _ => "unknown",
    }
}

pub fn subsystem_name(s: u8) -> &'static str {
    match s {
        0 => "sub-bottom",
        20 => "sidescan (set 1)",
        21 => "sidescan (set 2)",
        100 => "NMEA/nav",
        101 => "pressure",
        102 => "analog",
        _ => "unknown",
    }
}

/// Data format code -> (bytes per value, values per sample, signed).
///
/// Mirrors `DATA_FORMATS` in the Python: anything unrecognised falls back to
/// format 0, one signed 16-bit envelope value per sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataFormat {
    pub signed: bool,
    pub per_sample: usize,
}

impl DataFormat {
    pub fn from_code(code: i16) -> DataFormat {
        match code {
            1 => DataFormat { signed: true, per_sample: 2 }, // analytic I/Q
            9 => DataFormat { signed: false, per_sample: 1 }, // envelope u16
            0 | 2 | 3 | 4 => DataFormat { signed: true, per_sample: 1 },
            _ => DataFormat { signed: true, per_sample: 1 }, // unknown -> format 0
        }
    }
    /// Bytes occupied by `nsamples` samples.
    pub fn nbytes(&self, nsamples: usize) -> usize {
        nsamples * 2 * self.per_sample
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MessageHeader {
    pub offset: u64,
    pub version: u8,
    pub session: u8,
    pub mtype: u16,
    pub command: u8,
    pub subsystem: u8,
    pub channel: u8,
    pub sequence: u8,
    pub size: u32,
}

// ---- little-endian field readers -------------------------------------------
// Bounds-checked so a truncated or corrupt message yields None rather than a
// panic; the file is memory-mapped and its length is not something we control.

#[inline]
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
#[inline]
fn i16le(b: &[u8], o: usize) -> Option<i16> {
    u16le(b, o).map(|v| v as i16)
}
#[inline]
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
#[inline]
fn i32le(b: &[u8], o: usize) -> Option<i32> {
    u32le(b, o).map(|v| v as i32)
}
#[inline]
fn f32le(b: &[u8], o: usize) -> Option<f32> {
    u32le(b, o).map(f32::from_bits)
}

pub fn parse_message_header(buf: &[u8], offset: u64) -> Option<MessageHeader> {
    if buf.len() < MSG_HDR_SIZE {
        return None;
    }
    Some(MessageHeader {
        offset,
        version: buf[2],
        session: buf[3],
        mtype: u16le(buf, 4)?,
        command: buf[6],
        subsystem: buf[7],
        channel: buf[8],
        sequence: buf[9],
        size: u32le(buf, 12)?,
    })
}

/// JSF coordinate -> decimal degrees. `units == 2` means 1/10000 arc-minute.
///
/// Units 1 is millimetres in a projected grid, which we cannot resolve without
/// knowing the grid, so it comes back NaN exactly as the Python does.
#[inline]
pub fn coord_to_degrees(raw: i32, units: i16) -> f64 {
    match units {
        2 => raw as f64 / 10000.0 / 60.0,
        3 => raw as f64 / 1e6,
        _ => f64::NAN,
    }
}

// Validity bitmap, JSF bytes 30-31 of the type-80 body.
pub const V_POSITION: u16 = 1 << 0;
pub const V_COURSE: u16 = 1 << 1;
pub const V_SPEED: u16 = 1 << 2;
pub const V_HEADING: u16 = 1 << 3;
pub const V_PRESSURE: u16 = 1 << 4;
pub const V_PITCHROLL: u16 = 1 << 5;
pub const V_ALTITUDE: u16 = 1 << 6;
pub const V_DEPTH: u16 = 1 << 9;
pub const V_POS_INTERPOLATED: u16 = 1 << 13;

/// One type-0x0080 sonar-data message: header fields plus the trace samples.
#[derive(Clone, Debug)]
pub struct Ping {
    pub offset: u64,
    pub subsystem: u8,
    pub channel: u8,
    /// Seconds since the Unix epoch, whole seconds from body offset 0 plus the
    /// sub-second part of the milliseconds-today field at offset 200.
    ///
    /// The sonar pings at ~14 Hz and the position updates at 1 Hz. Without the
    /// milliseconds every ping in a second shares one timestamp, lands on one
    /// fix, and the two and a half metres of seabed between consecutive fixes
    /// is never drawn at all.
    pub time: f64,
    pub unix_time: i32,
    pub millis_today: u32,
    /// Broken-down UTC from the ping's own y/doy/h/m/s fields, which is a
    /// different clock reading from `time` and occasionally disagrees with it.
    pub ymd_hms: Option<(i32, u32, u32, u32, u32, u32)>,
    pub ping_number: u32,
    pub nsamples: u16,
    pub start_depth: u32,
    pub sample_interval_ns: u32,
    pub weighting_factor: i16,
    pub longitude: f64,
    pub latitude: f64,
    pub coord_units: i16,
    pub heading: f64,
    pub pitch: f64,
    pub roll: f64,
    pub depth_m: f64,
    pub altitude_m: f64,
    pub pressure_psi: f64,
    pub start_freq_hz: f64,
    pub end_freq_hz: f64,
    pub mixer_freq_hz: f32,
    pub sound_speed: f32,
    pub course: f64,
    pub speed: f64,
    pub gain: u16,
    pub max_adc: u16,
    pub data_format: i16,
    pub validity: u16,
    pub software_version: String,
    /// Trace amplitudes, weighting factor already applied.
    pub data: Vec<f32>,
}

impl Ping {
    /// The sonar's own bottom track. The spec says a zero altitude means "not
    /// filled", and bit 6 of the validity flag confirms it.
    pub fn altitude_valid(&self) -> bool {
        (self.validity & V_ALTITUDE) != 0 && self.altitude_m > 0.0
    }

    pub fn sample_rate_hz(&self) -> f64 {
        if self.sample_interval_ns == 0 {
            f64::NAN
        } else {
            1e9 / self.sample_interval_ns as f64
        }
    }

    /// Across-track slant range covered by the trace, using the recorded sound
    /// speed when present and 1500 m/s otherwise.
    pub fn slant_range_m(&self) -> f64 {
        let c = if self.sound_speed > 0.0 { self.sound_speed as f64 } else { 1500.0 };
        self.nsamples as f64 * self.sample_interval_ns as f64 * 1e-9 * c / 2.0
    }

    /// Metres of ground range per sample, on the flat-seabed approximation.
    pub fn sample_resolution_m(&self) -> f64 {
        let c = if self.sound_speed > 0.0 { self.sound_speed as f64 } else { 1500.0 };
        self.sample_interval_ns as f64 * 1e-9 * c / 2.0
    }
}

/// Decode trace samples with the weighting factor applied.
fn decode_samples(raw: &[u8], fmt: DataFormat, weighting: i16, nsamples: usize) -> Vec<f32> {
    let scale = (2.0f32).powi(-(weighting as i32));
    let mut out = Vec::with_capacity(nsamples);
    if fmt.per_sample == 2 {
        // analytic: magnitude of the I/Q pair
        for i in 0..nsamples {
            let o = i * 4;
            let (Some(a), Some(b)) = (i16le(raw, o), i16le(raw, o + 2)) else { break };
            out.push((a as f32).hypot(b as f32) * scale);
        }
    } else if fmt.signed {
        for i in 0..nsamples {
            let Some(v) = i16le(raw, i * 2) else { break };
            out.push(v as f32 * scale);
        }
    } else {
        for i in 0..nsamples {
            let Some(v) = u16le(raw, i * 2) else { break };
            out.push(v as f32 * scale);
        }
    }
    out
}

/// Parse a type-80 body. Field offsets are those verified against this dataset.
pub fn parse_sonar_message(hdr: &MessageHeader, body: &[u8]) -> Option<Ping> {
    if body.len() < SONAR_HDR_SIZE {
        return None;
    }
    let coord_units = i16le(body, 88)?;
    let unix_time = i32le(body, 0)?;
    let millis = if body.len() >= 204 { u32le(body, 200)? } else { 0 };
    let nsamples = u16le(body, 114)?;
    let data_format = i16le(body, 34)?;
    let fmt = DataFormat::from_code(data_format);
    let weighting_factor = i16le(body, 168)?;

    let (year, doy) = (i16le(body, 156)? as i32, i16le(body, 158)? as i32);
    let (hh, mm, ss) = (i16le(body, 160)?, i16le(body, 162)?, i16le(body, 164)?);
    let ymd_hms = if (1970..=2100).contains(&year)
        && (1..=366).contains(&doy)
        && (0..24).contains(&hh)
        && (0..60).contains(&mm)
        && (0..62).contains(&ss)
    {
        let (m, d) = crate::time::month_day(year, doy as u32)?;
        Some((year, m, d, hh as u32, mm as u32, ss as u32))
    } else {
        None
    };

    let nbytes = fmt.nbytes(nsamples as usize);
    let raw = body.get(SONAR_HDR_SIZE..(SONAR_HDR_SIZE + nbytes).min(body.len()))?;
    let data = decode_samples(raw, fmt, weighting_factor, nsamples as usize);

    let sw = body.get(210..216).unwrap_or(&[]);
    let end = sw.iter().position(|&c| c == 0).unwrap_or(sw.len());
    let software_version = String::from_utf8_lossy(&sw[..end]).into_owned();

    Some(Ping {
        offset: hdr.offset,
        subsystem: hdr.subsystem,
        channel: hdr.channel,
        time: unix_time as f64 + (millis % 1000) as f64 / 1000.0,
        unix_time,
        millis_today: millis,
        ymd_hms,
        ping_number: u32le(body, 8)?,
        nsamples,
        start_depth: u32le(body, 4)?,
        sample_interval_ns: u32le(body, 116)?,
        weighting_factor,
        longitude: coord_to_degrees(i32le(body, 80)?, coord_units),
        latitude: coord_to_degrees(i32le(body, 84)?, coord_units),
        coord_units,
        heading: u16le(body, 172)? as f64 / 100.0,
        pitch: i16le(body, 174)? as f64 * 180.0 / 32768.0,
        roll: i16le(body, 176)? as f64 * 180.0 / 32768.0,
        depth_m: i32le(body, 136)? as f64 / 1000.0,
        altitude_m: i32le(body, 144)? as f64 / 1000.0,
        pressure_psi: i32le(body, 132)? as f64 / 1000.0,
        start_freq_hz: u16le(body, 126)? as f64 * 10.0,
        end_freq_hz: u16le(body, 128)? as f64 * 10.0,
        mixer_freq_hz: f32le(body, 152)?,
        sound_speed: f32le(body, 148)?,
        course: i16le(body, 192)? as f64,
        speed: i16le(body, 194)? as f64 / 10.0,
        gain: u16le(body, 120)?,
        max_adc: u16le(body, 204)?,
        data_format,
        validity: u16le(body, 30)?,
        software_version,
        data,
    })
}

/// A memory-mapped JSF file. Scanning is a walk over the message headers; the
/// bodies of filtered-out messages are never touched.
pub struct JsfFile {
    pub path: PathBuf,
    map: Mmap,
}

impl JsfFile {
    pub fn open(path: impl AsRef<Path>) -> io::Result<JsfFile> {
        let path = path.as_ref().to_path_buf();
        let f = File::open(&path)?;
        // SAFETY: the survey files are read-only inputs; a concurrent truncation
        // would be a torn read, which is the same exposure the Python has.
        let map = unsafe { Mmap::map(&f)? };
        Ok(JsfFile { path, map })
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn bytes(&self) -> &[u8] {
        &self.map
    }

    /// Walk every message header in the file, calling `f` with the header and
    /// the body slice. Stops at the first lost sync or truncated message, which
    /// is what the Python reader does.
    pub fn walk<F: FnMut(&MessageHeader, &[u8]) -> bool>(&self, mut f: F) -> Result<(), SyncError> {
        let b: &[u8] = &self.map;
        let mut off = 0usize;
        while off + MSG_HDR_SIZE <= b.len() {
            let hdr = match parse_message_header(&b[off..], off as u64) {
                Some(h) => h,
                None => break,
            };
            let marker = u16le(b, off).unwrap_or(0);
            if marker != MARKER {
                return Err(SyncError { offset: off as u64, marker });
            }
            let start = off + MSG_HDR_SIZE;
            let end = start + hdr.size as usize;
            if end > b.len() {
                break; // truncated final message
            }
            if !f(&hdr, &b[start..end]) {
                return Ok(());
            }
            off = end;
        }
        Ok(())
    }

    /// Parse the single message that starts at `offset`.
    pub fn message_at(&self, offset: u64) -> Option<(MessageHeader, &[u8])> {
        let b: &[u8] = &self.map;
        let off = offset as usize;
        if off + MSG_HDR_SIZE > b.len() {
            return None;
        }
        if u16le(b, off)? != MARKER {
            return None;
        }
        let hdr = parse_message_header(&b[off..], offset)?;
        let start = off + MSG_HDR_SIZE;
        let end = (start + hdr.size as usize).min(b.len());
        Some((hdr, &b[start..end]))
    }

    /// Parse the sonar ping at `offset`, or None if that is not a type-80.
    pub fn ping_at(&self, offset: u64) -> Option<Ping> {
        let (hdr, body) = self.message_at(offset)?;
        if hdr.mtype != 80 {
            return None;
        }
        parse_sonar_message(&hdr, body)
    }

    /// Every type-2002 NMEA sentence, as (unix seconds, sentence).
    pub fn nmea(&self) -> Vec<(i32, String)> {
        let mut out = Vec::new();
        let _ = self.walk(|hdr, body| {
            if hdr.mtype == 2002 && body.len() > 12 {
                if let Some(t) = i32le(body, 0) {
                    let rest = &body[12..];
                    let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
                    out.push((t, String::from_utf8_lossy(&rest[..end]).trim().to_string()));
                }
            }
            true
        });
        out
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyncError {
    pub offset: u64,
    pub marker: u16,
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lost sync at offset {}: marker=0x{:04x}", self.offset, self.marker)
    }
}
impl std::error::Error for SyncError {}
