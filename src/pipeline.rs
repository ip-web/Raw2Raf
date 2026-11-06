//! Single-file RAW to RAF conversion pipeline.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use anyhow::{anyhow, Context, Result};
use tempfile::Builder;

use crate::jpeg::extract_raw_preview;
use crate::metadata::transfer_metadata;
use crate::repack::{repack, RawSourceInfo, RepackMode, RepackOptions};
use crate::template::{normalize_camera_model, GfxTemplate};

#[derive(Clone)]
pub struct ContainerPool {
    pub templates: Vec<Arc<GfxTemplate>>,
}

impl ContainerPool {
    pub fn new(templates: Vec<Arc<GfxTemplate>>) -> Self {
        Self { templates }
    }

    pub fn select_best_fitting(&self, width: usize, height: usize, min_bps: u32) -> Arc<GfxTemplate> {
        assert!(!self.templates.is_empty(), "Container pool is empty");

        // 1. Filter candidates where canvas can contain the input and bit depth is preserved
        let mut candidates: Vec<&Arc<GfxTemplate>> = self.templates
            .iter()
            .filter(|t| t.width >= width && t.height >= height && t.bits_per_sample >= min_bps)
            .collect();

        // Fallback: if no template meets min_bps, allow any container that contains the input dimensions
        if candidates.is_empty() {
            candidates = self.templates
                .iter()
                .filter(|t| t.width >= width && t.height >= height)
                .collect();
        }

        if !candidates.is_empty() {
            // Sort to find the best fit:
            // 1. Canvas area (width * height) ascending -> tightest fit / smallest file size
            // 2. Uncompressed preference (prefer uncompressed over compressed formats)
            // 3. Bit depth preference (prefer higher bit depth among equal area containers)
            // 4. Camera generation / recency ranking for templates with identical specifications
            candidates.sort_by(|a, b| {
                let area_a = a.width * a.height;
                let area_b = b.width * b.height;
                area_a.cmp(&area_b)
                    .then_with(|| b.is_uncompressed.cmp(&a.is_uncompressed))
                    .then_with(|| b.bits_per_sample.cmp(&a.bits_per_sample))
                    .then_with(|| {
                        let rank = |t: &GfxTemplate| {
                            let m = t.camera_model.to_uppercase();
                            if m.contains("T200") || m.contains("GFX100 II") || m.contains("GFX100S II") {
                                4
                            } else if m.contains("T100") || m.contains("A7") || m.contains("GFX100S") {
                                3
                            } else if m.contains("A5") || m.contains("A3") || m.contains("GFX50S II") {
                                2
                            } else if m.contains("GFX") {
                                1
                            } else {
                                0
                            }
                        };
                        rank(b).cmp(&rank(a))
                    })
            });
            return (*candidates[0]).clone();
        }

        // 2. If no candidate completely contains the input (e.g. input > 100MP):
        // Pick the candidate with the maximum bit depth and canvas area
        self.templates
            .iter()
            .max_by_key(|t| (t.bits_per_sample, t.width * t.height, t.is_uncompressed))
            .unwrap()
            .clone()
    }
}

#[derive(Clone)]
pub struct Converter {
    pub dnglab_path: Option<PathBuf>,
    pub container_pool: ContainerPool,
    pub exiftool_path: Option<PathBuf>,
}

pub struct ConvertFileOptions<'a> {
    pub mode: RepackMode,
    pub exposure_bias: f64,
    pub preview_path: Option<&'a str>,
    pub empty_preview: bool,
    pub keep_dng: bool,
    pub verify: bool,
    pub transfer_meta: bool,
    pub custom_metadata: &'a [(String, String)],
    pub custom_model: Option<&'a str>,
    pub non_gfx_model: Option<&'a str>,
    pub verbose: bool,
}

