// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2024 Fujifier Project / Isidore Paulin

use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Once;

use crate::pipeline::{ConvertFileOptions, Converter};
use crate::repack::RepackMode;

static INIT_HOOK: Once = Once::new();

fn ensure_panic_hook_installed() {
    INIT_HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|_info| {
            // Suppress default multi-thread stderr dumping.
            // Panics are safely caught by catch_unwind and returned via error buffer.
        }));
    });
}

fn set_error(buf: *mut c_char, len: usize, msg: &str) {
    if buf.is_null() || len == 0 {
        return;
    }
    let bytes = msg.as_bytes();
    let copy_len = bytes.len().min(len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, copy_len);
        *buf.add(copy_len) = 0;
    }
}

/// Returns the library version string.
#[no_mangle]
pub extern "C" fn raw2raf_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Converts a RAW camera file to a Fujifilm RAF file using the specified reference container.
///
/// Parameters:
/// - `input_path_ptr`: Path to input RAW file (e.g. .ARW, .CR2, .CR3, .NEF, .DNG)
/// - `output_path_ptr`: Path to target output .RAF file
/// - `container_arg_ptr`: Path to reference RAF container file(s) or folder (delimited by ; or ,)
/// - `non_gfx_model_ptr`: Optional camera model override for non-GFX containers (e.g. "GFX50S II" or null)
/// - `auto_container`: If true, automatically selects the smallest fitting Bayer RAF container from the pool
/// - `error_msg_buf`: Output buffer for error message on failure (null-terminated)
/// - `error_msg_buf_len`: Size of the output error buffer in bytes
///
/// Returns:
///   0 on success
///   Non-zero on error
#[no_mangle]
pub unsafe extern "C" fn raw2raf_convert_file(
    input_path_ptr: *const c_char,
    output_path_ptr: *const c_char,
    container_arg_ptr: *const c_char,
    non_gfx_model_ptr: *const c_char,
    auto_container: bool,
    error_msg_buf: *mut c_char,
    error_msg_buf_len: usize,
) -> i32 {
    raw2raf_convert_file_ex(
        input_path_ptr,
        output_path_ptr,
        container_arg_ptr,
        non_gfx_model_ptr,
        auto_container,
        -0.72,
        error_msg_buf,
        error_msg_buf_len,
    )
}

/// Converts a RAW camera file to a Fujifilm RAF file with explicit exposure bias.
#[no_mangle]
pub unsafe extern "C" fn raw2raf_convert_file_ex(
    input_path_ptr: *const c_char,
    output_path_ptr: *const c_char,
    container_arg_ptr: *const c_char,
    non_gfx_model_ptr: *const c_char,
    auto_container: bool,
    exposure_bias: f64,
    error_msg_buf: *mut c_char,
    error_msg_buf_len: usize,
) -> i32 {
    ensure_panic_hook_installed();

    let result = catch_unwind(AssertUnwindSafe(|| {
        if input_path_ptr.is_null() || output_path_ptr.is_null() || container_arg_ptr.is_null() {
            set_error(error_msg_buf, error_msg_buf_len, "Null pointer passed for input, output, or container path");
            return -1;
        }

        let input_str = match unsafe { CStr::from_ptr(input_path_ptr) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_error(error_msg_buf, error_msg_buf_len, "Input path is not valid UTF-8");
                return -2;
            }
        };

        let output_str = match unsafe { CStr::from_ptr(output_path_ptr) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_error(error_msg_buf, error_msg_buf_len, "Output path is not valid UTF-8");
                return -3;
            }
        };

        let container_str = match unsafe { CStr::from_ptr(container_arg_ptr) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_error(error_msg_buf, error_msg_buf_len, "Container path is not valid UTF-8");
                return -4;
            }
        };

        let in_path = PathBuf::from(input_str);
        let out_path = PathBuf::from(output_str);

        if !in_path.exists() {
            set_error(error_msg_buf, error_msg_buf_len, &format!("Input file does not exist: {}", in_path.display()));
            return -5;
        }

        let non_gfx_model = if !non_gfx_model_ptr.is_null() {
            match unsafe { CStr::from_ptr(non_gfx_model_ptr) }.to_str() {
                Ok(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
                _ => None,
            }
        } else {
            None
        };

        let containers: Vec<String> = container_str
            .split([';', '|', ','])
            .map(|s| s.trim().trim_matches(['\'', '"']).to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if containers.is_empty() {
            set_error(error_msg_buf, error_msg_buf_len, "No valid container paths provided");
            return -6;
        }

        // Initialize Converter with container templates
        let converter = match Converter::new(None, &containers, None, auto_container) {
            Ok(c) => c,
            Err(e) => {
                set_error(error_msg_buf, error_msg_buf_len, &format!("Failed to load reference RAF container: {e}"));
                return -7;
            }
        };

        let bias = if exposure_bias < -20.0 || exposure_bias > 20.0 {
            -0.72
        } else {
            exposure_bias
        };

        let file_opts = ConvertFileOptions {
            mode: RepackMode::Native,
            exposure_bias: bias,
            preview_path: None,
            empty_preview: false,
            keep_dng: false,
            verify: false,
            transfer_meta: false,
            custom_metadata: &[],
            custom_model: None,
            non_gfx_model: non_gfx_model.as_deref(),
            verbose: false,
        };

        if let Err(e) = converter.convert_file(&in_path, &out_path, &file_opts) {
            // Remove corrupted/partial output file on error
            if out_path.exists() {
                let _ = std::fs::remove_file(&out_path);
            }
            set_error(error_msg_buf, error_msg_buf_len, &format!("RAF conversion failed: {e:#}"));
            return -8;
        }

        0
    }));

    match result {
        Ok(code) => code,
        Err(panic_err) => {
            // Remove incomplete output file on panic
            if !output_path_ptr.is_null() {
                if let Ok(s) = unsafe { CStr::from_ptr(output_path_ptr) }.to_str() {
                    let p = Path::new(s);
                    if p.exists() {
                        let _ = std::fs::remove_file(p);
                    }
                }
            }

            let msg = if let Some(s) = panic_err.downcast_ref::<&str>() {
                format!("Panic in raw2raf native engine: {s}")
            } else if let Some(s) = panic_err.downcast_ref::<String>() {
                format!("Panic in raw2raf native engine: {s}")
            } else {
                "Panic in raw2raf native engine".to_string()
            };

            set_error(error_msg_buf, error_msg_buf_len, &msg);
            -99
        }
    }
}

