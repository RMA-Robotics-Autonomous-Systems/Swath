//! Triton XTF reader, enough of it to index and display a sidescan recording.
//!
//! XTF is the other format this survey turns up in. The file header is a fixed
//! 1024 bytes followed by one 256-byte descriptor per channel; packets after
//! that are self-delimiting via a magic number and a byte count.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

pub const MAGIC: u16 = 0xFACE;
pub const FILE_HEADER_SIZE: usize = 1024;
/// The channel descriptors live *inside* the file header: 256 bytes of fields
/// followed by six 128-byte `ChanInfo` slots, 1024 bytes in all. A writer with
/// more than six channels extends the header by one slot each.
pub const CHAN_INFO_OFFSET: usize = 256;
pub const CHAN_INFO_SIZE: usize = 128;
pub const CHAN_INFO_SLOTS: usize = 6;

/// Packet header types.
pub const HDR_SONAR: u8 = 0;
pub const HDR_NOTES: u8 = 1;
pub const HDR_BATHY: u8 = 2;
pub const HDR_ATTITUDE: u8 = 3;

#[inline]
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
#[inline]
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
#[inline]
fn f32le(b: &[u8], o: usize) -> Option<f32> {
    u32le(b, o).map(f32::from_bits)
}
#[inline]
fn f64le(b: &[u8], o: usize) -> Option<f64> {
    b.get(o..o + 8).map(|s| {
        f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
    })
}

#[derive(Clone, Debug)]
pub struct XtfHeader {
    pub recording_program: String,
    pub sonar_name: String,
    pub sonar_type: u16,
    pub n_channels: usize,
    /// Bytes per sample for each channel, from the channel descriptors.
    pub bytes_per_sample: Vec<u8>,
    pub channel_names: Vec<String>,
    /// Centre frequency per channel, Hz. Zero when the writer left it blank.
    ///
    /// Worth having because the JSF written by the same acquisition cannot
    /// hold it: see `index::recover_band_centres`.
    pub frequency_hz: Vec<f64>,
    pub nav_offset_y: f32,
    pub nav_offset_x: f32,
}

/// One sidescan ping from an XTF packet: both channels arrive in the same
/// packet, each with its own trailing header.
#[derive(Clone, Debug)]
pub struct XtfPing {
    pub offset: u64,
    pub channel: u8,
    pub time: f64,
    pub ping_number: u32,
    pub lat: f64,
    pub lon: f64,
    pub heading: f64,
    pub pitch: f64,
    pub roll: f64,
    pub depth_m: f64,
    pub altitude_m: f64,
    pub slant_range_m: f64,
    pub nsamples: usize,
    pub data: Vec<f32>,
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

pub struct XtfFile {
    pub path: PathBuf,
    pub header: XtfHeader,
    map: Mmap,
}

impl XtfFile {
    pub fn open(path: impl AsRef<Path>) -> io::Result<XtfFile> {
        let path = path.as_ref().to_path_buf();
        let f = File::open(&path)?;
        // SAFETY: read-only survey input, same exposure as the Python reader.
        let map = unsafe { Mmap::map(&f)? };
        if map.len() < FILE_HEADER_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short XTF header"));
        }
        let b: &[u8] = &map;
        // NumberOfSonarChannels and NumberOfBathymetryChannels. These were read
        // from 82/84, which is inside NoteString and is zero in every file
        // here -- so the channel table came back empty, the sample width fell
        // back to its default, and the packet walk started a quarter of a
        // kilobyte into the first ping and resynchronised past it.
        let n_channels =
            u16le(b, 166).unwrap_or(0) as usize + u16le(b, 168).unwrap_or(0) as usize;
        let mut bytes_per_sample = Vec::new();
        let mut channel_names = Vec::new();
        let mut frequency_hz = Vec::new();
        for i in 0..n_channels.min(16) {
            let o = CHAN_INFO_OFFSET + i * CHAN_INFO_SIZE;
            if o + CHAN_INFO_SIZE > b.len() {
                break;
            }
            bytes_per_sample.push(u16le(b, o + 6).unwrap_or(2).clamp(1, 8) as u8);
            channel_names.push(cstr(&b[o + 12..o + 28]));
            // Written in kHz by every writer seen here, but the field is not
            // documented with a unit, so read a large value as Hz rather than
            // reporting a gigahertz sonar.
            let f = f32le(b, o + 32).unwrap_or(0.0) as f64;
            frequency_hz.push(if f >= 10_000.0 { f } else { f * 1000.0 });
        }
        let header = XtfHeader {
            recording_program: cstr(&b[6..14]),
            sonar_name: cstr(&b[30..46]),
            sonar_type: u16le(b, 46).unwrap_or(0),
            n_channels,
            bytes_per_sample,
            channel_names,
            frequency_hz,
            nav_offset_y: f32le(b, 74).unwrap_or(0.0),
            nav_offset_x: f32le(b, 78).unwrap_or(0.0),
        };
        Ok(XtfFile { path, header, map })
    }