impl Converter {
    pub fn new(
        dnglab_override: Option<&str>,
        container_inputs: &[String],
        exiftool_override: Option<&str>,
        auto_container: bool,
    ) -> Result<Self> {
        let dnglab_path = find_executable("dnglab", dnglab_override);
        let exiftool_path = find_executable("exiftool", exiftool_override);

        let templates = resolve_container_templates(container_inputs, auto_container)?;

        println!(
            "[*] Container Pool: loaded {} container template(s):",
            templates.len()
        );
        let mut distinct = templates.clone();
        distinct.sort_by_key(|t| t.width * t.height);
        distinct.dedup_by_key(|t| (t.width, t.height, t.camera_model.clone()));
        for t in &distinct {
            let fname = t.source_path.file_name().unwrap_or_default().to_string_lossy();
            println!(
                "    - {:<30} (Canvas: {:>5} x {:>5}, Bits: {}) [from '{}']",
                t.camera_model, t.width, t.height, t.bits_per_sample, fname
            );
        }

        Ok(Self {
            dnglab_path,
            container_pool: ContainerPool::new(templates),
            exiftool_path,
        })
    }

    pub fn convert_to_dng<P1: AsRef<Path>, P2: AsRef<Path>>(&self, input_raw: P1, output_dng: P2) -> Result<()> {
        let dnglab = self.dnglab_path.as_ref()
            .ok_or_else(|| anyhow!("dnglab executable not found for DNG generation."))?;
        let mut cmd = Command::new(dnglab);
        cmd.args([
            "convert",
            "--compression", "uncompressed",
            "--embed-raw", "false",
            "--dng-preview", "false",
            "--dng-thumbnail", "false",
            "-f",
        ]);
        cmd.arg(input_raw.as_ref());
        cmd.arg(output_dng.as_ref());

        let output = cmd.output().with_context(|| format!("Running dnglab from: {:?}", dnglab))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(anyhow!("dnglab failed to convert:\n{}\n{}", stderr, stdout));
        }
        Ok(())
    }

    pub fn verify_raf<P: AsRef<Path>>(&self, raf_path: P) -> bool {
        // Fast in-memory verification with rawler
        if rawler::decode_file(raf_path.as_ref()).is_ok() {
            return true;
        }
        // Fallback to dnglab subprocess if available
        if let Some(ref dnglab) = self.dnglab_path {
            if let Ok(td) = Builder::new().prefix("verify_").tempdir() {
                let test_dng = td.path().join("verify_check.dng");
                let mut cmd = Command::new(dnglab);
                cmd.args(["convert", "-f"]);
                cmd.arg(raf_path.as_ref());
                cmd.arg(&test_dng);
                return cmd.output().map(|o| o.status.success()).unwrap_or(false);
            }
        }
        false
    }

    pub fn convert_file<P1: AsRef<Path>, P2: AsRef<Path>>(
        &self,
        input_raw: P1,
        output_raf: P2,
        opts: &ConvertFileOptions,
    ) -> Result<()> {
        let input_raw_ref = input_raw.as_ref();
        let output_raf_ref = output_raf.as_ref();

        if !input_raw_ref.exists() {
            return Err(anyhow!("Input RAW file not found: {:?}", input_raw_ref));
        }

        // Preview resolution
        let (preview_bytes_storage, empty_preview) = if opts.empty_preview {
            if opts.verbose {
                println!("[*] Preview mode: Stripping preview and leaving empty blank placeholder.");
            }
            (None, true)
        } else if let Some(p) = opts.preview_path {
            if opts.verbose {
                println!("[*] Loading custom preview from: {}", p);
            }
            let bytes = fs::read(p).with_context(|| format!("Reading custom preview: {}", p))?;
            (Some(bytes), false)
        } else {
            if opts.verbose {
                println!("[*] Extracting preview image from input RAW: {:?}", input_raw_ref);
            }
            let exif_str = self.exiftool_path.as_ref().map(|p| p.to_string_lossy().to_string());
            if let Some(bytes) = extract_raw_preview(input_raw_ref, exif_str.as_deref()) {
                if opts.verbose {
                    println!("    -> Extracted preview ({} bytes)", bytes.len());
                }
                (Some(bytes), false)
            } else {
                if opts.verbose {
                    println!("    -> No preview found in input RAW; using empty blank placeholder.");
                }
                (None, true)
            }
        };

        let preview_bytes = preview_bytes_storage.as_deref();

        // Step 1: Decode RAW in memory with rawler
        if opts.verbose {
            println!("[*] Step 1: Decoding RAW image in memory with rawler: {:?}", input_raw_ref);
        }
        let raw = rawler::decode_file(input_raw_ref)
            .with_context(|| format!("Decoding RAW file with rawler: {:?}", input_raw_ref))?;

        let mut raw_info = RawSourceInfo::from_rawler(&raw)?;

        // If WB multipliers were not available from rawler (or defaulted to 1.0),
        // try to query camera white balance from exiftool if available.
        if raw_info.r_mul == 1.0 && raw_info.b_mul == 1.0 {
            if let Some(ref tool) = self.exiftool_path {
                let tool_str = tool.to_string_lossy().to_string();
                if let Some((r, b)) = crate::metadata::read_camera_wb_via_exiftool(input_raw_ref, &tool_str) {
                    if opts.verbose {
                        println!("    - Camera WB extracted via exiftool: R={:.4}, B={:.4}", r, b);
                    }
                    raw_info.r_mul = r;
                    raw_info.b_mul = b;
                }
            }
        }

        if opts.verbose {
            println!("    - Camera: {} (model: {})", raw.clean_make, raw.clean_model);
            println!("    - Dimensions: {} x {} (Crop: {} x {})", raw_info.width, raw_info.height, raw_info.crop_w, raw_info.crop_h);
            println!("    - BlackLevel: {:.1}, WhiteLevel: {:.0}, Bits: {}", raw_info.black_level, raw_info.white_level, raw_info.bits_per_sample);
            println!("    - CFA pattern: {:?}, CropOrigin: [{}, {}]", raw_info.cfa_pattern, raw_info.crop_x, raw_info.crop_y);
            println!("    - White balance gains: R={:.4}, B={:.4}", raw_info.r_mul, raw_info.b_mul);
            if raw_info.active_top > 0 || raw_info.active_left > 0 {
                println!("    - ActiveArea: top={}, left={}", raw_info.active_top, raw_info.active_left);
            }
        }

        let raw_pixels_cow = match &raw.data {
            rawler::RawImageData::Integer(v) => std::borrow::Cow::Borrowed(v.as_slice()),
            rawler::RawImageData::Float(v) => {
                let converted = rawler::imgop::convert_from_f32_scaled_u16(v, 0, u16::MAX);
                std::borrow::Cow::Owned(converted)
            }
        };

        // If input RAW is 3-channel Linear RAW (e.g. Apple iPhone ProRAW), remosaic to single-channel Bayer RGGB
        let raw_pixels_remosaiced: Vec<u16>;
        let raw_pixels = if raw.cpp == 3 || raw_info.is_linear_raw {
            if opts.verbose {
                println!("    [*] Detected 3-channel Linear RAW; remosaicing RGB to single-channel Bayer RGGB...");
            }
            raw_pixels_remosaiced = crate::tiff::remosaic_rgb_to_bayer(raw_pixels_cow.as_ref(), raw_info.width, raw_info.height);
            &raw_pixels_remosaiced
        } else {
            raw_pixels_cow.as_ref()
        };

        // Determine active container template using best-fitting selection from the pool
        let w = raw_info.crop_w;
        let h = raw_info.crop_h;
        let min_bps = raw_info.bits_per_sample.max(14);
        let active_template = self.container_pool.select_best_fitting(w, h, min_bps);

        let effective_model: Option<String> = if let Some(m) = opts.custom_model {
            Some(normalize_camera_model(m))
        } else if active_template.is_non_gfx() {
            opts.non_gfx_model.map(normalize_camera_model)
        } else {
            None
        };

        if opts.verbose {
            let container_filename = active_template.source_path.file_name().unwrap_or_default().to_string_lossy();
            let model_override_display = if let Some(ref m) = effective_model {
                format!(" -> Model overridden to '{}'", m)
            } else {
                String::new()
            };
            println!(
                "    [*] [Best Fit Container] {}{} (Canvas: {} x {}, Bits: {}) -> Selected from '{}' (for input: {} x {})",
                active_template.camera_model, model_override_display, active_template.width, active_template.height, active_template.bits_per_sample,
                container_filename, w, h
            );
        }

        // Step 2: Repack into Fujifilm RAF
        if opts.verbose {
            let target_model_name = effective_model.as_deref().unwrap_or(&active_template.camera_model);
            println!(
                "[*] Step 2: Repacking into Fujifilm {} RAF (Mode: {:?}, ExposureBias: {:.2} EV)...",
                target_model_name, opts.mode, opts.exposure_bias
            );
        }

        // Ensure parent directory exists
        if let Some(parent) = output_raf_ref.parent() {
            fs::create_dir_all(parent)?;
        }

        let repack_opts = RepackOptions {
            mode: opts.mode,
            exposure_bias: opts.exposure_bias,
            preview_data: preview_bytes,
            empty_preview,
            custom_model: effective_model.as_deref(),
        };

        repack(&raw_info, raw_pixels, &active_template, output_raf_ref, &repack_opts)?;

        if opts.verbose {
            let raf_size = fs::metadata(output_raf_ref)?.len();
            println!("    -> Successfully generated: {:?} ({} bytes)", output_raf_ref, raf_size);
        }

        // Step 3: Verify with rawler / dnglab
        if opts.verify {
            if opts.verbose {
                println!("[*] Step 3: Verifying repacked RAF in memory...");
            }
            let ok = self.verify_raf(output_raf_ref);
            if ok {
                if opts.verbose {
                    println!("    -> [SUCCESS] Verification passed! Repacked RAF decodes cleanly.");
                }
            } else {
                eprintln!("    -> [WARNING] Verification reported an issue decoding the output RAF.");
            }
        }

        // Step 4: Metadata transfer via exiftool
        if opts.transfer_meta {
            if let Some(ref tool) = self.exiftool_path {
                if opts.verbose {
                    println!("[*] Step 4: Transferring shooting metadata from source RAW via exiftool (stripping non-translatable tags)...");
                }
                let mut effective_custom_meta: Vec<(String, String)> = opts.custom_metadata.to_vec();
                if let Some(ref m) = effective_model {
                    if let Some(pos) = effective_custom_meta.iter().position(|(k, _)| k.eq_ignore_ascii_case("Model")) {
                        effective_custom_meta[pos] = ("Model".to_string(), m.clone());
                    } else {
                        effective_custom_meta.push(("Model".to_string(), m.clone()));
                    }
                }
                let tool_str = tool.to_string_lossy().to_string();
                let ok = transfer_metadata(input_raw_ref, output_raf_ref, &tool_str, &effective_custom_meta)?;
                if ok && opts.verbose {
                    println!("    -> Metadata transferred: ExposureTime, FNumber, ISO, FocalLength, Lens, Date, GPS, ...");
                    println!("    -> Stripped non-translatable metadata: SerialNumber, InternalSerialNumber, LensSerialNumber, ImageUniqueID, ...");
                    for (k, v) in &effective_custom_meta {
                        println!("    -> Custom metadata: {}='{}'", k, v);
                    }
                }
            } else if opts.verbose {
                println!("    -> [SKIP] exiftool not found; metadata transfer skipped.");
            }
        }

        // Step 5: Keep DNG if requested (via dnglab if available)
        if opts.keep_dng {
            if let Some(ref _dnglab) = self.dnglab_path {
                let mut kept_dng = output_raf_ref.to_path_buf();
                let stem = output_raf_ref.file_stem().unwrap_or_default().to_string_lossy();
                kept_dng.set_file_name(format!("{}_intermediate.dng", stem));
                if let Err(e) = self.convert_to_dng(input_raw_ref, &kept_dng) {
                    eprintln!("    -> [WARNING] Failed to generate intermediate DNG: {}", e);
                } else if opts.verbose {
                    println!("[*] Kept intermediate DNG: {:?}", kept_dng);
                }
            }
        }

        Ok(())
    }
}

