//! Repacks DNG Bayer pixels and metadata into Fujifilm RAF container.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use anyhow::{anyhow, Context, Result};
use rayon::prelude::*;

use crate::jpeg::{splice_preview_jpeg, MINIMAL_BLANK_JPEG};
use crate::template::GfxTemplate;
use crate::tiff::DngInfo;

#[derive(Debug, Clone)]
pub struct RawSourceInfo {
    pub width: usize,
    pub height: usize,
    pub crop_x: usize,
    pub crop_y: usize,
    pub crop_w: usize,
    pub crop_h: usize,
    pub active_top: usize,
    pub active_left: usize,
    pub cfa_pattern: [u8; 4],
    pub black_level: f64,
    pub white_level: f64,
    pub bits_per_sample: u32,
    pub r_mul: f64,
    pub b_mul: f64,
    pub is_linear_raw: bool,
}

impl RawSourceInfo {
    pub fn from_rawler(raw: &rawler::RawImage) -> Result<Self> {
        let (active_top, active_left) = if let Some(act) = raw.active_area {
            (act.p.y, act.p.x)
        } else {
            (0, 0)
        };

        let (crop_x, crop_y, crop_w, crop_h) = if let Some(crop) = raw.crop_area {
            (crop.p.x, crop.p.y, crop.d.w, crop.d.h)
        } else if let Some(act) = raw.active_area {
            (act.p.x, act.p.y, act.d.w, act.d.h)
        } else {
            (0, 0, raw.width, raw.height)
        };

        let is_linear_raw = matches!(raw.photometric, rawler::rawimage::RawPhotometricInterpretation::LinearRaw) || raw.cpp == 3;

        let cfa_pattern = if is_linear_raw {
            [0, 1, 1, 2] // Linear RAW remosaiced to standard RGGB
        } else if raw.camera.cfa.width > 0 && raw.camera.cfa.height > 0 {
            [
                raw.camera.cfa.color_at(0, 0) as u8,
                raw.camera.cfa.color_at(0, 1) as u8,
                raw.camera.cfa.color_at(1, 0) as u8,
                raw.camera.cfa.color_at(1, 1) as u8,
            ]
        } else {
            [0, 1, 1, 2] // standard RGGB fallback
        };

        let bl_vec = raw.blacklevel.as_vec();
        let black_level = if !bl_vec.is_empty() {
            bl_vec.iter().copied().fold(0.0, |a, b| a + b as f64) / bl_vec.len() as f64
        } else {
            0.0
        };

        let white_level = raw.whitelevel.0.first().copied().unwrap_or(16383) as f64;

        let bits_per_sample = if white_level > 16383.0 {
            16
        } else if white_level > 4095.0 {
            14
        } else {
            12
        };

        let (r_mul, b_mul) = if raw.wb_coeffs[0].is_finite() && raw.wb_coeffs[0] > 0.0 && raw.wb_coeffs[1] > 0.0 {
            (
                (raw.wb_coeffs[0] / raw.wb_coeffs[1]) as f64,
                (raw.wb_coeffs[2] / raw.wb_coeffs[1]) as f64,
            )
        } else if raw.clean_model.contains("F828") {
            (1.55078125, 2.84375) // Sony DSC-F828 daylight WB default
        } else {
            (1.0, 1.0)
        };

        Ok(Self {
            width: raw.width,
            height: raw.height,
            crop_x,
            crop_y,
            crop_w,
            crop_h,
            active_top,
            active_left,
            cfa_pattern,
            black_level,
            white_level,
            bits_per_sample,
            r_mul,
            b_mul,
            is_linear_raw,
        })
    }

