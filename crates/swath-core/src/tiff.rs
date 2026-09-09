//! A reader for the GeoTIFFs a survey actually arrives with.
//!
//! Not a general TIFF library. It reads what GDAL writes when someone exports a
//! multibeam grid or a chart extract: one image, strips or tiles, uncompressed
//! or LZW/Deflate/PackBits, any of the integer and float sample formats, with
//! the georeferencing tags that say where the pixels are.
//!
//! The reason to have it at all rather than shelling out to `gdal_translate` is
//! the same reason the projections are in `geo`: the position of an imported
//! layer has to be defensible, and a transform this code owns can be tested
//! against an oracle. `tests/tiff.rs` reads the same files with rasterio and
//! requires the pixels and the corner coordinates to agree.
//!
//! Deliberately absent: multi-page files, JPEG-in-TIFF, CMYK, and anything
//! needing a colour-management pipeline. Those fail with a message naming what
//! they are rather than producing a plausible wrong picture.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

// ---- tags ------------------------------------------------------------------

const IMAGE_WIDTH: u16 = 256;
const IMAGE_LENGTH: u16 = 257;
const BITS_PER_SAMPLE: u16 = 258;
const COMPRESSION: u16 = 259;
const PHOTOMETRIC: u16 = 262;
const STRIP_OFFSETS: u16 = 273;
const SAMPLES_PER_PIXEL: u16 = 277;
const ROWS_PER_STRIP: u16 = 278;
const STRIP_BYTE_COUNTS: u16 = 279;
const PLANAR_CONFIG: u16 = 284;
const PREDICTOR: u16 = 317;
const COLOR_MAP: u16 = 320;
const TILE_WIDTH: u16 = 322;
const TILE_LENGTH: u16 = 323;
const TILE_OFFSETS: u16 = 324;
const TILE_BYTE_COUNTS: u16 = 325;
const EXTRA_SAMPLES: u16 = 338;
const SAMPLE_FORMAT: u16 = 339;

const MODEL_PIXEL_SCALE: u16 = 33550;
const MODEL_TIEPOINT: u16 = 33922;
const MODEL_TRANSFORMATION: u16 = 34264;
const GEO_KEY_DIRECTORY: u16 = 34735;
const GEO_ASCII_PARAMS: u16 = 34737;
const GDAL_NODATA: u16 = 42113;

/// GeoKey ids we act on.
const GT_MODEL_TYPE: u16 = 1024;
const GEOGRAPHIC_TYPE: u16 = 2048;
const PROJECTED_CS_TYPE: u16 = 3072;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ---- IFD -------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Field {
    typ: u16,
    count: u64,
    /// Either the inline value bytes or the offset to them.
    payload: [u8; 8],
    inline: bool,
}

struct Reader<'a> {
    b: &'a [u8],
    le: bool,
    big: bool,
}