fn find_executable(name: &str, explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }

    // Check current directory
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let local = PathBuf::from(format!("{}{}", name, ext));
    if local.exists() {
        return Some(local.canonicalize().unwrap_or(local));
    }

    // Check next to the current running binary
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let next_to_exe = parent.join(format!("{}{}", name, ext));
            if next_to_exe.exists() {
                return Some(next_to_exe);
            }
        }
    }

    // Check system PATH via which / where
    #[cfg(windows)]
    {
        if let Ok(output) = Command::new("where").arg(name).output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let pb = PathBuf::from(first_line.trim());
                    if pb.exists() {
                        return Some(pb);
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(output) = Command::new("which").arg(name).output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let pb = PathBuf::from(first_line.trim());
                    if pb.exists() {
                        return Some(pb);
                    }
                }
            }
        }
    }

    None
}

fn resolve_container_templates(
    inputs: &[String],
    auto_container: bool,
) -> Result<Vec<Arc<GfxTemplate>>> {
    let mut paths_to_check = Vec::new();

    let scan_dir = |dir: &Path, out: &mut Vec<PathBuf>| {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() {
                    if let Some(ext) = p.extension() {
                        if ext.to_string_lossy().eq_ignore_ascii_case("raf") {
                            out.push(p);
                        }
                    }
                }
            }
        }
    };

    if inputs.is_empty() || auto_container || inputs.iter().any(|s| s == "auto") {
        if let Some(cdir) = find_file("container") {
            if cdir.is_dir() {
                scan_dir(&cdir, &mut paths_to_check);
            }
        }
        if let Some(base) = find_file("DSCF0260.RAF") {
            paths_to_check.push(base);
        }
    }

    for input in inputs {
        if input == "auto" {
            continue;
        }
        for part in input.split(',') {
            let part_trimmed = part.trim();
            if part_trimmed.is_empty() {
                continue;
            }
            let pb = PathBuf::from(part_trimmed);
            if pb.is_dir() {
                scan_dir(&pb, &mut paths_to_check);
            } else if pb.is_file() {
                paths_to_check.push(pb);
            } else if let Some(found) = find_file(part_trimmed) {
                if found.is_dir() {
                    scan_dir(&found, &mut paths_to_check);
                } else {
                    paths_to_check.push(found);
                }
            } else {
                eprintln!("    -> [WARNING] Specified container not found: {}", part_trimmed);
            }
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut templates = Vec::new();

    for p in paths_to_check {
        let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
        if !seen.insert(canon) {
            continue;
        }
        if let Ok(tmpl) = GfxTemplate::load(&p) {
            templates.push(Arc::new(tmpl));
        }
    }

    if templates.is_empty() {
        return Err(anyhow!(
            "No valid Fujifilm RAF container templates found. Please place container RAF files in 'container/' or specify --container <PATH>."
        ));
    }

    Ok(templates)
}

fn find_file<P: AsRef<Path>>(rel: P) -> Option<PathBuf> {
    let p = rel.as_ref();
    if p.exists() {
        return Some(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
    }
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let next_to_exe = parent.join(p);
            if next_to_exe.exists() {
                return Some(next_to_exe);
            }
        }
    }
    None
}
