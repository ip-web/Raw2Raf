//! Metadata transfer from source RAW to output RAF using exiftool.

use std::path::Path;
use std::process::Command;
use anyhow::{Context, Result};

pub const METADATA_TAGS: &[&str] = &[
    // Exposure triangle
    "ExposureTime", "ShutterSpeedValue", "FNumber", "ApertureValue",
    "ISO", "ISOSpeedRatings", "SensitivityType", "RecommendedExposureIndex",
    // Exposure program & mode
    "ExposureProgram", "ExposureMode", "ExposureBiasValue", "ExposureCompensation",
    // Image orientation
    "Orientation",
    // Metering & flash
    "MeteringMode", "Flash", "FlashEnergy",
    // Lens & focal length
    "FocalLength", "FocalLengthIn35mmFormat", "MaxApertureValue",
    "LensMake", "LensModel", "LensInfo", "LensSpecification",
    // Focus & stabilisation
    "SubjectDistance", "SubjectDistanceRange", "FocusMode",
    // White balance & scene
    "SceneCaptureType", "SceneType", "LightSource", "GainControl",
    // Image adjustments
    "Contrast", "Saturation", "Sharpness",
    // Date & time
    "DateTimeOriginal", "CreateDate", "ModifyDate",
    "SubSecTime", "SubSecTimeOriginal", "SubSecTimeDigitized",
    "OffsetTime", "OffsetTimeOriginal", "OffsetTimeDigitized",
    // Creator / rights
    "Artist", "Copyright", "ImageDescription", "UserComment",
    // GPS
    "GPSLatitude", "GPSLatitudeRef", "GPSLongitude", "GPSLongitudeRef",
    "GPSAltitude", "GPSAltitudeRef", "GPSSpeed", "GPSSpeedRef",
    "GPSTrack", "GPSTrackRef", "GPSImgDirection", "GPSImgDirectionRef",
    "GPSDateStamp", "GPSTimeStamp", "GPSProcessingMethod", "GPSVersionID",
    "GPSMeasureMode", "GPSDOP", "GPSSatellites", "GPSStatus",
    // IPTC / XMP authoring info
    "Credit", "Source", "City", "State", "Country",
];

/// Non-translatable hardware and camera-specific tags that should be stripped.
pub const STRIP_TAGS: &[&str] = &[
    "SerialNumber",
    "InternalSerialNumber",
    "CameraSerialNumber",
    "BodySerialNumber",
    "LensSerialNumber",
    "ImageUniqueID",
    "RawImageDigest",
    "OriginalRawFileName",
    "OwnerName",
];

pub fn transfer_metadata<P1: AsRef<Path>, P2: AsRef<Path>>(
    source_raw: P1,
    output_raf: P2,
    exiftool_path: &str,
    custom_metadata: &[(String, String)],
) -> Result<bool> {
    let source_ref = source_raw.as_ref();
    let output_ref = output_raf.as_ref();

    let mut cmd = Command::new(exiftool_path);
    cmd.args(["-overwrite_original", "-P", "-n", "-F", "-m", "-q"]);

    // Reset default orientation on both IFD0 and IFD1 to Horizontal (1)
    // so container orientation doesn't leak through if source RAW has no Orientation tag.
    cmd.arg("-Orientation=1");
    cmd.arg("-IFD1:Orientation=1");

    cmd.arg("-TagsFromFile");
    cmd.arg(source_ref);

    for tag in METADATA_TAGS {
        cmd.arg(format!("-{}", tag));
    }

    // Synchronize IFD1 (preview/thumbnail) orientation with primary IFD0 Orientation
    cmd.arg("-IFD1:Orientation<Orientation");

    // Strip non-translatable / hardware-specific metadata
    for tag in STRIP_TAGS {
        cmd.arg(format!("-{}=", tag));
    }

    // Apply user-defined metadata key-value pairs (can set/override any tag or set Model)
    for (key, val) in custom_metadata {
        if val.is_empty() {
            cmd.arg(format!("-{}=", key));
        } else {
            cmd.arg(format!("-{}={}", key, val));
        }
    }

    cmd.arg(output_ref);

    let output = cmd.output().with_context(|| format!("Executing exiftool from: {}", exiftool_path))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_trimmed = stderr.trim();

    let has_hard_error = stderr_trimmed.lines().any(|line| {
        let l = line.trim_start();
        l.starts_with("Error:") && !l.contains("Warning")
    });

    if has_hard_error {
        eprintln!("    -> [WARNING] exiftool metadata transfer failed: {}", stderr_trimmed);
        return Ok(false);
    }

    if !stderr_trimmed.is_empty() {
        let snippet: String = stderr_trimmed.chars().take(200).collect();
        println!("    -> [NOTE] exiftool warnings (non-fatal): {}", snippet);
    }

    Ok(true)
}

/// Query camera white balance multipliers via exiftool for formats where rawler does not extract WB.
pub fn read_camera_wb_via_exiftool<P: AsRef<Path>>(source_raw: P, exiftool_path: &str) -> Option<(f64, f64)> {
    let output = Command::new(exiftool_path)
        .args([
            "-s", "-s", "-s",
            "-WBRedAsShot", "-WBGreenAsShot", "-WBBlueAsShot",
            "-RedBalance", "-BlueBalance",
            "-AsShotNeutral",
        ])
        .arg(source_raw.as_ref())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();

    // 1. Check WBRedAsShot, WBGreenAsShot, WBBlueAsShot (e.g. Sony SRF)
    if lines.len() >= 3 {
        if let (Ok(r), Ok(g), Ok(b)) = (lines[0].parse::<f64>(), lines[1].parse::<f64>(), lines[2].parse::<f64>()) {
            if g > 0.0 && r > 0.0 && b > 0.0 {
                return Some((r / g, b / g));
            }
        }
    }

    // 2. Check RedBalance / BlueBalance
    if lines.len() >= 2 {
        if let (Ok(r), Ok(b)) = (lines[0].parse::<f64>(), lines[1].parse::<f64>()) {
            if r > 0.0 && b > 0.0 {
                return Some((r, b));
            }
        }
    }

    // 3. Check AsShotNeutral (e.g. "0.45 1.0 0.55")
    for line in &lines {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            if let (Ok(nr), Ok(ng), Ok(nb)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>(), parts[2].parse::<f64>()) {
                if nr > 0.0 && ng > 0.0 && nb > 0.0 {
                    return Some((ng / nr, ng / nb));
                }
            }
        }
    }

    None
}
