//! TIFF / DNG parser for reading raw Bayer pixel data and metadata.
#![allow(dead_code)]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    pub fn read_u16(&self, buf: &[u8]) -> u16 {
        match self {
            Endian::Little => u16::from_le_bytes([buf[0], buf[1]]),
            Endian::Big => u16::from_be_bytes([buf[0], buf[1]]),
        }
    }

    pub fn read_u32(&self, buf: &[u8]) -> u32 {
        match self {
            Endian::Little => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            Endian::Big => u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        }
    }

    pub fn read_i16(&self, buf: &[u8]) -> i16 {
        match self {
            Endian::Little => i16::from_le_bytes([buf[0], buf[1]]),
            Endian::Big => i16::from_be_bytes([buf[0], buf[1]]),
        }
    }

    pub fn read_i32(&self, buf: &[u8]) -> i32 {
        match self {
            Endian::Little => i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            Endian::Big => i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        }
    }

    pub fn read_f32(&self, buf: &[u8]) -> f32 {
        match self {
            Endian::Little => f32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            Endian::Big => f32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        }
    }

    pub fn read_f64(&self, buf: &[u8]) -> f64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[..8]);
        match self {
            Endian::Little => f64::from_le_bytes(b),
            Endian::Big => f64::from_be_bytes(b),
        }
    }
}

/// Reads TIFF tag values, handling inline values (<= 4 bytes) vs offset values (> 4 bytes).
pub fn read_tiff_tag_value<R: Read + Seek>(
    reader: &mut R,
    endian: Endian,
    dtype: u16,
    count: usize,
    val_or_off: u32,
) -> Result<Vec<f64>> {
    let type_size = match dtype {
        1 | 2 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => 1,
    };
    let total_bytes = count * type_size;

    let raw_bytes = if total_bytes <= 4 {
        let b = match endian {
            Endian::Little => val_or_off.to_le_bytes(),
            Endian::Big => val_or_off.to_be_bytes(),
        };
        b[..total_bytes].to_vec()
    } else {
        let cur = reader.stream_position()?;
        reader.seek(SeekFrom::Start(val_or_off as u64))?;
        let mut buf = vec![0u8; total_bytes];
        reader.read_exact(&mut buf)?;
        reader.seek(SeekFrom::Start(cur))?;
        buf
    };

    let mut res = Vec::with_capacity(count);
    match dtype {
        1 | 7 => {
            // BYTE / UNDEFINED
            for &b in &raw_bytes {
                res.push(b as f64);
            }
        }
        3 => {
            // SHORT (u16)
            for chunk in raw_bytes.chunks_exact(2) {
                res.push(endian.read_u16(chunk) as f64);
            }
        }
        4 => {
            // LONG (u32)
            for chunk in raw_bytes.chunks_exact(4) {
                res.push(endian.read_u32(chunk) as f64);
            }
        }
        5 => {
            // RATIONAL (u32 / u32)
            for chunk in raw_bytes.chunks_exact(8) {
                let num = endian.read_u32(&chunk[0..4]);
                let den = endian.read_u32(&chunk[4..8]);
                res.push(if den != 0 { num as f64 / den as f64 } else { 0.0 });
            }
        }
        8 => {
            // SSHORT (i16)
            for chunk in raw_bytes.chunks_exact(2) {
                res.push(endian.read_i16(chunk) as f64);
            }
        }
        9 => {
            // SLONG (i32)
            for chunk in raw_bytes.chunks_exact(4) {
                res.push(endian.read_i32(chunk) as f64);
            }
        }
        10 => {
            // SRATIONAL (i32 / i32)
            for chunk in raw_bytes.chunks_exact(8) {
                let num = endian.read_i32(&chunk[0..4]);
                let den = endian.read_i32(&chunk[4..8]);
                res.push(if den != 0 { num as f64 / den as f64 } else { 0.0 });
            }
        }
        11 => {
            // FLOAT (f32)
            for chunk in raw_bytes.chunks_exact(4) {
                res.push(endian.read_f32(chunk) as f64);
            }
        }
        12 => {
            // DOUBLE (f64)
            for chunk in raw_bytes.chunks_exact(8) {
                res.push(endian.read_f64(chunk));
            }
        }
        _ => {
            for &b in &raw_bytes {
                res.push(b as f64);
            }
        }
    }
    Ok(res)
}