impl<'a> Reader<'a> {
    fn u16(&self, at: usize) -> io::Result<u16> {
        let s = self.b.get(at..at + 2).ok_or_else(|| bad("short read"))?;
        let a = [s[0], s[1]];
        Ok(if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) })
    }
    fn u32(&self, at: usize) -> io::Result<u32> {
        let s = self.b.get(at..at + 4).ok_or_else(|| bad("short read"))?;
        let a = [s[0], s[1], s[2], s[3]];
        Ok(if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) })
    }
    fn u64(&self, at: usize) -> io::Result<u64> {
        let s = self.b.get(at..at + 8).ok_or_else(|| bad("short read"))?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(if self.le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) })
    }

    /// Element size of a TIFF field type, or 0 for types we do not know.
    fn type_size(t: u16) -> usize {
        match t {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 | 16 | 17 | 18 => 8,
            _ => 0,
        }
    }

    /// Where a field's values live when they do not fit in the entry itself.
    fn payload_offset(&self, f: &Field) -> usize {
        if self.big {
            (if self.le { u64::from_le_bytes(f.payload) } else { u64::from_be_bytes(f.payload) })
                as usize
        } else {
            let a = [f.payload[0], f.payload[1], f.payload[2], f.payload[3]];
            (if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }) as usize
        }
    }

    /// The bytes of a field: inline, or read from where the entry points.
    fn payload<'b>(&'b self, f: &'b Field, total: usize) -> io::Result<&'b [u8]> {
        if f.inline {
            Ok(&f.payload[..total.min(8)])
        } else {
            let off = self.payload_offset(f);
            self.b.get(off..off + total).ok_or_else(|| bad("field payload out of range"))
        }
    }

    /// Every value of a field, widened to u64. Signed types are sign-extended
    /// through i64 first so a negative predictor or offset survives.
    fn ints(&self, f: &Field) -> io::Result<Vec<u64>> {
        let sz = Self::type_size(f.typ);
        if sz == 0 {
            return Err(bad(format!("unsupported field type {}", f.typ)));
        }
        let bytes = self.payload(f, sz * f.count as usize)?;
        let mut out = Vec::with_capacity(f.count as usize);
        for i in 0..f.count as usize {
            let at = i * sz;
            if at + sz > bytes.len() {
                break;
            }
            let v = match f.typ {
                1 | 7 => bytes[at] as u64,
                6 => bytes[at] as i8 as i64 as u64,
                3 => {
                    let a = [bytes[at], bytes[at + 1]];
                    (if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }) as u64
                }
                8 => {
                    let a = [bytes[at], bytes[at + 1]];
                    (if self.le { i16::from_le_bytes(a) } else { i16::from_be_bytes(a) }) as i64
                        as u64
                }
                4 => {
                    let a: [u8; 4] = bytes[at..at + 4].try_into().unwrap();
                    (if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }) as u64
                }
                9 => {
                    let a: [u8; 4] = bytes[at..at + 4].try_into().unwrap();
                    (if self.le { i32::from_le_bytes(a) } else { i32::from_be_bytes(a) }) as i64
                        as u64
                }
                16 | 18 => {
                    let a: [u8; 8] = bytes[at..at + 8].try_into().unwrap();
                    if self.le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) }
                }
                17 => {
                    let a: [u8; 8] = bytes[at..at + 8].try_into().unwrap();
                    (if self.le { i64::from_le_bytes(a) } else { i64::from_be_bytes(a) }) as u64
                }
                _ => return Err(bad(format!("field type {} is not an integer", f.typ))),
            };
            out.push(v);
        }
        Ok(out)
    }

    fn floats(&self, f: &Field) -> io::Result<Vec<f64>> {
        let sz = Self::type_size(f.typ);
        let bytes = self.payload(f, sz * f.count as usize)?;
        let mut out = Vec::with_capacity(f.count as usize);
        for i in 0..f.count as usize {
            let at = i * sz;
            if at + sz > bytes.len() {
                break;
            }
            out.push(match f.typ {
                11 => {
                    let a: [u8; 4] = bytes[at..at + 4].try_into().unwrap();
                    (if self.le { f32::from_le_bytes(a) } else { f32::from_be_bytes(a) }) as f64
                }
                12 => {
                    let a: [u8; 8] = bytes[at..at + 8].try_into().unwrap();
                    if self.le { f64::from_le_bytes(a) } else { f64::from_be_bytes(a) }
                }
                5 | 10 => {
                    let n: [u8; 4] = bytes[at..at + 4].try_into().unwrap();
                    let d: [u8; 4] = bytes[at + 4..at + 8].try_into().unwrap();
                    let (n, d) = if self.le {
                        (u32::from_le_bytes(n) as f64, u32::from_le_bytes(d) as f64)
                    } else {
                        (u32::from_be_bytes(n) as f64, u32::from_be_bytes(d) as f64)
                    };
                    if d == 0.0 { 0.0 } else { n / d }
                }
                _ => return Err(bad(format!("field type {} is not a float", f.typ))),
            });
        }
        Ok(out)
    }

    fn ascii(&self, f: &Field) -> io::Result<String> {
        let bytes = self.payload(f, f.count as usize)?;
        Ok(String::from_utf8_lossy(bytes).trim_end_matches('\0').to_string())
    }
}

// ---- sample description ----------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    Uint,
    Int,
    Float,
}

/// How the pixels are laid out on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Strips of `rows_per` full-width rows.
    Strips { rows_per: usize },
    /// Tiles of `w` x `h`.
    Tiles { w: usize, h: usize },
}

pub struct GeoTiff {
    path: PathBuf,
    map: Mmap,
    le: bool,
    big: bool,

