# Scripts

Utility scripts for the Lap project.

## Windows phone import acceptance

For an existing Time-cut profile with one archived phone, `Time-cut.exe --resume-verify-phone N` enables automatic import into the configured archive and requests a verification scan once at least `N` items are visible. Set `N` to the count from the earlier device scan. The normal tray application owns the request across launches. A second complete scan must add zero files and match every source path and content digest, including companion files. The bounded desktop test in `test_timecut_iphone.mjs` remains available for interactive diagnostics.

## 1. download_models

Downloads CLIP, YuNet and SFace to `src-tauri/resources/models/`. Both scripts pin model revisions and verify SHA-256 for cached files and new downloads. The YuNet and SFace license texts are downloaded and verified alongside their weights.

### Usage

**macOS / Linux:**
```bash
./scripts/download_models.sh
```

**Windows (PowerShell):**
```powershell
.\scripts\download_models.ps1
```

### Models Downloaded

| Model | Source | Purpose |
|-------|--------|---------|
| `tokenizer.json` | OpenAI CLIP ViT-B/32 | Text tokenization |
| `text_model.onnx` | CLIP ViT-B/32 (quantized) | Text embedding |
| `vision_model.onnx` | CLIP ViT-B/32 (quantized) | Image embedding |
| `face_detection_yunet_2026may.onnx` | OpenCV Zoo YuNet | Face detection |
| `face_recognition_sface_2021dec.onnx` | OpenCV Zoo SFace | Face grouping |

## 2. download_ffmpeg_sidecar

Downloads FFmpeg and FFprobe sidecar binaries for the current platform into `src-tauri/resources/ffmpeg/`. This ensures the packaged app can include the sidecar binaries with releases built from source.

The Windows PowerShell downloader pins GitHub asset IDs and verifies SHA-256 for cached and new x64 and ARM64 executables. The Bash script checks the same Windows assets as well as both supported Linux architectures. macOS downloads use the release URLs. Fresh downloads fail on HTTP errors and use temporary files before promotion. See [build-input provenance and checksum maintenance](../contrib/aur/README.md); this does not add Arch ARM packaging support.

### Usage

**macOS / Linux:**
```bash
./scripts/download_ffmpeg_sidecar.sh
```

**Windows (PowerShell):**
```powershell
.\scripts\download_ffmpeg_sidecar.ps1
```

### Sidecar Files Downloaded

| Platform | Files |
|----------|-------|
| macOS Intel | `ffmpeg-x86_64-apple-darwin`, `ffprobe-x86_64-apple-darwin` |
| macOS Apple Silicon | `ffmpeg-aarch64-apple-darwin`, `ffprobe-aarch64-apple-darwin` |
| Linux x86_64 | `ffmpeg-x86_64-unknown-linux-gnu`, `ffprobe-x86_64-unknown-linux-gnu` |
| Linux aarch64 | `ffmpeg-aarch64-unknown-linux-gnu`, `ffprobe-aarch64-unknown-linux-gnu` |
| Windows x86_64 | `ffmpeg-x86_64-pc-windows-msvc.exe`, `ffprobe-x86_64-pc-windows-msvc.exe` |

## Downloader integrity regression tests

Run `python -m unittest discover` from the repository root to discover the downloader suite. For verbose downloader-only output, use `python -m unittest discover -s scripts/tests -v`.
These tests execute the actual Bash downloaders in temporary layouts, replacing only the network transport to exercise corrupt caches, corrupt successful responses, and failed transfers. Real upstream clean-download/cache smoke evidence is recorded in the Phase 1 verification artifact under `.context/`.

`.github/workflows/test-downloaders.yml` runs Bash syntax checks and the downloader-only suite on Linux and macOS for pull requests and `main` pushes that change `scripts/**` or that workflow. It also supports manual dispatch. The suite uses no upstream downloads or build secrets.

## Native media compatibility tests

From the repository root, build the native test executable in the project's configured Rust build environment. Use the executable path printed by Cargo as the argument to the Python 3.8+ runner.

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked --release --no-run
python scripts/test_timecut_compat.py "<test executable path printed by Cargo>"
```

The first run downloads a real iPhone HEIC/MOV Live Photo pair from the [osxphotos test collection](https://github.com/RhetTbull/osxphotos/tree/e51a1570d39bf999eb37434a63f741e909f95b30/tests/test-images) and Nikon D300 and E5700 NEFs from [RAW-Samples](https://www.rawsamples.ch/). Each download is pinned by SHA-256 and cached in `.timecut-build/public-media-cache`. A populated cache supports offline runs. The runner stages copies in a temporary folder, adds two repository JPEGs for companion and deduplication scenarios, and truncates a copy of the D300 file to exercise the embedded-preview fallback. It executes all seven ignored tests in isolated processes and reports each result.
