//! JPEG parsing, sanitization, splicing, and embedded preview extraction.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use anyhow::{anyhow, Result};

/// Minimal valid 16x16 black JPEG image used as an empty preview placeholder.
pub const MINIMAL_BLANK_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x01, 0x00, 0x48,
    0x00, 0x48, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08,
    0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0a, 0x0c, 0x14, 0x0d, 0x0c, 0x0b, 0x0b, 0x0c, 0x19, 0x12,
    0x13, 0x0f, 0x14, 0x1d, 0x1a, 0x1f, 0x1e, 0x1d, 0x1a, 0x1c, 0x1c, 0x20, 0x24, 0x2e, 0x27, 0x20,
    0x22, 0x2c, 0x23, 0x1c, 0x1c, 0x28, 0x37, 0x29, 0x2c, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1f, 0x27,
    0x39, 0x3d, 0x38, 0x32, 0x3c, 0x2e, 0x33, 0x34, 0x32, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x10,
    0x00, 0x10, 0x01, 0x01, 0x22, 0x00, 0xff, 0xc4, 0x00, 0x1f, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04,
    0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0xff, 0xc4, 0x00, 0xb5, 0x10, 0x00, 0x02, 0x01, 0x03,
    0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00, 0x01, 0x7d, 0x01, 0x02, 0x03, 0x00,
    0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32,
    0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72,
    0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35,
    0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55,
    0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
    0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94,
    0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2,
    0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9,
    0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6,
    0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xff, 0xda,
    0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00, 0x7b, 0xfb, 0x40, 0xff, 0xd9,
];

/// Finds the byte offset where image tables/frames start (after APP/COM metadata segments).
pub fn find_jpeg_image_start(jpeg: &[u8]) -> Result<usize> {
    if jpeg.len() < 4 || &jpeg[0..2] != b"\xff\xd8" {
        return Err(anyhow!("Not a valid JPEG (missing SOI)"));
    }

    let mut pos = 2;
    while pos < jpeg.len().saturating_sub(1) {
        if jpeg[pos] != 0xff {
            break;
        }
        let marker = jpeg[pos + 1];
        // Standalone markers without length: SOI, EOI, TEM, RST0-7
        if marker == 0xd8 || marker == 0xd9 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            pos += 2;
            continue;
        }
        if pos + 4 > jpeg.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
        // APP0-APP15 (0xe0 - 0xef) and COM (0xfe) are metadata segments
        if (0xe0..=0xef).contains(&marker) || marker == 0xfe {
            pos += 2 + seg_len;
        } else {
            return Ok(pos);
        }
    }
    Ok(pos)
}

/// Strips proprietary/non-standard segments from a JPEG, keeping only standard segments:
/// DQT (0xdb), SOFn (0xc0-0xc3, 0xc5-0xc7, 0xc9-0xcb, 0xcd-0xcf), DHT (0xc4),
/// SOS (0xda) and trailing image data, EOI (0xd9), DRI (0xdd).
pub fn strip_jpeg_to_standard(jpeg: &[u8]) -> Vec<u8> {
    if jpeg.len() < 4 || &jpeg[0..2] != b"\xff\xd8" {
        return jpeg.to_vec();
    }

    let mut out = Vec::with_capacity(jpeg.len());
    out.extend_from_slice(b"\xff\xd8");

    let mut pos = 2;
    while pos < jpeg.len().saturating_sub(1) {
        if jpeg[pos] != 0xff {
            // Raw scan data after SOS - copy rest verbatim
            out.extend_from_slice(&jpeg[pos..]);
            break;
        }
        let marker = jpeg[pos + 1];
        // Standalone markers
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            pos += 2;
            continue;
        }
        if marker == 0xd9 {
            out.extend_from_slice(b"\xff\xd9");
            pos += 2;
            continue;
        }
        if pos + 4 > jpeg.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
        let total_seg = 2 + seg_len;

        let is_sof = (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8;
        let is_keep = is_sof
            || marker == 0xc4  // DHT
            || marker == 0xdb  // DQT
            || marker == 0xda  // SOS
            || marker == 0xd9  // EOI
            || marker == 0xdd; // DRI

        if is_keep {
            if pos + total_seg <= jpeg.len() {
                out.extend_from_slice(&jpeg[pos..pos + total_seg]);
                if marker == 0xda {
                    // SOS: copy everything after segment header as raw scan data
                    let sos_end = pos + total_seg;
                    out.extend_from_slice(&jpeg[sos_end..]);
                    break;
                }
            } else {
                break;
            }
        }
        pos += total_seg;
    }
    out
}