    #[allow(dead_code)]
    pub fn from_dng(dng: &DngInfo) -> Self {
        let rm = if dng.as_shot_neutral[0] != 0.0 { dng.as_shot_neutral[1] / dng.as_shot_neutral[0] } else { 1.0 };
        let bm = if dng.as_shot_neutral[2] != 0.0 { dng.as_shot_neutral[1] / dng.as_shot_neutral[2] } else { 1.0 };
        let (r_mul, b_mul) = (rm, bm);

        let mut cfa_pattern = [0u8; 4];
        if dng.cfa_pattern.len() >= 4 {
            cfa_pattern.copy_from_slice(&dng.cfa_pattern[..4]);
        }

            Self {
                width: dng.width,
                height: dng.height,
                crop_x: dng.crop_x,
                crop_y: dng.crop_y,
                crop_w: dng.crop_w,
                crop_h: dng.crop_h,
                active_top: dng.active_top,
                active_left: dng.active_left,
                cfa_pattern,
                black_level: dng.black_level,
                white_level: dng.white_level as f64,
                bits_per_sample: dng.bits_per_sample as u32,
                r_mul,
                b_mul,
                is_linear_raw: dng.samples_per_pixel == 3 || dng.photometric_interpretation == 34892,
            }
    }
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// Packs 14-bit pixel values into 32-bit byte-reversed (MSB32) big-endian bitstream.
/// Matches Fujifilm hardware decompress_14be_msb32 format (16 pixels in 28 bytes).
pub fn pack_14be_msb32(pixels: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity((pixels.len() * 14).div_ceil(8));
    for p in pixels.chunks_exact(16) {
        let w0: u32 = ((p[0] as u32 & 0x3fff) << 18)
            | ((p[1] as u32 & 0x3fff) << 4)
            | ((p[2] as u32 & 0x3fff) >> 10);
        let w1: u32 = (((p[2] as u32 & 0x3fff) & 0x3ff) << 22)
            | ((p[3] as u32 & 0x3fff) << 8)
            | ((p[4] as u32 & 0x3fff) >> 6);
        let w2: u32 = (((p[4] as u32 & 0x3fff) & 0x3f) << 26)
            | ((p[5] as u32 & 0x3fff) << 12)
            | ((p[6] as u32 & 0x3fff) >> 2);
        let w3: u32 = (((p[6] as u32 & 0x3fff) & 0x3) << 30)
            | ((p[7] as u32 & 0x3fff) << 16)
            | ((p[8] as u32 & 0x3fff) << 2)
            | ((p[9] as u32 & 0x3fff) >> 12);
        let w4: u32 = (((p[9] as u32 & 0x3fff) & 0xfff) << 20)
            | ((p[10] as u32 & 0x3fff) << 6)
            | ((p[11] as u32 & 0x3fff) >> 8);
        let w5: u32 = (((p[11] as u32 & 0x3fff) & 0xff) << 24)
            | ((p[12] as u32 & 0x3fff) << 10)
            | ((p[13] as u32 & 0x3fff) >> 4);
        let w6: u32 = (((p[13] as u32 & 0x3fff) & 0xf) << 28)
            | ((p[14] as u32 & 0x3fff) << 14)
            | (p[15] as u32 & 0x3fff);

        out.extend_from_slice(&w0.to_le_bytes());
        out.extend_from_slice(&w1.to_le_bytes());
        out.extend_from_slice(&w2.to_le_bytes());
        out.extend_from_slice(&w3.to_le_bytes());
        out.extend_from_slice(&w4.to_le_bytes());
        out.extend_from_slice(&w5.to_le_bytes());
        out.extend_from_slice(&w6.to_le_bytes());
    }
    out
}

/// Packs 12-bit pixel values into little-endian 12-bit packed bitstream (2 pixels in 3 bytes).
/// Matches Fujifilm hardware decompress_12le format.
pub fn pack_12le(pixels: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity((pixels.len() * 12).div_ceil(8));
    for p in pixels.chunks_exact(2) {
        let b0 = (p[0] & 0xff) as u8;
        let b1 = (((p[0] >> 8) & 0x0f) | ((p[1] & 0x0f) << 4)) as u8;
        let b2 = ((p[1] >> 4) & 0xff) as u8;
        out.push(b0);
        out.push(b1);
        out.push(b2);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepackMode {
    Native,
    Canvas,
    Unpadded,
}

impl std::str::FromStr for RepackMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "native" => Ok(RepackMode::Native),
            "canvas" => Ok(RepackMode::Canvas),
            "unpadded" => Ok(RepackMode::Unpadded),
            _ => Err(anyhow!("Unknown mode: {}. Choose 'native', 'canvas', or 'unpadded'.", s)),
        }
    }
}

pub struct RepackOptions<'a> {
    pub mode: RepackMode,
    pub exposure_bias: f64,
    pub preview_data: Option<&'a [u8]>,
    pub empty_preview: bool,
    pub custom_model: Option<&'a str>,
}

