# Raw2Raf

> **A High-Performance Camera RAW to Fujifilm RAF Converter for Testing, Profiling, and RAW Pipeline Research.**

`Raw2Raf` is an open-source, ultra-fast research tool written in Rust that repacks proprietary camera RAW files (Sony ARW, Canon CR2/CR3/CRW, Nikon NEF/NRW, Panasonic RW2, Olympus ORF, DJI DNG, Apple ProRAW DNG, and Leica DNG) into valid Fujifilm Bayer `.RAF` containers.

---

## Purpose & Testing Use Cases

This tool is designed specifically for **testing, experimental camera profiling, and RAW pipeline research**:

1. **RAW Processing Engine & Color Pipeline Benchmarking**:
   - Evaluate how major commercial RAW processors (Capture One, Adobe Lightroom / Camera Raw, FastRawViewer, LibRaw, rawler, Darktable) interpret foreign sensor data through Fujifilm's color science and demosaicing pipelines.
   - Test and analyze Fujifilm Film Simulations (Provia, Velvia, Astia, Classic Chrome, Pro Neg, Classic Neg, Eterna, Nostalgic Neg, Reala Ace) across non-Fujifilm sensor technologies (Sony Exmor BSI, Canon Dual Pixel CMOS, Panasonic Micro Four Thirds, Sony 4-color RGBE/RCGB CCDs, Apple Linear DNG).

2. **Sensor Geometry & CFA Phase Alignment Testing**:
   - Benchmark how RAW decoders handle Bayer phase discrepancies (shifting `GRBG`, `GBRG`, or `BGGR` grids into standard `RGGB`).
   - Stress-test decoders with exotic sensor layouts such as the 4-color `RCGB` / `RGBE` sensor found in the Sony DSC-F828.
   - Verify optical black border removal via `ActiveArea` vs. `DefaultCropOrigin` tags.
   - Validate odd-coordinate crop origin handling (such as `crop_y = 7` on Nikon Coolpix P1000) through row parity normalization.
   - Analyze 3-channel Linear RGB to single-channel Bayer RGGB re-mosaicing for Apple ProRAW.

3. **Container Compatibility & Sensor Matrix Research**:
   - Investigate how RAW decoders validate sensor matrix dimensions and bitstreams:
     - When a file identifies as a medium-format **GFX 100S II** ($11,808 \times 8,754$, $\sim 208\text{ MB}$ uncompressed) vs. **GFX 50S II** ($8,472 \times 6,210$, $\sim 106\text{ MB}$) vs. compact Bayer APS-C **X-T200 / X-A3** ($6,016 \times 4,014$, $\sim 48\text{ MB}$).
   - Test decoder bit-depth expectations: preserving genuine 14-bit and 16-bit sensor data without lossy truncation.
   - Evaluate decoder decompression logic between packed 14-bit bitstreams (`msb32`) and uncompressed 16-bit word streams (`u16`).

4. **Metadata Sanitization & EXIF Tag Profiling**:
   - Examine how decoders parse camera model identifiers (`Model`, `FujiModel`), white balance gains (`WB_GRBLevels`), orientation tags (`IFD0:Orientation` and `IFD1:Orientation`), and baseline exposure compensation tags (`Tag 0x9650` / `0x9651`).
   - Cleanly sanitize and strip non-translatable donor hardware tags (`SerialNumber`, `InternalSerialNumber`, `LensSerialNumber`, `ImageUniqueID`).

---

## Architectural Highlights

- **Direct In-Memory RAW Decoding (`rawler`)**:
  Decodes proprietary camera files directly in RAM in **50–250 ms** — zero intermediate `.dng` files written to disk, avoiding SSD wear and eliminating external decoding dependencies.
- **Multi-Threaded Parallel Batch Engine (`rayon`)**:
  Equipped with an auto-scaling parallel worker pool (`--jobs <N>`). Batch-converts folders of RAW files concurrently in seconds.
- **Zero-Copy Template Caching (`Arc<GfxTemplate>`)**:
  Fujifilm reference containers are loaded and cached in memory once, then safely shared across all worker threads.
- **Dynamic Container Pool & Auto-Fitting**:
  Automatically selects the smallest container canvas that safely accommodates the input sensor's active area and bit depth.
- **Sub-10ms Parallel Pixel Rendering & SIMD LUT**:
  Dynamic range mapping from sensor black/white levels to Fujifilm standard levels using vectorized lookup tables and parallel row chunks.

---

## How It Works