    pub width: usize,
    pub height: usize,
    pub samples: usize,
    pub bits: usize,
    pub kind: SampleKind,
    pub photometric: u16,
    /// Palette from tag 320, if the file is colour-mapped.
    pub palette: Option<Vec<[u8; 3]>>,
    pub nodata: Option<f64>,

    /// Pixel (col, row) -> model (x, y):
    /// `x = t[0]*col + t[1]*row + t[2]`, `y = t[3]*col + t[4]*row + t[5]`.
    pub transform: [f64; 6],
    pub epsg: Option<u32>,
    pub crs_name: String,

    layout: Layout,
    compression: u16,
    predictor: u16,
    offsets: Vec<u64>,
    counts: Vec<u64>,
    /// Decoded blocks, keyed by block index.
    cache: std::cell::RefCell<BlockCache>,
}

#[derive(Default)]
struct BlockCache {
    map: HashMap<usize, std::rc::Rc<Vec<u8>>>,
    order: Vec<usize>,
}

impl BlockCache {
    fn get(&mut self, i: usize) -> Option<std::rc::Rc<Vec<u8>>> {
        self.map.get(&i).cloned()
    }
    fn put(&mut self, i: usize, v: std::rc::Rc<Vec<u8>>) {
        self.map.insert(i, v);
        self.order.push(i);
        while self.order.len() > 64 {
            let old = self.order.remove(0);
            if !self.order.contains(&old) {
                self.map.remove(&old);
            }
        }
    }
}

impl GeoTiff {
    pub fn open(path: impl AsRef<Path>) -> io::Result<GeoTiff> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::File::open(&path)?;
        // Safety: the file is read-only for the life of this value. A file
        // truncated underneath us would fault; so would every other mmap in
        // this program, and the recordings are treated the same way.
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < 8 {
            return Err(bad("file is too short to be a TIFF"));
        }
        let le = match &map[0..2] {
            b"II" => true,
            b"MM" => false,
            _ => return Err(bad("not a TIFF: no byte-order mark")),
        };
        let r0 = Reader { b: &map, le, big: false };
        let magic = r0.u16(2)?;
        let big = match magic {
            42 => false,
            43 => true,
            _ => return Err(bad(format!("not a TIFF: magic {magic}"))),
        };
        let r = Reader { b: &map, le, big };
        let (ifd_off, entry_size) = if big {
            if r.u16(4)? != 8 {
                return Err(bad("BigTIFF with an offset size other than 8"));
            }
            (r.u64(8)? as usize, 20usize)
        } else {
            (r.u32(4)? as usize, 12usize)
        };

        // ---- read the first IFD ----
        let (n_entries, first) = if big {
            (r.u64(ifd_off)? as usize, ifd_off + 8)
        } else {
            (r.u16(ifd_off)? as usize, ifd_off + 2)
        };
        let mut fields: HashMap<u16, Field> = HashMap::new();
        for i in 0..n_entries {
            let at = first + i * entry_size;
            let tag = r.u16(at)?;
            let typ = r.u16(at + 2)?;
            let count = if big { r.u64(at + 4)? } else { r.u32(at + 4)? as u64 };
            let val_at = if big { at + 12 } else { at + 8 };
            let val_len = if big { 8 } else { 4 };
            let mut payload = [0u8; 8];
            let src = map
                .get(val_at..val_at + val_len)
                .ok_or_else(|| bad("IFD entry runs off the end"))?;
            payload[..val_len].copy_from_slice(src);
            let inline = Reader::type_size(typ) * count as usize <= val_len;
            fields.insert(tag, Field { typ, count, payload, inline });
        }

        let one = |tag: u16, default: u64| -> io::Result<u64> {
            match fields.get(&tag) {
                Some(f) => Ok(r.ints(f)?.first().copied().unwrap_or(default)),
                None => Ok(default),
            }
        };