pub fn repack<P: AsRef<Path>>(
    raw_info: &RawSourceInfo,
    src_pixels: &[u16],
    template: &GfxTemplate,
    output_path: P,
    opts: &RepackOptions,
) -> Result<()> {
    let mut hdr = template.header.clone();
    let mut meta_data = template.meta_data.clone();
    let mut raw_head = template.raw_head.clone();

    // Step 1: Prepare preview JPEG
    let replacement_jpeg = if opts.empty_preview || opts.preview_data.is_none() {
        MINIMAL_BLANK_JPEG
    } else {
        opts.preview_data.unwrap()
    };

    let mut jpeg_data = splice_preview_jpeg(&template.jpeg_data, replacement_jpeg)
        .context("Splicing preview JPEG")?;

    // Step 2: Calculate White Balance multipliers
    let (r_mul, b_mul) = (raw_info.r_mul, raw_info.b_mul);

    let base_g = if let Some(off) = template.subifd_offsets.wb_auto_offset {
        if off + 4 <= template.raw_head.len() {
            let g = u32::from_le_bytes([
                template.raw_head[off],
                template.raw_head[off + 1],
                template.raw_head[off + 2],
                template.raw_head[off + 3],
            ]);
            if g > 0 && g <= 1024 { g } else { 302 }
        } else {
            302
        }
    } else {
        302
    };
    let r_val: u32 = (base_g as f64 * r_mul).round() as u32;
    let b_val: u32 = (base_g as f64 * b_mul).round() as u32;

    let mut wb_bytes = [0u8; 12];
    wb_bytes[0..4].copy_from_slice(&base_g.to_le_bytes());
    wb_bytes[4..8].copy_from_slice(&r_val.to_le_bytes());
    wb_bytes[8..12].copy_from_slice(&b_val.to_le_bytes());

    // Standard daylight (D65) and tungsten (A) reference levels in Tag 0xf00c
    let r_d65: u32 = (base_g as f64 * (2636.0 / 1024.0)).round() as u32;
    let b_d65: u32 = (base_g as f64 * (1680.0 / 1024.0)).round() as u32;
    let r_a: u32 = (base_g as f64 * (1584.0 / 1024.0)).round() as u32;
    let b_a: u32 = (base_g as f64 * (3020.0 / 1024.0)).round() as u32;

    let mut std_wb_bytes = [0u8; 32];
    std_wb_bytes[0..4].copy_from_slice(&base_g.to_le_bytes());
    std_wb_bytes[4..8].copy_from_slice(&r_a.to_le_bytes());
    std_wb_bytes[8..12].copy_from_slice(&b_a.to_le_bytes());
    std_wb_bytes[12..16].copy_from_slice(&17u32.to_le_bytes());
    std_wb_bytes[16..20].copy_from_slice(&base_g.to_le_bytes());
    std_wb_bytes[20..24].copy_from_slice(&r_d65.to_le_bytes());
    std_wb_bytes[24..28].copy_from_slice(&b_d65.to_le_bytes());
    std_wb_bytes[28..32].copy_from_slice(&21u32.to_le_bytes());

    // Update SubIFD White Balance in raw_head dynamically
    if let Some(off) = template.subifd_offsets.wb_std_offset {
        if off + 32 <= raw_head.len() {
            raw_head[off..off + 32].copy_from_slice(&std_wb_bytes);
        }
    }

    if let Some(off) = template.subifd_offsets.wb_auto_offset {
        if off + 12 <= raw_head.len() {
            raw_head[off..off + 12].copy_from_slice(&wb_bytes);
        }
    }

    if let Some(off) = template.subifd_offsets.wb_as_shot_offset {
        if off + 12 <= raw_head.len() {
            raw_head[off..off + 12].copy_from_slice(&wb_bytes);
        }
    }

    if let Some(off) = template.subifd_offsets.wb_auto_copy1_offset {
        if off + 12 <= raw_head.len() {
            raw_head[off..off + 12].copy_from_slice(&wb_bytes);
        }
    }

    if let Some(off) = template.subifd_offsets.wb_auto_copy2_offset {
        if off + 12 <= raw_head.len() {
            raw_head[off..off + 12].copy_from_slice(&wb_bytes);
        }
    }

    // Update embedded JPEG Makernotes WB tags (0x144a, 0x144b, 0x144c)
    let pattern_le = b"\x4a\x14\x03\x00\x01\x00\x00\x00";
    if let Some(wb_pos) = jpeg_data.windows(pattern_le.len()).position(|w| w == pattern_le) {
        if wb_pos + 34 <= jpeg_data.len() {
            jpeg_data[wb_pos + 8..wb_pos + 10].copy_from_slice(&(r_val as u16).to_le_bytes());
            if &jpeg_data[wb_pos + 12..wb_pos + 20] == b"\x4b\x14\x03\x00\x01\x00\x00\x00" {
                jpeg_data[wb_pos + 20..wb_pos + 22].copy_from_slice(&(base_g as u16).to_le_bytes());
            }
            if &jpeg_data[wb_pos + 24..wb_pos + 32] == b"\x4c\x14\x03\x00\x01\x00\x00\x00" {
                jpeg_data[wb_pos + 32..wb_pos + 34].copy_from_slice(&(b_val as u16).to_le_bytes());
            }
        }
    }

    // Step 3: CFA Bayer phase alignment
    let mut shift_x = 0usize;
    let mut shift_y = 0usize;
    if raw_info.cfa_pattern == [1, 0, 2, 1] {
        // GRBG -> shift x by 1 to make RGGB
        shift_x = 1;
        shift_y = 0;
    } else if raw_info.cfa_pattern == [1, 2, 0, 1] {
        // GBRG -> shift y by 1 to make RGGB
        shift_x = 0;
        shift_y = 1;
    } else if raw_info.cfa_pattern == [2, 1, 1, 0] {
        // BGGR -> shift x by 1, y by 1 to make RGGB
        shift_x = 1;
        shift_y = 1;
    } else if raw_info.cfa_pattern == [3, 0, 2, 1] {
        // ERBG (Sony DSC-F828): Emerald=3, Red=0, Blue=2, Green=1
        // Shift x by 1 gives:
        // Row 0: Red, Emerald(Green1)
        // Row 1: Green2, Blue
        // which perfectly matches standard RGGB Bayer layout!
        shift_x = 1;
        shift_y = 0;
    }

    let aa_top = if raw_info.active_top > 0 { raw_info.active_top } else { 0 };
    let aa_left = if raw_info.active_left > 0 { raw_info.active_left } else { 0 };

    let crop_start_y = aa_top.max(raw_info.crop_y);
    let crop_start_x = aa_left.max(raw_info.crop_x);

    let mut start_x = crop_start_x + shift_x;
    let mut start_y = crop_start_y + shift_y;

    let is_standard_bayer = raw_info.cfa_pattern.iter().all(|&v| v <= 3);
    if is_standard_bayer {
        // Normalize start_y to even
        if start_y % 2 != 0 {
            start_y += 1;
        }
        // Normalize start_x parity
        if shift_x == 0 && (start_x % 2 != 0) {
            start_x += 1;
        } else if shift_x == 1 && (start_x % 2 == 0) {
            start_x += 1;
        }
    }

    // Target container parameters (dynamically detected from template)
    let target_w = template.width;
    let target_h = template.height;
    let target_bps = template.bits_per_sample;
    let target_black = template.black_level;

    let max_src_w = raw_info.width.saturating_sub(start_x);
    let max_src_h = raw_info.height.saturating_sub(start_y);

    let act_w = ((raw_info.crop_w.min(target_w.saturating_sub(start_x)).min(max_src_w)) / 2) * 2;
    let act_h = ((raw_info.crop_h.min(target_h.saturating_sub(start_y)).min(max_src_h)) / 2) * 2;

    // Step 4: Dynamic Range & Bit-Depth Normalization LUT (clean linear mapping preserving full highlight headroom)
    let usable_src_range = (raw_info.white_level - raw_info.black_level).max(1.0);
    let white_saturation = if target_bps == 16 {
        65535.0
    } else if target_bps == 12 {
        4095.0
    } else {
        16383.0
    };
    let scale = (white_saturation - target_black as f64) / usable_src_range;
    let mut lut = [0u16; 65536];
    for (x, slot) in lut.iter_mut().enumerate() {
        let scaled = target_black as f64 + (x as f64 - raw_info.black_level).max(0.0) * scale;
        *slot = scaled.round().clamp(target_black as f64, white_saturation) as u16;
    }

    let bias_num = (opts.exposure_bias * 100.0).round() as i16;
    let bias_den = 100u16;

    // Step 5: Layout according to mode
    let raw_payload: Vec<u8> = match opts.mode {
        RepackMode::Canvas | RepackMode::Native => {
            let pad_x = if act_w < target_w { ((target_w - act_w) / 4) * 2 } else { 0 };
            let pad_y = if act_h < target_h { ((target_h - act_h) / 4) * 2 } else { 0 };

            // Initialize canvas buffer pre-filled with target container's black level
            let mut canvas = vec![target_black; target_w * target_h];

            // Parallel render active rows into canvas using Rayon
            canvas
                .par_chunks_exact_mut(target_w)
                .enumerate()
                .skip(pad_y)
                .take(act_h)
                .for_each(|(y_dst, dst_row)| {
                    let r = y_dst - pad_y;
                    let y_src = start_y + r;
                    if y_src < raw_info.height {
                        let src_stride = raw_info.width;
                        let src_start = y_src * src_stride + start_x;
                        let src_end = (src_start + act_w).min(src_pixels.len());
                        if src_start < src_end {
                            let len = src_end - src_start;
                            let src_slice = &src_pixels[src_start..src_end];
                            let dst_slice = &mut dst_row[pad_x..pad_x + len];
                            for (dst_val, &src_val) in dst_slice.iter_mut().zip(src_slice) {
                                *dst_val = lut[src_val as usize];
                            }
                        }
                    }
                });

            // Update BlackLevel in raw_head dynamically
            if let Some(off) = template.subifd_offsets.black_level_offset {
                if off + 16 <= raw_head.len() {
                    for i in 0..4 {
                        let pos = off + i * 4;
                        raw_head[pos..pos + 4].copy_from_slice(&(target_black as u32).to_le_bytes());
                    }
                }
            }

            let (crop_top, crop_left, crop_w, crop_h, aspect_w, aspect_h) = if opts.mode == RepackMode::Native {
                let ct = pad_y;
                let cl = pad_x;
                let cw = act_w;
                let ch = act_h;
                let g = gcd(cw, ch);
                let mut aw = cw / g;
                let mut ah = ch / g;
                if aw > 100 || ah > 100 {
                    let ratio = cw as f64 / ch as f64;
                    if (ratio - 1.5).abs() < 0.05 {
                        aw = 3;
                        ah = 2;
                    } else if (ratio - 4.0 / 3.0).abs() < 0.05 {
                        aw = 4;
                        ah = 3;
                    } else if (ratio - 16.0 / 9.0).abs() < 0.05 {
                        aw = 16;
                        ah = 9;
                    } else if (ratio - 1.0).abs() < 0.05 {
                        aw = 1;
                        ah = 1;
                    }
                }
                (ct, cl, cw, ch, aw, ah)
            } else {
                (
                    template.default_crop_top,
                    template.default_crop_left,
                    template.default_crop_w,
                    template.default_crop_h,
                    4,
                    3,
                )
            };

            // Update CFA Meta Directory entries
            let mut curr = 4;
            if meta_data.len() >= 4 {
                let num_entries = u32::from_be_bytes([meta_data[0], meta_data[1], meta_data[2], meta_data[3]]) as usize;
                for _ in 0..num_entries {
                    if curr + 4 > meta_data.len() {
                        break;
                    }
                    let tag = u16::from_be_bytes([meta_data[curr], meta_data[curr + 1]]);
                    let length = u16::from_be_bytes([meta_data[curr + 2], meta_data[curr + 3]]) as usize;
                    let doff = curr + 4;
                    if doff + length > meta_data.len() {
                        break;
                    }

                    match tag {
                        0x0100 => {
                            // Full size (Height, Width)
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(target_h as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&(target_w as u16).to_be_bytes());
                            }
                        }
                        0x0110 => {
                            // Crop origin (Top, Left)
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(crop_top as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&(crop_left as u16).to_be_bytes());
                            }
                        }
                        0x0111 | 0x0113 => {
                            // Crop size (H, W)
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(crop_h as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&(crop_w as u16).to_be_bytes());
                            }
                        }
                        0x0112 => {
                            // Dimensions (W, H)
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(crop_w as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&(crop_h as u16).to_be_bytes());
                            }
                        }
                        0x0115 => {
                            // Aspect ratio (Height, Width)
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(aspect_h as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&(aspect_w as u16).to_be_bytes());
                            }
                        }
                        0x0141 => {
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(target_bps as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&((target_bps * 3) as u16).to_be_bytes());
                            }
                        }
                        0x9650 | 0x9651 => {
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&bias_num.to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&bias_den.to_be_bytes());
                            }
                        }
                        _ => {}
                    }
                    curr += 4 + length;
                }
            }

            let is_target_gfx = if let Some(m) = opts.custom_model {
                m.to_uppercase().contains("GFX")
            } else {
                !template.is_non_gfx()
            };

            // Convert canvas to packed or uncompressed bytes based on container template format
            let payload = if target_bps == 12 && !is_target_gfx {
                pack_12le(&canvas)
            } else if target_bps == 14 && !is_target_gfx {
                pack_14be_msb32(&canvas)
            } else {
                let mut p = Vec::with_capacity(canvas.len() * 2);
                for val in &canvas {
                    p.extend_from_slice(&val.to_le_bytes());
                }
                p
            };
            payload
        }

        RepackMode::Unpadded => {
            let target_w = raw_info.width;
            let target_h = raw_info.height;

            let mut scaled_pixels = Vec::with_capacity(src_pixels.len());
            for &p in src_pixels {
                scaled_pixels.push(lut[p as usize]);
            }

            let crop_w = raw_info.crop_w;
            let crop_h = raw_info.crop_h;
            let top_margin = (target_h.saturating_sub(crop_h)) / 2;
            let left_margin = (target_w.saturating_sub(crop_w)) / 2;

            let mut curr = 4;
            if meta_data.len() >= 4 {
                let num_entries = u32::from_be_bytes([meta_data[0], meta_data[1], meta_data[2], meta_data[3]]) as usize;
                for _ in 0..num_entries {
                    if curr + 4 > meta_data.len() {
                        break;
                    }
                    let tag = u16::from_be_bytes([meta_data[curr], meta_data[curr + 1]]);
                    let length = u16::from_be_bytes([meta_data[curr + 2], meta_data[curr + 3]]) as usize;
                    let doff = curr + 4;
                    if doff + length > meta_data.len() {
                        break;
                    }

                    match tag {
                        0x0100 => {
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(target_h as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&(target_w as u16).to_be_bytes());
                            }
                        }
                        0x0110 => {
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(top_margin as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&(left_margin as u16).to_be_bytes());
                            }
                        }
                        0x0111 | 0x0113 => {
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(crop_h as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&(crop_w as u16).to_be_bytes());
                            }
                        }
                        0x0112 => {
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(crop_w as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&(crop_h as u16).to_be_bytes());
                            }
                        }
                        0x0115 => {
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&2u16.to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&3u16.to_be_bytes());
                            }
                        }
                        0x0141 => {
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&(target_bps as u16).to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&((target_bps * 3) as u16).to_be_bytes());
                            }
                        }
                        0x9650 | 0x9651 => {
                            if length >= 4 {
                                meta_data[doff..doff + 2].copy_from_slice(&bias_num.to_be_bytes());
                                meta_data[doff + 2..doff + 4].copy_from_slice(&bias_den.to_be_bytes());
                            }
                        }
                        _ => {}
                    }
                    curr += 4 + length;
                }
            }

            if raw_head.len() >= 52 {
                raw_head[36..40].copy_from_slice(&(target_w as u32).to_le_bytes());
                raw_head[48..52].copy_from_slice(&(target_h as u32).to_le_bytes());
            }
            if let Some(off) = template.subifd_offsets.black_level_offset {
                if off + 16 <= raw_head.len() {
                    for i in 0..4 {
                        let pos = off + i * 4;
                        raw_head[pos..pos + 4].copy_from_slice(&(target_black as u32).to_le_bytes());
                    }
                }
            } else if raw_head.len() >= 288 {
                for i in 0..4 {
                    let pos = 272 + i * 4;
                    raw_head[pos..pos + 4].copy_from_slice(&(target_black as u32).to_le_bytes());
                }
            }

            let is_target_gfx = if let Some(m) = opts.custom_model {
                m.to_uppercase().contains("GFX")
            } else {
                !template.is_non_gfx()
            };

            let payload = if target_bps == 12 && !is_target_gfx {
                pack_12le(&scaled_pixels)
            } else if target_bps == 14 && !is_target_gfx {
                pack_14be_msb32(&scaled_pixels)
            } else {
                let mut p = Vec::with_capacity(scaled_pixels.len() * 2);
                for val in &scaled_pixels {
                    p.extend_from_slice(&val.to_le_bytes());
                }
                p
            };
            payload
        }
    };

    // Set ImageWidth and ImageHeight in raw_head
    if raw_head.len() >= 52 {
        raw_head[36..40].copy_from_slice(&(target_w as u32).to_le_bytes());
        raw_head[48..52].copy_from_slice(&(target_h as u32).to_le_bytes());
    }

    // Set BitsPerSample (tag 0xf003 at offset 60)
    if raw_head.len() >= 64 {
        raw_head[60..64].copy_from_slice(&(target_bps as u32).to_le_bytes());
    }

    // Set uncompressed / packed StripByteCounts in SubIFD (tag 0xf008 at offset 120 or template offset)
    let uncompressed_size = raw_payload.len();
    if let Some(off) = template.subifd_offsets.strip_byte_counts_offset {
        if off + 4 <= raw_head.len() {
            raw_head[off..off + 4].copy_from_slice(&(uncompressed_size as u32).to_le_bytes());
        }
    } else if raw_head.len() >= 124 {
        raw_head[120..124].copy_from_slice(&(uncompressed_size as u32).to_le_bytes());
    }

    // Dynamic layout calculation
    let jpeg_off = 148usize;
    let jpeg_len = jpeg_data.len();
    let meta_off = ((jpeg_off + jpeg_len + 3) / 4) * 4;
    let gap1 = meta_off - (jpeg_off + jpeg_len);
    let meta_len = meta_data.len();
    let raw_off = ((meta_off + meta_len + 2047) / 2048) * 2048;
    let gap2 = raw_off - (meta_off + meta_len);
    let raw_len = uncompressed_size;

    // Update RAF header offsets
    hdr[0x54..0x58].copy_from_slice(&(jpeg_off as u32).to_be_bytes());
    hdr[0x58..0x5c].copy_from_slice(&(jpeg_len as u32).to_be_bytes());
    hdr[0x5c..0x60].copy_from_slice(&(meta_off as u32).to_be_bytes());
    hdr[0x60..0x64].copy_from_slice(&(meta_len as u32).to_be_bytes());
    hdr[0x64..0x68].copy_from_slice(&(raw_off as u32).to_be_bytes());
    hdr[0x68..0x6c].copy_from_slice(&(raw_len as u32).to_be_bytes());
    hdr[0x6c..0x70].copy_from_slice(&0u32.to_be_bytes()); // RAFCompression: 0 = uncompressed
    hdr[0x70..0x74].copy_from_slice(&(raw_len as u32).to_be_bytes());

    // If user specified custom model, update RAF header model string at 0x1c..0x3c
    if let Some(model) = opts.custom_model {
        let mut model_buf = [0u8; 32];
        let bytes = model.as_bytes();
        let len = bytes.len().min(31);
        model_buf[..len].copy_from_slice(&bytes[..len]);
        hdr[0x1c..0x3c].copy_from_slice(&model_buf);
    }

    // Write output RAF file using buffered I/O
    let out_file = File::create(output_path.as_ref())
        .with_context(|| format!("Creating output RAF file: {:?}", output_path.as_ref()))?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, out_file);

    writer.write_all(&hdr)?;
    writer.write_all(&jpeg_data)?;
    if gap1 > 0 {
        writer.write_all(&vec![0u8; gap1])?;
    }
    writer.write_all(&meta_data)?;
    if gap2 > 0 {
        writer.write_all(&vec![0u8; gap2])?;
    }
    writer.write_all(&raw_head)?;
    writer.write_all(&raw_payload)?;
    writer.flush()?;

    Ok(())
}