/// Remosaics a 3-channel 16-bit linear RGB image into a single-channel Bayer RGGB mosaic.
/// Applies an energy-preserving 2x2 anti-aliasing filter to suppress Nyquist fold-back,
/// false color fringing, and moiré artifacts during downstream re-demosaicing.
pub fn remosaic_rgb_to_bayer(rgb_u16: &[u16], width: usize, height: usize) -> Vec<u16> {
    let mut bayer = vec![0u16; width * height];
    let get_rgb = |x: usize, y: usize, ch: usize| -> u32 {
        let px = x.min(width - 1);
        let py = y.min(height - 1);
        rgb_u16[(py * width + px) * 3 + ch] as u32
    };

    for y in (0..height).step_by(2) {
        let y0_w = y * width;
        let y1_w = (y + 1).min(height - 1) * width;

        for x in (0..width).step_by(2) {
            let x0 = x;
            let x1 = (x + 1).min(width - 1);

            // Red at (x, y): center-weighted filter across 2x2 cell
            let r00 = get_rgb(x0, y, 0);
            let r01 = get_rgb(x1, y, 0);
            let r10 = get_rgb(x0, y + 1, 0);
            let r11 = get_rgb(x1, y + 1, 0);
            let r_filtered = ((r00 * 4 + r01 * 2 + r10 * 2 + r11 + 4) / 9) as u16;
            bayer[y0_w + x0] = r_filtered;

            // Green 1 at (x+1, y)
            if x + 1 < width {
                let g00 = get_rgb(x0, y, 1);
                let g01 = get_rgb(x1, y, 1);
                let g10 = get_rgb(x0, y + 1, 1);
                let g11 = get_rgb(x1, y + 1, 1);
                let g1_filtered = ((g01 * 4 + g00 * 2 + g11 * 2 + g10 + 4) / 9) as u16;
                bayer[y0_w + x1] = g1_filtered;
            }

            // Green 2 at (x, y+1)
            if y + 1 < height {
                let g00 = get_rgb(x0, y, 1);
                let g01 = get_rgb(x1, y, 1);
                let g10 = get_rgb(x0, y + 1, 1);
                let g11 = get_rgb(x1, y + 1, 1);
                let g2_filtered = ((g10 * 4 + g00 * 2 + g11 * 2 + g01 + 4) / 9) as u16;
                bayer[y1_w + x0] = g2_filtered;
            }

            // Blue at (x+1, y+1)
            if x + 1 < width && y + 1 < height {
                let b00 = get_rgb(x0, y, 2);
                let b01 = get_rgb(x1, y, 2);
                let b10 = get_rgb(x0, y + 1, 2);
                let b11 = get_rgb(x1, y + 1, 2);
                let b_filtered = ((b11 * 4 + b01 * 2 + b10 * 2 + b00 + 4) / 9) as u16;
                bayer[y1_w + x1] = b_filtered;
            }
        }
    }
    bayer
}

#[derive(Debug, Clone)]
pub struct DngInfo {
    pub filepath: String,
    pub endian: Endian,
    pub width: usize,
    pub height: usize,
    pub crop_x: usize,
    pub crop_y: usize,
    pub crop_w: usize,
    pub crop_h: usize,
    pub bits_per_sample: u32,
    pub samples_per_pixel: u32,
    pub photometric_interpretation: u32,
    pub cfa_pattern: Vec<u8>,
    #[allow(dead_code)]
    pub cfa_repeat_dim: [u16; 2],
    pub black_level: f64,
    pub white_level: u32,
    pub as_shot_neutral: [f64; 3],
    pub strip_offsets: Vec<u32>,
    pub strip_byte_counts: Vec<u32>,
    pub raw_pixel_length: usize,
    pub active_top: usize,
    pub active_left: usize,
    pub active_bottom: usize,
    pub active_right: usize,
}