        let width = one(IMAGE_WIDTH, 0)? as usize;
        let height = one(IMAGE_LENGTH, 0)? as usize;
        if width == 0 || height == 0 {
            return Err(bad("image has no size"));
        }
        let samples = one(SAMPLES_PER_PIXEL, 1)? as usize;
        let bits_all: Vec<u64> = match fields.get(&BITS_PER_SAMPLE) {
            Some(f) => r.ints(f)?,
            None => vec![1],
        };
        let bits = *bits_all.first().unwrap_or(&1) as usize;
        if bits_all.iter().any(|&b| b as usize != bits) {
            return Err(bad("bands with different bit depths are not supported"));
        }
        if ![8usize, 16, 32, 64].contains(&bits) {
            return Err(bad(format!("{bits}-bit samples are not supported")));
        }
        let fmt = match fields.get(&SAMPLE_FORMAT) {
            Some(f) => r.ints(f)?.first().copied().unwrap_or(1),
            None => 1,
        };
        let kind = match fmt {
            1 => SampleKind::Uint,
            2 => SampleKind::Int,
            3 => SampleKind::Float,
            other => return Err(bad(format!("sample format {other} is not supported"))),
        };
        if kind == SampleKind::Float && bits != 32 && bits != 64 {
            return Err(bad(format!("{bits}-bit floating point is not supported")));
        }
        let compression = one(COMPRESSION, 1)? as u16;
        let predictor = one(PREDICTOR, 1)? as u16;
        let planar = one(PLANAR_CONFIG, 1)? as u16;
        if planar != 1 && samples > 1 {
            return Err(bad("planar (band-separate) files are not supported"));
        }
        let _ = planar;
        let photometric = one(PHOTOMETRIC, 1)? as u16;
        if photometric == 6 {
            return Err(bad("YCbCr (JPEG) TIFFs are not supported"));
        }
        if photometric == 5 {
            return Err(bad("CMYK TIFFs are not supported"));
        }

        let palette = match fields.get(&COLOR_MAP) {
            Some(f) => {
                let v = r.ints(f)?;
                let n = v.len() / 3;
                Some(
                    (0..n)
                        .map(|i| {
                            [
                                (v[i] >> 8) as u8,
                                (v[n + i] >> 8) as u8,
                                (v[2 * n + i] >> 8) as u8,
                            ]
                        })
                        .collect(),
                )
            }
            None => None,
        };

        let layout = if fields.contains_key(&TILE_WIDTH) {
            Layout::Tiles {
                w: one(TILE_WIDTH, 0)? as usize,
                h: one(TILE_LENGTH, 0)? as usize,
            }
        } else {
            Layout::Strips { rows_per: one(ROWS_PER_STRIP, height as u64)?.max(1) as usize }
        };
        let (off_tag, cnt_tag) = match layout {
            Layout::Tiles { .. } => (TILE_OFFSETS, TILE_BYTE_COUNTS),
            Layout::Strips { .. } => (STRIP_OFFSETS, STRIP_BYTE_COUNTS),
        };
        let offsets = fields
            .get(&off_tag)
            .ok_or_else(|| bad("no strip or tile offsets"))
            .and_then(|f| r.ints(f))?;
        let counts = fields
            .get(&cnt_tag)
            .ok_or_else(|| bad("no strip or tile byte counts"))
            .and_then(|f| r.ints(f))?;
        if offsets.len() != counts.len() {
            return Err(bad("offset and byte-count tables disagree in length"));
        }

        // ---- georeferencing ----
        let mut transform = [1.0, 0.0, 0.0, 0.0, -1.0, 0.0];
        if let Some(f) = fields.get(&MODEL_TRANSFORMATION) {
            let m = r.floats(f)?;
            if m.len() >= 8 {
                // 4x4 row-major; we use the 2D part
                transform = [m[0], m[1], m[3], m[4], m[5], m[7]];
            }
        } else if let (Some(sf), Some(tf)) =
            (fields.get(&MODEL_PIXEL_SCALE), fields.get(&MODEL_TIEPOINT))
        {
            let s = r.floats(sf)?;
            let t = r.floats(tf)?;
            if s.len() >= 2 && t.len() >= 6 {
                // The tie point maps raster (i,j) to model (x,y); with a pixel
                // scale the raster is axis-aligned and y runs the other way.
                transform = [s[0], 0.0, t[3] - t[0] * s[0], 0.0, -s[1], t[4] + t[1] * s[1]];
            }
        }

