//! Universal Fujifilm RAF template loader with dynamic metadata extraction.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone, Default)]
pub struct SubIfdTagOffsets {
    pub black_level_offset: Option<usize>,
    pub strip_byte_counts_offset: Option<usize>,
    pub wb_std_offset: Option<usize>,
    pub wb_auto_offset: Option<usize>,
    pub wb_as_shot_offset: Option<usize>,
    pub wb_auto_copy1_offset: Option<usize>,
    pub wb_auto_copy2_offset: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct GfxTemplate {
    pub source_path: std::path::PathBuf,
    pub is_uncompressed: bool,
    pub header: Vec<u8>,
    pub jpeg_data: Vec<u8>,
    pub meta_data: Vec<u8>,
    pub raw_head: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub default_crop_w: usize,
    pub default_crop_h: usize,
    pub default_crop_top: usize,
    pub default_crop_left: usize,
    pub black_level: u16,
    pub bits_per_sample: u32,
    pub subifd_offsets: SubIfdTagOffsets,
    pub camera_model: String,
}

/// Normalize camera model names to standard Fujifilm canonical representations when recognized.
pub fn normalize_camera_model(input: &str) -> String {
    let s = input.trim();
    let lower_clean: String = s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();

    match lower_clean.as_str() {
        "gfx50sii" | "gfx50s2" => "GFX50S II".to_string(),
        "gfx50s" => "GFX 50S".to_string(),
        "gfx50r" => "GFX 50R".to_string(),
        "gfx100sii" | "gfx100s2" => "GFX100S II".to_string(),
        "gfx100s" => "GFX100S".to_string(),
        "gfx100ii" | "gfx1002" => "GFX 100 II".to_string(),
        "gfx100" => "GFX 100".to_string(),
        _ => s.to_string(),
    }
}

impl GfxTemplate {
    pub fn is_non_gfx(&self) -> bool {
        !self.camera_model.to_uppercase().contains("GFX")
    }

    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let mut file = File::open(path_ref)
            .with_context(|| format!("Failed to open template RAF file: {:?}", path_ref))?;

        let mut header = vec![0u8; 148];
        file.read_exact(&mut header)
            .context("Reading 148-byte RAF header from template")?;

        if &header[0..16] != b"FUJIFILMCCD-RAW " {
            return Err(anyhow!("Template file is not a valid Fujifilm RAF file"));
        }

        let jpeg_off = u32::from_be_bytes([header[0x54], header[0x55], header[0x56], header[0x57]]) as u64;
        let jpeg_len = u32::from_be_bytes([header[0x58], header[0x59], header[0x5a], header[0x5b]]) as usize;
        let meta_off = u32::from_be_bytes([header[0x5c], header[0x5d], header[0x5e], header[0x5f]]) as u64;
        let meta_len = u32::from_be_bytes([header[0x60], header[0x61], header[0x62], header[0x63]]) as usize;
        let raw_off = u32::from_be_bytes([header[0x64], header[0x65], header[0x66], header[0x67]]) as u64;

        file.seek(SeekFrom::Start(jpeg_off))
            .context("Seeking to template JPEG preview")?;
        let mut jpeg_data = vec![0u8; jpeg_len];
        file.read_exact(&mut jpeg_data)
            .context("Reading template JPEG preview")?;

        file.seek(SeekFrom::Start(meta_off))
            .context("Seeking to template CFA meta directory")?;
        let mut meta_data = vec![0u8; meta_len];
        file.read_exact(&mut meta_data)
            .context("Reading template CFA meta directory")?;

        file.seek(SeekFrom::Start(raw_off))
            .context("Seeking to template raw TIFF header")?;
        let mut raw_head = vec![0u8; 2048];
        file.read_exact(&mut raw_head)
            .context("Reading template raw TIFF header (2048 bytes)")?;