/// Combines the template's Fuji EXIF/MakerNotes APP segments with the
/// image stream (quantization, frame, huffman, DCT scan) of replacement_jpeg.
pub fn splice_preview_jpeg(template_jpeg: &[u8], replacement_jpeg: &[u8]) -> Result<Vec<u8>> {
    let tmpl_img_start = find_jpeg_image_start(template_jpeg)?;
    let fuji_app_segments = &template_jpeg[2..tmpl_img_start];

    let clean_replacement = strip_jpeg_to_standard(replacement_jpeg);
    let repl_img_start = find_jpeg_image_start(&clean_replacement)?;
    let repl_image_stream = &clean_replacement[repl_img_start..];

    let mut out = Vec::with_capacity(2 + fuji_app_segments.len() + repl_image_stream.len());
    out.extend_from_slice(b"\xff\xd8");
    out.extend_from_slice(fuji_app_segments);
    out.extend_from_slice(repl_image_stream);
    Ok(out)
}

/// Extracts embedded preview JPEG from a camera RAW file.
pub fn extract_raw_preview<P: AsRef<Path>>(raw_path: P, exiftool_path: Option<&str>) -> Option<Vec<u8>> {
    let raw_path_ref = raw_path.as_ref();
    if !raw_path_ref.exists() {
        return None;
    }

    // Method 1: exiftool (preferred)
    let tool_candidates = if let Some(p) = exiftool_path {
        vec![p.to_string()]
    } else {
        vec![
            "exiftool.exe".to_string(),
            "exiftool".to_string(),
        ]
    };

    for tool in &tool_candidates {
        for tag in &["-JpgFromRaw", "-PreviewImage", "-OtherImage", "-ThumbnailImage"] {
            if let Ok(output) = Command::new(tool)
                .args(["-b", tag])
                .arg(raw_path_ref)
                .output()
            {
                if output.status.success() && output.stdout.len() > 16384 && &output.stdout[0..3] == b"\xff\xd8\xff" {
                    return Some(output.stdout);
                }
            }
        }
    }

    // Method 2: Fast direct binary scan of up to 64MB
    if let Ok(mut f) = File::open(raw_path_ref) {
        let max_scan = 64 * 1024 * 1024;
        let mut buf = Vec::new();
        let _ = (&mut f).take(max_scan).read_to_end(&mut buf);

        let mut largest_jpeg: Option<Vec<u8>> = None;
        let mut largest_len = 0usize;
        let mut start = 0usize;

        while start + 4 < buf.len() {
            // Find SOI
            let mut pos = None;
            for i in start..buf.len().saturating_sub(3) {
                if &buf[i..i + 3] == b"\xff\xd8\xff" {
                    pos = Some(i);
                    break;
                }
            }
            let p = match pos {
                Some(val) => val,
                None => break,
            };

            // Find EOI
            let mut eoi = None;
            for i in (p + 2)..buf.len().saturating_sub(1) {
                if &buf[i..i + 2] == b"\xff\xd9" {
                    eoi = Some(i);
                    break;
                }
            }

            if let Some(e) = eoi {
                let length = (e + 2) - p;
                if length > 16384 && length > largest_len {
                    let candidate = &buf[p..e + 2];
                    // Check for SOF0 or SOF2 marker
                    if candidate.windows(2).any(|w| w == b"\xff\xc0" || w == b"\xff\xc2") {
                        largest_len = length;
                        largest_jpeg = Some(candidate.to_vec());
                    }
                }
                start = e + 2;
            } else {
                break;
            }
        }

        if largest_jpeg.is_some() {
            return largest_jpeg;
        }
    }

    None
}

