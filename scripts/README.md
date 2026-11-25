# Scripts

Utility scripts for the Lap project.

## Windows phone import acceptance

For a connected Android phone, select **File transfer** and run the packaged EXE against an isolated local archive. Pass the exact name printed by `Time-cut.exe --probe-devices`.

```powershell
node scripts/test_timecut_phone.mjs <exe> --android-device "Galaxy S21 5G"
```

The runner prints the test directory under `.timecut-build/android-full-*`. To resume a stopped run, insert that directory after the EXE argument. The full test checks intake, local filing, per-file BLAKE3, album membership, a second scan of the same phone, local-folder deduplication and on-device media decoding on the PC. To check the Windows Shell view independently, capture a snapshot with `measure_timecut_android_inventory.ps1` and compare it offline with an explicit completed run ID.

For an independent Android album acceptance, capture the album inventory while Time-cut is idle, then supply it to the runner.

```powershell
pwsh -STA -File scripts/measure_timecut_android_inventory.ps1 -Scope Gallery -Snapshot .timecut-build/gallery-shell.json -DeviceName "Galaxy S21 5G"
node scripts/test_timecut_phone.mjs <exe> --android-device "Galaxy S21 5G" --gallery-snapshot .timecut-build/gallery-shell.json
```

The runner compares both complete scans with every path and byte length in the independent snapshot. It also checks that the current verified receipts have been filed. An upgrade test can reuse a copied archive from an older import; unrelated receipts from the earlier scope remain in its inbox.

For a stalled Android import, close the isolated application to release the USB session, then run `verify_android_source_samples.ps1 -ArchiveRoot <archive> -RunId <id> -InboxOnly` under `pwsh -STA`. The default sample compares source and inbox bytes. Add `-SampleGroup Pending` to inspect unresolved files. A matching sample establishes byte integrity for the selected file.

To test additions, duplicates, same-name files, sidecars and a same-length replacement on a Samsung phone, use `test_android_mtp_fixture.ps1` with the same local receipt throughout. Run `prepare` offline, then `seed`. Build the empty-directory helper and run its `bind` command while the ownership marker is still present. Scan the seeded phone, run `mutation`, then scan again. On a successful whole-device baseline, `verify_android_fixture_run.py` compares the three completed run IDs with the receipt. When a separate file blocks the whole device, the opt-in Rust test `owned_android_fixture_capture_and_repeat` can exercise the generated directory on its own. Run that test once after seed and once after mutation. Finish with the fixture script's `cleanup` and the bound directory helper's `remove`. The scripts' header examples show the arguments.

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

Run `python -m unittest discover -s scripts/tests -v` from the repository root. The tests execute the Bash downloaders in temporary directories and replace the network transport to exercise corrupt caches, corrupt successful responses and failed transfers. On Windows they also exercise the PowerShell sidecar downloader and check that rebuilding a portable ZIP excludes stale files.

`.github/workflows/test-downloaders.yml` runs on Windows, Linux and macOS for pull requests and `main` pushes that change `scripts/**` or the workflow. It also supports manual dispatch.

## Native media compatibility tests

From the repository root, build the native test executable in the project's configured Rust build environment. Use the executable path printed by Cargo as the argument to the Python 3.8+ runner.

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked --release --no-run
python scripts/test_timecut_compat.py "<test executable path printed by Cargo>"
```

The first run downloads a real iPhone HEIC/MOV Live Photo pair from the [osxphotos test collection](https://github.com/RhetTbull/osxphotos/tree/e51a1570d39bf999eb37434a63f741e909f95b30/tests/test-images) and Nikon D300 and E5700 NEFs from [RAW-Samples](https://www.rawsamples.ch/). Each download is pinned by SHA-256 and cached in `.timecut-build/public-media-cache`. A populated cache supports offline runs. The runner stages copies in a temporary folder, adds two repository JPEGs for companion and deduplication scenarios, and truncates a copy of the D300 file to exercise the embedded-preview fallback. It executes all seven ignored tests in isolated processes and reports each result.