        // Extract camera model from header string at offset 0x1c..0x3c
        let raw_model = &header[0x1c..0x3c.min(header.len())];
        let camera_model = String::from_utf8_lossy(raw_model)
            .trim_matches('\0')
            .trim()
            .to_string();

        // Parse CFA Meta Directory tags
        let mut meta_w = 0usize;
        let mut meta_h = 0usize;
        let mut default_crop_top = 7usize;
        let mut default_crop_left = 8usize;
        let mut default_crop_w = 0usize;
        let mut default_crop_h = 0usize;
        let mut bits_per_sample = 14u32;

        if meta_data.len() >= 4 {
            let num_entries = u32::from_be_bytes([meta_data[0], meta_data[1], meta_data[2], meta_data[3]]) as usize;
            let mut curr = 4;
            for _ in 0..num_entries {
                if curr + 4 > meta_data.len() {
                    break;
                }
                let tag = u16::from_be_bytes([meta_data[curr], meta_data[curr + 1]]);
                let len = u16::from_be_bytes([meta_data[curr + 2], meta_data[curr + 3]]) as usize;
                let doff = curr + 4;
                if doff + len > meta_data.len() {
                    break;
                }
                let val_bytes = &meta_data[doff..doff + len];
                match tag {
                    0x0100 => {
                        // Full size (Height, Width)
                        if val_bytes.len() >= 4 {
                            meta_h = u16::from_be_bytes([val_bytes[0], val_bytes[1]]) as usize;
                            meta_w = u16::from_be_bytes([val_bytes[2], val_bytes[3]]) as usize;
                        }
                    }
                    0x0110 => {
                        // Crop origin (Top, Left)
                        if val_bytes.len() >= 4 {
                            default_crop_top = u16::from_be_bytes([val_bytes[0], val_bytes[1]]) as usize;
                            default_crop_left = u16::from_be_bytes([val_bytes[2], val_bytes[3]]) as usize;
                        }
                    }
                    0x0111 => {
                        // Crop size (Height, Width)
                        if val_bytes.len() >= 4 {
                            default_crop_h = u16::from_be_bytes([val_bytes[0], val_bytes[1]]) as usize;
                            default_crop_w = u16::from_be_bytes([val_bytes[2], val_bytes[3]]) as usize;
                        }
                    }
                    0x0141 => {
                        // Bit depth
                        if val_bytes.len() >= 2 {
                            bits_per_sample = u16::from_be_bytes([val_bytes[0], val_bytes[1]]) as u32;
                        }
                    }
                    _ => {}
                }
                curr += 4 + len;
            }
        }

        // Parse raw_head SubIFD
        let mut width = meta_w;
        let mut height = meta_h;
        if raw_head.len() >= 52 {
            let rw = u32::from_le_bytes([raw_head[36], raw_head[37], raw_head[38], raw_head[39]]) as usize;
            let rh = u32::from_le_bytes([raw_head[48], raw_head[49], raw_head[50], raw_head[51]]) as usize;
            if rw > 0 && rh > 0 {
                width = rw;
                height = rh;
            }
        }

        if default_crop_w == 0 || default_crop_h == 0 {
            default_crop_w = width.saturating_sub(16);
            default_crop_h = height.saturating_sub(18);
        }

