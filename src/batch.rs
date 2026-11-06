//! Recursive folder batch conversion with multithreading and mirrored directory structure.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use anyhow::{anyhow, Result};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::pipeline::{ConvertFileOptions, Converter};
use crate::repack::RepackMode;

pub const RAW_EXTENSIONS: &[&str] = &[
    "arw", "cr2", "cr3", "crw", "dng", "nef", "nrw", "orf", "rw2", "srf", "sr2",
];

pub fn is_raw_file(path: &Path) -> bool {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    if file_name.ends_with("_converted.raf") || file_name.ends_with(".raf") {
        return false;
    }
    if let Some(ext) = path.extension() {
        let ext_str = ext.to_string_lossy().to_ascii_lowercase();
        RAW_EXTENSIONS.contains(&ext_str.as_str())
    } else {
        false
    }
}

pub fn collect_raw_files<P: AsRef<Path>>(root: P) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(true).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() && is_raw_file(entry.path()) {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    files
}

pub struct BatchOptions {
    pub mode: RepackMode,
    pub exposure_bias: f64,
    pub empty_preview: bool,
    pub keep_dng: bool,
    pub verify: bool,
    pub transfer_meta: bool,
    pub custom_metadata: Vec<(String, String)>,
    pub custom_model: Option<String>,
    pub non_gfx_model: Option<String>,
    pub overwrite: bool,
    pub jobs: usize,
}

pub fn run_batch(
    converter: &Converter,
    input_dir: &Path,
    output_root: Option<&Path>,
    opts: &BatchOptions,
) -> Result<()> {
    let input_dir = input_dir.canonicalize().unwrap_or_else(|_| input_dir.to_path_buf());
    if !input_dir.is_dir() {
        return Err(anyhow!("--batch path is not a directory: {:?}", input_dir));
    }

    let raw_files = collect_raw_files(&input_dir);
    if raw_files.is_empty() {
        println!("[INFO] No supported RAW files found under: {:?}", input_dir);
        return Ok(());
    }

    let total = raw_files.len();
    println!("\n[BATCH] Found {} RAW file(s) under: {:?}", total, input_dir);
    if let Some(out) = output_root {
        println!("[BATCH] Output root : {:?}", out);
    } else {
        println!("[BATCH] Output      : alongside source files (no --output-dir set)");
    }
    println!("[BATCH] Mode        : {:?}", opts.mode);
    let thread_count = if opts.jobs > 0 { opts.jobs } else { rayon::current_num_threads() };
    println!("[BATCH] Concurrency : {} worker thread(s)\n", thread_count);

    let progress_idx = AtomicUsize::new(0);
    let succeeded = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());

    // Configure thread pool
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .build()?;

    pool.install(|| {
        raw_files.par_iter().for_each(|raw_path| {
            let idx = progress_idx.fetch_add(1, Ordering::SeqCst) + 1;

            let raf_path = if let Some(out_root) = output_root {
                let rel = match raw_path.strip_prefix(&input_dir) {
                    Ok(r) => r,
                    Err(_) => raw_path.file_name().map(Path::new).unwrap_or(raw_path.as_path()),
                };
                let mut p = out_root.join(rel);
                p.set_extension("raf");
                p
            } else {
                let mut p = raw_path.clone();
                p.set_extension("raf");
                p
            };

            let rel_in = raw_path
                .strip_prefix(&input_dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| raw_path.file_name().unwrap_or_default().to_string_lossy().to_string());

            let rel_out = if let Some(out_root) = output_root {
                raf_path
                    .strip_prefix(out_root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| raf_path.to_string_lossy().to_string())
            } else {
                raf_path.file_name().unwrap_or_default().to_string_lossy().to_string()
            };

            if raf_path.exists() && !opts.overwrite {
                println!("[{}/{}] {}\n         -> {}\n         [SKIP] Output already exists (use --overwrite to replace).\n", idx, total, rel_in, rel_out);
                skipped.fetch_add(1, Ordering::SeqCst);
                return;
            }

            println!("[{}/{}] {}\n         -> {}", idx, total, rel_in, rel_out);

            if let Some(parent) = raf_path.parent() {
                let _ = fs::create_dir_all(parent);
            }

            let file_opts = ConvertFileOptions {
                mode: opts.mode,
                exposure_bias: opts.exposure_bias,
                preview_path: None,
                empty_preview: opts.empty_preview,
                keep_dng: opts.keep_dng,
                verify: opts.verify,
                transfer_meta: opts.transfer_meta,
                custom_metadata: &opts.custom_metadata,
                custom_model: opts.custom_model.as_deref(),
                non_gfx_model: opts.non_gfx_model.as_deref(),
                verbose: false, // compact logging for batch
            };

            match converter.convert_file(raw_path, &raf_path, &file_opts) {
                Ok(_) => {
                    succeeded.fetch_add(1, Ordering::SeqCst);
                    println!("         [OK]\n");
                }
                Err(e) => {
                    failed.fetch_add(1, Ordering::SeqCst);
                    eprintln!("         [FAILED] {}\n", e);
                    failures.lock().unwrap().push((rel_in, e.to_string()));
                }
            }
        });
    });

    let s_cnt = succeeded.load(Ordering::SeqCst);
    let k_cnt = skipped.load(Ordering::SeqCst);
    let f_cnt = failed.load(Ordering::SeqCst);

    println!("============================================================");
    println!("[BATCH DONE] {} file(s) processed.", total);
    println!("  Converted : {}", s_cnt);
    println!("  Skipped   : {}  (already existed)", k_cnt);
    println!("  Failed    : {}", f_cnt);
    let fail_list = failures.lock().unwrap();
    if !fail_list.is_empty() {
        println!("\nFailed files:");
        for (fname, reason) in fail_list.iter() {
            println!("  - {}: {}", fname, reason);
        }
    }
    println!("============================================================");

    if f_cnt > 0 {
        return Err(anyhow!("{} file(s) failed to convert in batch.", f_cnt));
    }

    Ok(())
}