        let mut epsg = None;
        let mut crs_name = String::new();
        if let Some(f) = fields.get(&GEO_KEY_DIRECTORY) {
            let keys = r.ints(f)?;
            let ascii = fields
                .get(&GEO_ASCII_PARAMS)
                .and_then(|f| r.ascii(f).ok())
                .unwrap_or_default();
            // header is four shorts, then `count` four-short entries
            if keys.len() >= 4 {
                let n = keys[3] as usize;
                let mut model = 0u64;
                for i in 0..n {
                    let at = 4 + i * 4;
                    if at + 3 >= keys.len() {
                        break;
                    }
                    let (id, loc, _cnt, val) = (keys[at] as u16, keys[at + 1], keys[at + 2], keys[at + 3]);
                    match id {
                        GT_MODEL_TYPE if loc == 0 => model = val,
                        PROJECTED_CS_TYPE if loc == 0 && val != 0 && val != 32767 => {
                            epsg = Some(val as u32)
                        }
                        GEOGRAPHIC_TYPE if loc == 0 && epsg.is_none() && val != 0 && val != 32767 => {
                            epsg = Some(val as u32)
                        }
                        _ => {}
                    }
                }
                // model 2 is geographic; if the file only carried a geographic
                // key we already have it, but a file with neither and a
                // degree-sized pixel is almost certainly WGS84.
                if epsg.is_none() && (model == 2 || transform[0].abs() < 0.01) {
                    epsg = Some(4326);
                }
            }
            crs_name = ascii.split('|').next().unwrap_or("").trim().to_string();
        } else if transform[0].abs() < 0.01 {
            epsg = Some(4326);
        }

        let nodata = fields
            .get(&GDAL_NODATA)
            .and_then(|f| r.ascii(f).ok())
            .and_then(|s| s.trim().parse::<f64>().ok());

        // ExtraSamples tells us an RGBA file's fourth band is alpha rather than
        // a fourth colour; we only need to know it exists.
        let _ = fields.get(&EXTRA_SAMPLES);