        // Parse SubIFD tag offsets for BlackLevel and WhiteBalance
        let mut subifd_offsets = SubIfdTagOffsets::default();
        let mut strip_offsets = 2048usize;
        let endian = if raw_head.starts_with(b"II") { "<" } else { ">" };
        if raw_head.len() >= 8 {
            let first_ifd = if endian == "<" {
                u32::from_le_bytes([raw_head[4], raw_head[5], raw_head[6], raw_head[7]]) as usize
            } else {
                u32::from_be_bytes([raw_head[4], raw_head[5], raw_head[6], raw_head[7]]) as usize
            };

            let mut sub_off = 26usize;
            if first_ifd + 2 <= raw_head.len() {
                let num_tags = if endian == "<" {
                    u16::from_le_bytes([raw_head[first_ifd], raw_head[first_ifd + 1]]) as usize
                } else {
                    u16::from_be_bytes([raw_head[first_ifd], raw_head[first_ifd + 1]]) as usize
                };
                let mut pos = first_ifd + 2;
                for _ in 0..num_tags {
                    if pos + 12 > raw_head.len() {
                        break;
                    }
                    let t = if endian == "<" {
                        u16::from_le_bytes([raw_head[pos], raw_head[pos + 1]])
                    } else {
                        u16::from_be_bytes([raw_head[pos], raw_head[pos + 1]])
                    };
                    let v = if endian == "<" {
                        u32::from_le_bytes([raw_head[pos + 8], raw_head[pos + 9], raw_head[pos + 10], raw_head[pos + 11]]) as usize
                    } else {
                        u32::from_be_bytes([raw_head[pos + 8], raw_head[pos + 9], raw_head[pos + 10], raw_head[pos + 11]]) as usize
                    };
                    if t == 0xf000 {
                        sub_off = v;
                        break;
                    }
                    pos += 12;
                }
            }

            if sub_off + 2 <= raw_head.len() {
                let num_sub = if endian == "<" {
                    u16::from_le_bytes([raw_head[sub_off], raw_head[sub_off + 1]]) as usize
                } else {
                    u16::from_be_bytes([raw_head[sub_off], raw_head[sub_off + 1]]) as usize
                };
                let mut pos = sub_off + 2;
                for _ in 0..num_sub {
                    if pos + 12 > raw_head.len() {
                        break;
                    }
                    let t = if endian == "<" {
                        u16::from_le_bytes([raw_head[pos], raw_head[pos + 1]])
                    } else {
                        u16::from_be_bytes([raw_head[pos], raw_head[pos + 1]])
                    };
                    let v = if endian == "<" {
                        u32::from_le_bytes([raw_head[pos + 8], raw_head[pos + 9], raw_head[pos + 10], raw_head[pos + 11]]) as usize
                    } else {
                        u32::from_be_bytes([raw_head[pos + 8], raw_head[pos + 9], raw_head[pos + 10], raw_head[pos + 11]]) as usize
                    };

                    match t {
                        0xf007 => strip_offsets = v,
                        0xf008 => subifd_offsets.strip_byte_counts_offset = Some(pos + 8),
                        0xf00a => subifd_offsets.black_level_offset = Some(v),
                        0xf00c => subifd_offsets.wb_std_offset = Some(v),
                        0xf00d => subifd_offsets.wb_auto_offset = Some(v),
                        0xf00e => subifd_offsets.wb_as_shot_offset = Some(v),
                        0xf013 => subifd_offsets.wb_auto_copy1_offset = Some(v),
                        0xf014 => subifd_offsets.wb_auto_copy2_offset = Some(v),
                        _ => {}
                    }
                    pos += 12;
                }
            }
        }

        if strip_offsets > 0 && strip_offsets <= raw_head.len() {
            raw_head.truncate(strip_offsets);
        }

        // Determine default black level
        let mut black_level = 1024u16;
        if let Some(bl_off) = subifd_offsets.black_level_offset {
            if bl_off + 4 <= raw_head.len() {
                let val = u32::from_le_bytes([raw_head[bl_off], raw_head[bl_off + 1], raw_head[bl_off + 2], raw_head[bl_off + 3]]) as u16;
                if val > 0 {
                    black_level = val;
                }
            }
        }

        let is_uncompressed = u32::from_be_bytes([header[0x6c], header[0x6d], header[0x6e], header[0x6f]]) == 0;

        Ok(Self {
            source_path: path_ref.to_path_buf(),
            is_uncompressed,
            header,
            jpeg_data,
            meta_data,
            raw_head,
            width,
            height,
            default_crop_w,
            default_crop_h,
            default_crop_top,
            default_crop_left,
            black_level,
            bits_per_sample,
            subifd_offsets,
            camera_model,
        })
    }
}