```
[Input Camera RAW]  (Sony ARW, Canon CR2/CR3/CRW, Nikon NEF, DJI DNG, ...)
        │
        ▼  Step 1: Direct in-memory decode with rawler (50–250 ms)
[Linearized Bayer Pixel Data in RAM]
        ├─► Sensor geometry  (width, height, crop origin/size, active area)
        ├─► Radiometry       (black level, white level, bits per sample)
        ├─► CFA phase        (RGGB, GRBG, GBRG, BGGR, or non-standard RCGB)
        └─► White balance    (wb_coeffs, as_shot_neutral)
        │
        ▼  Step 2: RAF repacking (Parallel Rayon row rendering + LUT + ContainerPool)
[Fujifilm RAF Container]
        ├─► 148-byte header  (magic, camera ID, section offsets)
        ├─► Preview JPEG     (sanitized source preview stream)
        ├─► CFA metadata     (crop, dimensions, aspect ratio, bit-depth — tags 0x0100…0xCA00)
        ├─► Exposure bias    (Tag 0x9650 calibrated to -0.72 EV)
        └─► RAW TIFF block   (FujiSubIFD + packed or uncompressed Bayer pixels)
        │
        ▼  Step 3: Fast in-memory RAF validation (rawler / dnglab)
        │
        ▼  Step 4: Shooting metadata transfer (exiftool)
[Shooting metadata written into RAF EXIF; serial numbers stripped]
```

---

## Verified Camera Matrix

The conversion engine has been tested across 18 distinct camera systems:

| Camera Model | Format | Sensor Resolution | Bit Depth | CFA Layout | Best-Fitting Container |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Sony Alpha a7 III** | `.ARW` | 6000 × 4000 | 14-bit | RGGB | `X-T200` (24 MP) |
| **Sony Alpha a7R IV** | `.ARW` | 9504 × 6336 | 16-bit | RGGB | `GFX 100S II` (102 MP) |
| **Sony DSC-R1** | `.SR2` | 3888 × 2592 | 14-bit | GRBG | `X-T200` (24 MP) |
| **Sony DSC-F828** | `.SRF` | 3264 × 2448 | 14-bit | RCGB (4-color) | `X-T200` (24 MP) |
| **Canon EOS 5D Mark IV** | `.CR2` | 6720 × 4480 | 14-bit | RGGB | `GFX 50S II` (51.4 MP) |
| **Canon EOS 5D Mark III** | `.CR2` | 5760 × 3840 | 14-bit | RGGB | `X-T200` (24 MP) |
| **Canon EOS R** | `.CR3` | 6720 × 4480 | 14-bit | RGGB | `GFX 50S II` (51.4 MP) |
| **Canon EOS D60** | `.CRW` | 3088 × 2056 | 12-bit (scaled 14b) | RGGB | `X-T200` (24 MP) |
| **Nikon Z6** | `.NEF` | 6048 × 4024 | 14-bit | RGGB | `GFX 50S II` (51.4 MP) |
| **Nikon Coolpix P1000** | `.NRW` | 4608 × 3456 | 12-bit (scaled 14b) | RGGB | `X-T200` (24 MP) |
| **OM System OM-1** | `.ORF` | 5184 × 3888 | 12-bit (scaled 14b) | RGGB | `X-T200` (24 MP) |
| **Panasonic Lumix G9** | `.RW2` | 5184 × 3888 | 12-bit (scaled 14b) | RGGB | `X-T200` (24 MP) |
| **Panasonic Lumix GH5** | `.RW2` | 5184 × 3888 | 12-bit (scaled 14b) | RGGB | `X-T200` (24 MP) |
| **Ricoh GR III** | `.DNG` | 6000 × 4000 | 14-bit | RGGB | `X-T200` (24 MP) |
| **Apple iPhone 12 Pro** | `.DNG` | 4032 × 3024 | 16-bit | Linear RGB → RGGB | `GFX 100S II` (102 MP) |
| **DJI Mavic 3 Pro** | `.DNG` | 4024 × 3016 | 16-bit | GRBG | `GFX 100S II` (102 MP) |
| **Leica M9** | `.DNG` | 5212 × 3472 | 14-bit | RGGB | `X-T200` (24 MP) |

---

## Building from Source