impl DngInfo {
    pub fn parse<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let filepath = path_ref.to_string_lossy().to_string();
        let mut file = File::open(path_ref).with_context(|| format!("Opening DNG file: {}", filepath))?;

        let mut header = [0u8; 16];
        file.read_exact(&mut header)?;

        let endian = if &header[0..2] == b"II" {
            Endian::Little
        } else if &header[0..2] == b"MM" {
            Endian::Big
        } else {
            return Err(anyhow!("Not a valid TIFF/DNG file (invalid byte order mark)"));
        };

        let first_ifd = endian.read_u32(&header[4..8]);

        // Read IFD0
        file.seek(SeekFrom::Start(first_ifd as u64))?;
        let mut num_tags_buf = [0u8; 2];
        file.read_exact(&mut num_tags_buf)?;
        let num_tags = endian.read_u16(&num_tags_buf) as usize;

        let mut subifd_offsets = Vec::new();
        let mut as_shot_neutral = [1.0, 1.0, 1.0];

        for _ in 0..num_tags {
            let mut entry = [0u8; 12];
            file.read_exact(&mut entry)?;
            let tag = endian.read_u16(&entry[0..2]);
            let dtype = endian.read_u16(&entry[2..4]);
            let count = endian.read_u32(&entry[4..8]) as usize;
            let val_or_off = endian.read_u32(&entry[8..12]);

            if tag == 0x014a {
                // SubIFDs
                let vals = read_tiff_tag_value(&mut file, endian, dtype, count, val_or_off)?;
                subifd_offsets = vals.into_iter().map(|v| v as u32).collect();
            } else if tag == 0xc628 {
                // AsShotNeutral
                let vals = read_tiff_tag_value(&mut file, endian, dtype, count, val_or_off)?;
                if vals.len() >= 3 {
                    as_shot_neutral = [vals[0], vals[1], vals[2]];
                }
            }
        }

        let target_ifds = if !subifd_offsets.is_empty() {
            subifd_offsets
        } else {
            vec![first_ifd]
        };

        // Read SubIFD 0 (Primary raw image)
        let raw_ifd_off = target_ifds[0];
        file.seek(SeekFrom::Start(raw_ifd_off as u64))?;
        file.read_exact(&mut num_tags_buf)?;
        let s_tags = endian.read_u16(&num_tags_buf) as usize;

        let mut width = 0usize;
        let mut height = 0usize;
        let mut bits_per_sample = 16u32;
        let mut photometric_interpretation = 32803u32;
        let mut strip_offsets = Vec::new();
        let mut samples_per_pixel = 1u32;
        let mut strip_byte_counts = Vec::new();
        let mut cfa_pattern = vec![0u8, 1, 1, 2];
        let mut cfa_repeat_dim = [2u16, 2u16];
        let mut black_level = 1024.0f64;
        let mut white_level = 16383u32;
        let mut crop_x = 0usize;
        let mut crop_y = 0usize;
        let mut crop_w = 0usize;
        let mut crop_h = 0usize;
        let mut active_top = 0usize;
        let mut active_left = 0usize;
        let mut active_bottom = 0usize;
        let mut active_right = 0usize;