        Ok(GeoTiff {
            path,
            map,
            le,
            big,
            width,
            height,
            samples,
            bits,
            kind,
            photometric,
            palette,
            nodata,
            transform,
            epsg,
            crs_name,
            layout,
            compression,
            predictor,
            offsets,
            counts,
            cache: std::cell::RefCell::new(BlockCache::default()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the image is a single measured band rather than a picture.
    pub fn is_single_band(&self) -> bool {
        self.samples == 1 && self.palette.is_none()
    }

    /// Model coordinates of a pixel centre.
    pub fn pixel_to_model(&self, col: f64, row: f64) -> (f64, f64) {
        let t = &self.transform;
        (
            t[0] * (col + 0.5) + t[1] * (row + 0.5) + t[2],
            t[3] * (col + 0.5) + t[4] * (row + 0.5) + t[5],
        )
    }

    /// The inverse: model coordinates back to a fractional pixel.
    pub fn model_to_pixel(&self, x: f64, y: f64) -> (f64, f64) {
        let t = &self.transform;
        let det = t[0] * t[4] - t[1] * t[3];
        if det.abs() < 1e-30 {
            return (f64::NAN, f64::NAN);
        }
        let dx = x - t[2];
        let dy = y - t[5];
        ((t[4] * dx - t[1] * dy) / det - 0.5, (t[0] * dy - t[3] * dx) / det - 0.5)
    }

    /// Bytes per pixel once decoded.
    fn px_bytes(&self) -> usize {
        self.bits / 8 * self.samples
    }

    fn blocks_across(&self) -> usize {
        match self.layout {
            Layout::Tiles { w, .. } => self.width.div_ceil(w),
            Layout::Strips { .. } => 1,
        }
    }

    /// The block holding a pixel, and the pixel's position inside it.
    fn locate(&self, col: usize, row: usize) -> Option<(usize, usize, usize, usize)> {
        match self.layout {
            Layout::Strips { rows_per } => {
                let b = row / rows_per;
                Some((b, col, row % rows_per, self.width))
            }
            Layout::Tiles { w, h } => {
                let b = (row / h) * self.blocks_across() + col / w;
                Some((b, col % w, row % h, w))
            }
        }
    }

    /// Decode one block, from the cache when it is there.
    fn block(&self, i: usize) -> io::Result<std::rc::Rc<Vec<u8>>> {
        if let Some(v) = self.cache.borrow_mut().get(i) {
            return Ok(v);
        }
        let off = *self.offsets.get(i).ok_or_else(|| bad("block index out of range"))? as usize;
        let n = *self.counts.get(i).ok_or_else(|| bad("block index out of range"))? as usize;
        let raw = self
            .map
            .get(off..off + n)
            .ok_or_else(|| bad(format!("block {i} runs past the end of the file")))?;

        let mut data = match self.compression {
            1 => raw.to_vec(),
            5 => lzw_decode(raw)?,
            8 | 32946 => miniz_oxide::inflate::decompress_to_vec_zlib(raw)
                .map_err(|e| bad(format!("deflate: {e:?}")))?,
            32773 => packbits_decode(raw),
            other => {
                return Err(bad(format!(
                    "compression {other} is not supported (need none, LZW, Deflate or PackBits)"
                )))
            }
        };

        // Undo the predictor. Both variants work along a row, so the row width
        // in this block has to be known.
        let (bw, bh) = match self.layout {
            Layout::Strips { rows_per } => {
                let full = self.height.div_ceil(rows_per);
                let rows = if i + 1 == full && self.height % rows_per != 0 {
                    self.height % rows_per
                } else {
                    rows_per
                };
                (self.width, rows)
            }
            Layout::Tiles { w, h } => (w, h),
        };
        match self.predictor {
            1 => {}
            2 => unpredict_horizontal(&mut data, bw, bh, self.samples, self.bits, self.le),
            3 => unpredict_float(&mut data, bw, bh, self.samples, self.bits, self.le),
            other => return Err(bad(format!("predictor {other} is not supported"))),
        }
        let rc = std::rc::Rc::new(data);
        self.cache.borrow_mut().put(i, rc.clone());
        Ok(rc)
    }

    /// One band of one pixel, as a real number. `None` outside the image or
    /// where the block cannot be read.
    pub fn value(&self, col: usize, row: usize, band: usize) -> Option<f64> {
        if col >= self.width || row >= self.height || band >= self.samples {
            return None;
        }
        let (bi, bx, by, bw) = self.locate(col, row)?;
        let data = self.block(bi).ok()?;
        let stride = bw * self.px_bytes();
        let at = by * stride + bx * self.px_bytes() + band * (self.bits / 8);
        let s = data.get(at..at + self.bits / 8)?;
        Some(self.decode_sample(s))
    }

    fn decode_sample(&self, s: &[u8]) -> f64 {
        let le = self.le;
        macro_rules! num {
            ($t:ty, $n:expr) => {{
                let a: [u8; $n] = s[..$n].try_into().unwrap();
                if le { <$t>::from_le_bytes(a) } else { <$t>::from_be_bytes(a) }
            }};
        }
        match (self.kind, self.bits) {
            (SampleKind::Float, 32) => num!(f32, 4) as f64,
            (SampleKind::Float, 64) => num!(f64, 8),
            (SampleKind::Int, 8) => s[0] as i8 as f64,
            (SampleKind::Int, 16) => num!(i16, 2) as f64,
            (SampleKind::Int, 32) => num!(i32, 4) as f64,
            (SampleKind::Int, 64) => num!(i64, 8) as f64,
            (SampleKind::Uint, 8) => s[0] as f64,
            (SampleKind::Uint, 16) => num!(u16, 2) as f64,
            (SampleKind::Uint, 32) => num!(u32, 4) as f64,
            (SampleKind::Uint, 64) => num!(u64, 8) as f64,
            _ => f64::NAN,
        }
    }

    /// A pixel as 8-bit RGB, for files that are pictures rather than
    /// measurements. Honours the palette and the white-is-zero convention.
    pub fn rgb(&self, col: usize, row: usize) -> Option<[u8; 3]> {
        let scale = |v: f64| -> u8 {
            let m = match (self.kind, self.bits) {
                (SampleKind::Uint, 8) => 255.0,
                (SampleKind::Uint, 16) => 65535.0,
                (SampleKind::Float, _) => 1.0,
                _ => 255.0,
            };
            (v / m * 255.0).clamp(0.0, 255.0) as u8
        };
        if let Some(pal) = &self.palette {
            let i = self.value(col, row, 0)? as usize;
            return Some(*pal.get(i).unwrap_or(&[0, 0, 0]));
        }
        if self.samples >= 3 {
            return Some([
                scale(self.value(col, row, 0)?),
                scale(self.value(col, row, 1)?),
                scale(self.value(col, row, 2)?),
            ]);
        }
        let v = scale(self.value(col, row, 0)?);
        // photometric 0 is white-is-zero, which reads as a negative otherwise
        let v = if self.photometric == 0 { 255 - v } else { v };
        Some([v, v, v])
    }

    /// Alpha of a pixel, for an RGBA file. 255 when the file has no alpha.
    pub fn alpha(&self, col: usize, row: usize) -> u8 {
        if self.samples < 4 || self.palette.is_some() {
            return 255;
        }
        let m = if self.bits == 16 { 65535.0 } else { 255.0 };
        self.value(col, row, 3)
            .map(|v| (v / m * 255.0).clamp(0.0, 255.0) as u8)
            .unwrap_or(255)
    }

    /// A short human description for the layers panel.
    pub fn describe(&self) -> String {
        let fmt = match (self.kind, self.bits) {
            (SampleKind::Float, b) => format!("float{b}"),
            (SampleKind::Int, b) => format!("int{b}"),
            (SampleKind::Uint, b) => format!("uint{b}"),
        };
        let comp = match self.compression {
            1 => "none",
            5 => "LZW",
            8 | 32946 => "deflate",
            32773 => "packbits",
            _ => "?",
        };
        format!(
            "{}x{} · {} band{} · {fmt} · {comp}",
            self.width,
            self.height,
            self.samples,
            if self.samples == 1 { "" } else { "s" }
        )
    }

    pub fn is_big(&self) -> bool {
        self.big
    }
}

// ---- decompression ---------------------------------------------------------

/// TIFF's PackBits: a run-length scheme with a signed count byte.
fn packbits_decode(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() * 2);
    let mut i = 0;
    while i < src.len() {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            let take = n as usize + 1;
            let end = (i + take).min(src.len());
            out.extend_from_slice(&src[i..end]);
            i = end;
        } else if n != -128 {
            if i >= src.len() {
                break;
            }
            let take = (1 - n as i32) as usize;
            out.extend(std::iter::repeat(src[i]).take(take));
            i += 1;
        }
        // -128 is a no-op by the spec
    }
    out
}

/// TIFF LZW: MSB-first codes of 9 to 12 bits, with the off-by-one code-width
/// bump that the TIFF 6 specification describes and every encoder implements.
fn lzw_decode(src: &[u8]) -> io::Result<Vec<u8>> {
    const CLEAR: u16 = 256;
    const EOI: u16 = 257;
    let mut out: Vec<u8> = Vec::with_capacity(src.len() * 3);
    // Entries are (prefix code, appended byte); the first 256 are literals.
    let mut prefix: Vec<u16> = Vec::with_capacity(4096);
    let mut suffix: Vec<u8> = Vec::with_capacity(4096);
    let reset = |p: &mut Vec<u16>, s: &mut Vec<u8>| {
        p.clear();
        s.clear();
        for i in 0..256u16 {
            p.push(u16::MAX);
            s.push(i as u8);
        }
        p.push(u16::MAX);
        s.push(0); // 256 clear
        p.push(u16::MAX);
        s.push(0); // 257 eoi
    };
    reset(&mut prefix, &mut suffix);

    let mut width = 9u32;
    let mut bitpos = 0usize;
    let total_bits = src.len() * 8;
    let mut prev: Option<u16> = None;
    let mut scratch: Vec<u8> = Vec::with_capacity(64);

    while bitpos + width as usize <= total_bits {
        let mut code = 0u32;
        for _ in 0..width {
            let byte = src[bitpos >> 3];
            let bit = (byte >> (7 - (bitpos & 7))) & 1;
            code = (code << 1) | bit as u32;
            bitpos += 1;
        }
        let code = code as u16;
        if code == EOI {
            break;
        }
        if code == CLEAR {
            reset(&mut prefix, &mut suffix);
            width = 9;
            prev = None;
            continue;
        }
        // Expand, either an existing entry or the classic KwKwK case where the
        // code being read is the one this step is about to define.
        scratch.clear();
        let mut walk = if (code as usize) < prefix.len() {
            code
        } else if let Some(p) = prev {
            // deferred: emit prev's expansion then its first byte
            let mut w = p;
            while w != u16::MAX {
                scratch.push(suffix[w as usize]);
                w = prefix[w as usize];
            }
            scratch.reverse();
            let first = scratch[0];
            scratch.push(first);
            out.extend_from_slice(&scratch);
            if prefix.len() < 4096 {
                prefix.push(p);
                suffix.push(first);
            }
            if prefix.len() + 1 >= (1 << width) && width < 12 {
                width += 1;
            }
            prev = Some(code);
            continue;
        } else {
            return Err(bad("LZW stream starts with an undefined code"));
        };
        while walk != u16::MAX {
            scratch.push(suffix[walk as usize]);
            walk = prefix[walk as usize];
        }
        scratch.reverse();
        out.extend_from_slice(&scratch);
        if let Some(p) = prev {
            if prefix.len() < 4096 {
                prefix.push(p);
                suffix.push(scratch[0]);
            }
        }
        // The bump happens one code early, which is the quirk that separates a
        // TIFF LZW decoder from a GIF one.
        if prefix.len() + 1 >= (1 << width) && width < 12 {
            width += 1;
        }
        prev = Some(code);
    }
    Ok(out)
}

/// Predictor 2: each sample is stored as its difference from the one to its
/// left, per band.
fn unpredict_horizontal(
    data: &mut [u8],
    w: usize,
    h: usize,
    samples: usize,
    bits: usize,
    le: bool,
) {
    let bytes = bits / 8;
    let stride = w * samples * bytes;
    for row in 0..h {
        let base = row * stride;
        if base + stride > data.len() {
            break;
        }
        for col in 1..w {
            for s in 0..samples {
                let cur = base + (col * samples + s) * bytes;
                let prev = base + ((col - 1) * samples + s) * bytes;
                // The stored differences are in the file's byte order, so the
                // addition has to be too: doing it little-endian on a
                // big-endian file carries across the wrong byte boundary.
                match bits {
                    8 => data[cur] = data[cur].wrapping_add(data[prev]),
                    16 => {
                        let pa = [data[prev], data[prev + 1]];
                        let ca = [data[cur], data[cur + 1]];
                        let (a, b) = if le {
                            (u16::from_le_bytes(pa), u16::from_le_bytes(ca))
                        } else {
                            (u16::from_be_bytes(pa), u16::from_be_bytes(ca))
                        };
                        let sum = b.wrapping_add(a);
                        data[cur..cur + 2].copy_from_slice(&if le {
                            sum.to_le_bytes()
                        } else {
                            sum.to_be_bytes()
                        });
                    }
                    32 => {
                        let pa: [u8; 4] = data[prev..prev + 4].try_into().unwrap();
                        let ca: [u8; 4] = data[cur..cur + 4].try_into().unwrap();
                        let (a, b) = if le {
                            (u32::from_le_bytes(pa), u32::from_le_bytes(ca))
                        } else {
                            (u32::from_be_bytes(pa), u32::from_be_bytes(ca))
                        };
                        let sum = b.wrapping_add(a);
                        data[cur..cur + 4].copy_from_slice(&if le {
                            sum.to_le_bytes()
                        } else {
                            sum.to_be_bytes()
                        });
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Predictor 3: the bytes of each sample are de-interleaved across the row and
/// then horizontally differenced, which compresses floating point far better
/// than differencing whole values.
fn unpredict_float(data: &mut [u8], w: usize, h: usize, samples: usize, bits: usize, le: bool) {
    let bytes = bits / 8;
    let stride = w * samples * bytes;
    let count = w * samples;
    let mut row_buf = vec![0u8; stride];
    for row in 0..h {
        let base = row * stride;
        if base + stride > data.len() {
            break;
        }
        let r = &mut data[base..base + stride];
        for i in 1..stride {
            r[i] = r[i].wrapping_add(r[i - 1]);
        }
        // De-shuffle. Plane 0 holds every sample's most significant byte, so
        // on a little-endian file the planes are written back in reverse --
        // getting this the wrong way round turns a seabed at -23 m into
        // numbers around 1e-40, which is exactly what it looks like.
        for i in 0..count {
            for b in 0..bytes {
                let at = if le { i * bytes + (bytes - 1 - b) } else { i * bytes + b };
                row_buf[at] = r[b * count + i];
            }
        }
        r.copy_from_slice(&row_buf);
    }
}