    fn data_start(&self) -> usize {
        // The six standard slots are already inside the 1024-byte header; only
        // channels beyond them push the packets back.
        FILE_HEADER_SIZE
            + self.header.n_channels.saturating_sub(CHAN_INFO_SLOTS) * CHAN_INFO_SIZE
    }

    /// Walk sidescan packets, yielding one `XtfPing` per channel.
    pub fn pings<F: FnMut(XtfPing) -> bool>(&self, mut f: F) {
        let b: &[u8] = &self.map;
        let mut off = self.data_start();
        while off + 14 <= b.len() {
            let Some(magic) = u16le(b, off) else { break };
            if magic != MAGIC {
                // resynchronise: XTF writers occasionally pad
                match b[off..].windows(2).position(|w| w == MAGIC.to_le_bytes()) {
                    Some(p) if p > 0 => {
                        off += p;
                        continue;
                    }
                    _ => break,
                }
            }
            let htype = b[off + 2];
            let Some(nbytes) = u32le(b, off + 10) else { break };
            let nbytes = nbytes as usize;
            if nbytes < 14 || off + nbytes > b.len() {
                break;
            }
            if htype == HDR_SONAR {
                let p = &b[off..off + nbytes];
                let n_chans = p.get(9).copied().unwrap_or(0) as usize;
                let time = packet_time(p).unwrap_or(f64::NAN);
                let ping_number = u32le(p, 28).unwrap_or(0);
                // Channel sub-headers begin at 256 and each is 64 bytes,
                // followed by that channel's samples.
                let mut co = 256usize;
                for ci in 0..n_chans.min(8) {
                    if co + 64 > p.len() {
                        break;
                    }
                    let sub = &p[co..co + 64];
                    let ch = u16le(sub, 2).unwrap_or(ci as u16) as u8;
                    let nsamples = u32le(sub, 8).unwrap_or(0) as usize;
                    let slant = f32le(sub, 40).unwrap_or(0.0) as f64;
                    let bps = self
                        .header
                        .bytes_per_sample
                        .get(ch as usize)
                        .copied()
                        .unwrap_or(2)
                        .max(1) as usize;
                    let dstart = co + 64;
                    let dlen = nsamples * bps;
                    if dstart + dlen > p.len() {
                        break;
                    }
                    let raw = &p[dstart..dstart + dlen];
                    let data = decode(raw, bps, nsamples);
                    let ping = XtfPing {
                        offset: off as u64,
                        channel: ch,
                        time,
                        ping_number,
                        lat: f64le(p, 60).unwrap_or(f64::NAN),
                        lon: f64le(p, 52).unwrap_or(f64::NAN),
                        heading: f32le(p, 88).unwrap_or(f32::NAN) as f64,
                        pitch: f32le(p, 92).unwrap_or(0.0) as f64,
                        roll: f32le(p, 96).unwrap_or(0.0) as f64,
                        depth_m: f32le(p, 72).unwrap_or(0.0) as f64,
                        altitude_m: f32le(p, 100).unwrap_or(0.0) as f64,
                        slant_range_m: slant,
                        nsamples,
                        data,
                    };
                    if !f(ping) {
                        return;
                    }
                    co = dstart + dlen;
                }
            }
            off += nbytes;
        }
    }
}

fn decode(raw: &[u8], bytes_per_sample: usize, n: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(n);
    match bytes_per_sample {
        1 => out.extend(raw.iter().take(n).map(|&v| v as f32)),
        2 => {
            for i in 0..n {
                let Some(v) = u16le(raw, i * 2) else { break };
                out.push(v as f32);
            }
        }
        4 => {
            for i in 0..n {
                let Some(v) = u32le(raw, i * 4) else { break };
                out.push(v as f32);
            }
        }
        _ => {}
    }
    out
}

/// Packet timestamp: year/month/day/hour/minute/second/hseconds at offset 14.
fn packet_time(p: &[u8]) -> Option<f64> {
    let y = u16le(p, 14)? as i32;
    let (mo, d, h, mi, s) = (p[16] as u32, p[17] as u32, p[18] as u32, p[19] as u32, p[20] as u32);
    let hs = p[21] as f64;
    if !(1970..=2100).contains(&y) || mo == 0 || mo > 12 || d == 0 || d > 31 {
        return None;
    }
    Some(crate::time::unix_from_utc(y, mo, d, h, mi, s) as f64 + hs / 100.0)
}