        for _ in 0..s_tags {
            let mut entry = [0u8; 12];
            file.read_exact(&mut entry)?;
            let stag = endian.read_u16(&entry[0..2]);
            let sdtype = endian.read_u16(&entry[2..4]);
            let scount = endian.read_u32(&entry[4..8]) as usize;
            let sval = endian.read_u32(&entry[8..12]);

            let val = read_tiff_tag_value(&mut file, endian, sdtype, scount, sval)?;
            if val.is_empty() {
                continue;
            }

            match stag {
                0x0100 => width = val[0] as usize,
                0x0101 => height = val[0] as usize,
                0x0102 => bits_per_sample = val[0] as u32,
                0x0106 => photometric_interpretation = val[0] as u32,
                0x0111 => strip_offsets = val.into_iter().map(|v| v as u32).collect(),
                0x0115 => samples_per_pixel = val[0] as u32,
                0x0117 => strip_byte_counts = val.into_iter().map(|v| v as u32).collect(),
                0x828e => cfa_pattern = val.into_iter().map(|v| v as u8).collect(),
                0xc68d => {
                    // CFAPattern2 or ActiveArea (dnglab stores ActiveArea here)
                    if val.len() == 4 && val.iter().any(|&v| v > 3.0) {
                        active_top = val[0] as usize;
                        active_left = val[1] as usize;
                        active_bottom = val[2] as usize;
                        active_right = val[3] as usize;
                    } else if val.len() <= 4 && cfa_pattern == [0, 1, 1, 2] {
                        cfa_pattern = val.into_iter().map(|v| v as u8).collect();
                    }
                }
                0xc62d => {
                    // ActiveArea (standard DNG tag)
                    if val.len() >= 4 {
                        active_top = val[0] as usize;
                        active_left = val[1] as usize;
                        active_bottom = val[2] as usize;
                        active_right = val[3] as usize;
                    }
                }
                0x828d => {
                    if val.len() >= 2 {
                        cfa_repeat_dim = [val[0] as u16, val[1] as u16];
                    }
                }
                0xc61a => {
                    // BlackLevel (can be rational or multi-channel)
                    let sum: f64 = val.iter().sum();
                    black_level = sum / val.len() as f64;
                }
                0xc61d => white_level = val[0] as u32,
                0xc61f => {
                    if val.len() >= 2 {
                        crop_x = val[0] as usize;
                        crop_y = val[1] as usize;
                    }
                }
                0xc620 => {
                    if val.len() >= 2 {
                        crop_w = val[0] as usize;
                        crop_h = val[1] as usize;
                    }
                }
                _ => {}
            }
        }

        let raw_pixel_length: usize = strip_byte_counts.iter().map(|&c| c as usize).sum();

        if crop_w == 0 || crop_h == 0 {
            crop_w = width;
            crop_h = height;
        }

        Ok(Self {
            filepath,
            endian,
            width,
            height,
            crop_x,
            crop_y,
            crop_w,
            crop_h,
            bits_per_sample,
            samples_per_pixel,
            photometric_interpretation,
            cfa_pattern,
            cfa_repeat_dim,
            black_level,
            white_level,
            as_shot_neutral,
            strip_offsets,
            strip_byte_counts,
            raw_pixel_length,
            active_top,
            active_left,
            active_bottom,
            active_right,
        })
    }

    /// Reads raw pixel data from strips, remosaicing 3-channel Linear RGB (iPhone ProRAW) if needed.
    /// Returns 16-bit pixel samples as `Vec<u16>`.
    pub fn read_raw_pixels(&self) -> Result<Vec<u16>> {
        let mut file = File::open(&self.filepath)?;
        let mut raw_bytes = Vec::with_capacity(self.raw_pixel_length);

        for (&off, &cnt) in self.strip_offsets.iter().zip(&self.strip_byte_counts) {
            file.seek(SeekFrom::Start(off as u64))?;
            let mut strip = vec![0u8; cnt as usize];
            file.read_exact(&mut strip)?;
            raw_bytes.extend_from_slice(&strip);
        }

        // Convert raw_bytes to u16 slice according to DNG endianness
        let num_u16 = raw_bytes.len() / 2;
        let mut u16_pixels = Vec::with_capacity(num_u16);
        for chunk in raw_bytes.chunks_exact(2) {
            u16_pixels.push(self.endian.read_u16(chunk));
        }

        // Check if 3-channel Linear Raw
        if self.samples_per_pixel == 3 || self.photometric_interpretation == 34892 {
            Ok(remosaic_rgb_to_bayer(&u16_pixels, self.width, self.height))
        } else {
            Ok(u16_pixels)
        }
    }
}