### Prerequisites
- [Rust](https://rustup.rs/) (version 1.75 or later)
- (Optional) [ExifTool](https://exiftool.org/) for transferring camera shooting metadata.
- (Optional) [dnglab](https://github.com/dnglab/dnglab) for post-conversion DNG export/verification.

### Compilation
```powershell
# Clone the repository
git clone https://github.com/ip-web/Raw2Raf.git
cd Raw2Raf

# Build optimized release binary
cargo build --release
```

The compiled binary will be placed at `target/release/raw2raf.exe`.

---

## Usage Guide

### 1. Single-File Conversion

```powershell
# Basic conversion (auto-selects best-fitting container from container/ folder)
.\raw2raf.exe sample.ARW output.raf

# If output filename is omitted, defaults to <input>_converted.raf
.\raw2raf.exe sample.CR3

# Explicit container specification
.\raw2raf.exe sample.ARW output.raf --container container/Fuji_GFX50S_II_-_Uncompressed.RAF

# Fast conversion (skip metadata transfer and dnglab verification)
.\raw2raf.exe sample.ARW output.raf --no-metadata --no-verify
```

### 2. Recursive Batch Conversion

```powershell
# Recursively convert an entire folder of RAW files into an output directory:
.\raw2raf.exe --batch D:\Photos\Raw_Samples --output-dir D:\Converted_RAFs

# Overwrite existing files and use 16 worker threads:
.\raw2raf.exe --batch D:\Photos\Raw_Samples --output-dir D:\Converted_RAFs -j 16 --overwrite
```

### 3. Model & Metadata Overrides for Testing

```powershell
# Override model when a non-GFX container is selected (e.g. mapping 24MP X-T200 container to GFX50S II):
.\raw2raf.exe --batch D:\Photos --output-dir D:\Converted --non-gfx-model "GFX50S II"

# Global model override:
.\raw2raf.exe sample.ARW output.raf --model "GFX 100S II"

# Custom metadata tag injection:
.\raw2raf.exe sample.ARW output.raf --set Artist="Researcher" --set Copyright="Test Laboratory"
```

---

## Command Line Options

```
Usage: raw2raf.exe [OPTIONS] [INPUT_RAW] [OUTPUT_RAF]

Arguments:
  [INPUT_RAW]   Path to a single input RAW file (e.g. .ARW, .CR2, .CR3, .NEF)
  [OUTPUT_RAF]  Output .RAF path for single-file mode (optional; defaults to <input>_converted.raf)

Options:
      --batch <INPUT_DIR>              Recursively convert every RAW file found under INPUT_DIR
      --output-dir <DIR>               Root directory for batch output (mirrors subfolder structure)
      --overwrite                      Batch mode: overwrite existing .raf files
  -j, --jobs <JOBS>                    Number of worker threads (default: 0 = auto-detect CPU cores)
      --mode <MODE>                    Repack mode: 'native' (default), 'canvas', or 'unpadded'
      --exposure-bias <EV>             Raw baseline exposure bias in EV (default: -0.72)
      --preview <PATH>                 Path to a custom JPEG preview to embed
      --empty-preview                  Strip embedded preview and leave an empty blank placeholder
      --dnglab <PATH>                  Path to dnglab executable
      --container <PATH>...            Path(s) or directory of reference Fujifilm RAF container files
      --auto-container                 Automatically select smallest compatible Bayer container from pool
      --set <KEY=VALUE>...             Set custom metadata key-value pair(s)
      --model <MODEL>                  Override camera model metadata and RAF header
      --non-gfx-model <MODEL>          Override camera model specifically when a non-GFX container is selected
      --keep-dng                       Keep intermediate DNG file
      --no-verify                      Skip post-conversion verification
      --no-metadata                    Skip exiftool metadata transfer
  -h, --help                           Print help
  -V, --version                        Print version
```

---

## Container Setup

Fujifilm Bayer RAF containers serve as the structural envelope for repacked raw streams. For testing:
- Place reference `.RAF` container files (e.g. genuine uncompressed samples from `GFX 100S II`, `GFX 50S II`, `X-T200`, `X-A3`) in a `container/` directory.
- `Raw2Raf` dynamically parses container headers, preview blocks, and CFA metadata directories to construct structurally authentic RAF files.

---

## Disclaimer

This software is an independent research project developed for **technical evaluation, color profile testing, sensor layout benchmarking, and educational purposes**. Fujifilm, GFX, X-Trans, and related camera marks are trademarks of FUJIFILM Corporation. This project is not affiliated with, endorsed by, or sponsored by FUJIFILM Corporation.

---

## License

This project is licensed under the **GNU General Public License v3.0** (GPL-3.0). See the [LICENSE](LICENSE) file for the full license text.
