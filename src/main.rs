//! RAW to RAF Converter in Rust
//! High-performance conversion of camera RAW files to Fujifilm GFX 100S II RAF files.

mod batch;
mod jpeg;
mod metadata;
mod pipeline;
mod repack;
mod template;
mod tiff;

use std::path::PathBuf;
use std::process;
use std::str::FromStr;
use clap::Parser;

use crate::batch::{run_batch, BatchOptions};
use crate::pipeline::{ConvertFileOptions, Converter};
use crate::repack::RepackMode;

#[derive(Parser, Debug)]
#[command(
    name = "raw2raf",
    author = "IPWeb",
    version = "1.0.1",
    about = "Ultra-fast camera RAW to Fujifilm GFX 100S II RAF converter in Rust.",
    after_help = "Single-file mode:\n  raw2raf IMG_001.ARW [IMG_001.raf]\n\nBatch mode (recursive folder):\n  raw2raf --batch /path/to/raws --output-dir /path/to/output\n  raw2raf --batch /path/to/raws            # output alongside source files"
)]
struct Cli {
    /// Path to a single input RAW file (e.g. .ARW, .CR2, .CR3, .NEF)
    #[arg(value_name = "INPUT_RAW", conflicts_with = "batch")]
    input_raw: Option<PathBuf>,

    /// Output .RAF path for single-file mode (optional; defaults to <input>_converted.raf)
    #[arg(value_name = "OUTPUT_RAF", conflicts_with = "batch")]
    output_raf: Option<PathBuf>,

    /// Recursively convert every RAW file found under INPUT_DIR
    #[arg(long, value_name = "INPUT_DIR", conflicts_with = "input_raw")]
    batch: Option<PathBuf>,

    /// Root directory for batch output. Sub-folder structure from INPUT_DIR is preserved.
    #[arg(long, value_name = "DIR")]
    output_dir: Option<PathBuf>,

    /// Batch mode: overwrite .raf files that already exist in the output location
    #[arg(long)]
    overwrite: bool,

    /// Number of worker threads for parallel batch conversion (default: 0 = auto-detect CPU cores)
    #[arg(short = 'j', long, default_value = "0")]
    jobs: usize,

    /// Repack mode: 'native' (default), 'canvas' (102MP canvas), or 'unpadded'
    #[arg(long, default_value = "native")]
    mode: String,

    /// Raw baseline exposure bias in EV (default: -0.72 matching native GFX baseline calibration)
    #[arg(long, default_value = "-0.72")]
    exposure_bias: f64,

    /// Path to a custom JPEG preview to embed (single-file mode only)
    #[arg(long, value_name = "PATH")]
    preview: Option<String>,

    /// Strip embedded preview and leave an empty blank placeholder
    #[arg(long, visible_alias = "no-preview")]
    empty_preview: bool,

    /// Path to dnglab executable
    #[arg(long, value_name = "PATH")]
    dnglab: Option<String>,

    /// Path(s) or directory of reference Fujifilm RAF container files (e.g. --container container/ or multiple files, default: container/ folder or DSCF0260.RAF).
    #[arg(long, value_name = "PATH", alias = "template", num_args = 1.., value_delimiter = ',')]
    container: Vec<String>,

    /// Automatically select the smallest compatible Bayer RAF container based on input resolution
    #[arg(long)]
    auto_container: bool,

    /// Set custom metadata key-value pair(s), e.g. --set Model="GFX50S II" --set Artist="Name"
    #[arg(long = "set", alias = "meta", value_name = "KEY=VALUE", num_args = 1..)]
    custom_meta: Vec<String>,

    /// Override camera model metadata and RAF header (shorthand for --set Model=<MODEL>)
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,

    /// Override camera model specifically when a non-GFX container is selected (e.g. --non-gfx-model "GFX50S II" or "gfx50-sii")
    #[arg(long, value_name = "MODEL")]
    non_gfx_model: Option<String>,

    /// Keep the intermediate DNG file
    #[arg(long)]
    keep_dng: bool,

    /// Skip post-conversion dnglab verification
    #[arg(long)]
    no_verify: bool,

    /// Skip exiftool metadata transfer (keeps template GFX shooting metadata)
    #[arg(long)]
    no_metadata: bool,
}

fn main() {
    let cli = Cli::parse();

    if cli.input_raw.is_none() && cli.batch.is_none() {
        eprintln!("[ERROR] You must specify either an input RAW file or --batch <INPUT_DIR>.");
        eprintln!("Run with --help for usage instructions.");
        process::exit(1);
    }

    let mode = match RepackMode::from_str(&cli.mode) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[ERROR] {}", e);
            process::exit(1);
        }
    };

    let converter = match Converter::new(
        cli.dnglab.as_deref(),
        &cli.container,
        None,
        cli.auto_container,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ERROR] Initialization failed: {}", e);
            process::exit(1);
        }
    };

    let mut custom_metadata: Vec<(String, String)> = Vec::new();

    for item in &cli.custom_meta {
        if let Some((k, v)) = item.split_once('=') {
            custom_metadata.push((k.trim().to_string(), v.trim().to_string()));
        } else {
            eprintln!("[ERROR] Invalid metadata format '{}'. Expected KEY=VALUE (e.g. --set Model=\"GFX50S II\")", item);
            process::exit(1);
        }
    }

    if let Some(ref m) = cli.model {
        if let Some(pos) = custom_metadata.iter().position(|(k, _)| k.eq_ignore_ascii_case("Model")) {
            custom_metadata[pos] = ("Model".to_string(), m.clone());
        } else {
            custom_metadata.push(("Model".to_string(), m.clone()));
        }
    }

    let custom_model = custom_metadata
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Model"))
        .map(|(_, v)| v.clone());

    // Single-file mode
    if let Some(ref in_raw) = cli.input_raw {
        let out_raf = if let Some(ref out) = cli.output_raf {
            out.clone()
        } else {
            let stem = in_raw.file_stem().unwrap_or_default().to_string_lossy();
            let parent = in_raw.parent().unwrap_or_else(|| std::path::Path::new(""));
            parent.join(format!("{}_converted.raf", stem))
        };

        let file_opts = ConvertFileOptions {
            mode,
            exposure_bias: cli.exposure_bias,
            preview_path: cli.preview.as_deref(),
            empty_preview: cli.empty_preview,
            keep_dng: cli.keep_dng,
            verify: !cli.no_verify,
            transfer_meta: !cli.no_metadata,
            custom_metadata: &custom_metadata,
            custom_model: custom_model.as_deref(),
            non_gfx_model: cli.non_gfx_model.as_deref(),
            verbose: true,
        };

        if let Err(e) = converter.convert_file(in_raw, &out_raf, &file_opts) {
            eprintln!("\n[ERROR] Conversion failed: {:#}", e);
            process::exit(1);
        }
        return;
    }

    // Batch mode
    if let Some(ref batch_in) = cli.batch {
        let batch_opts = BatchOptions {
            mode,
            exposure_bias: cli.exposure_bias,
            empty_preview: cli.empty_preview,
            keep_dng: cli.keep_dng,
            verify: !cli.no_verify,
            transfer_meta: !cli.no_metadata,
            custom_metadata,
            custom_model,
            non_gfx_model: cli.non_gfx_model,
            overwrite: cli.overwrite,
            jobs: cli.jobs,
        };

        if let Err(e) = run_batch(
            &converter,
            batch_in,
            cli.output_dir.as_deref(),
            &batch_opts,
        ) {
            eprintln!("\n[ERROR] Batch processing encountered errors: {}", e);
            process::exit(1);
        }
    }
}
