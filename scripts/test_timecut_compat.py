"""Run the seven native media compatibility tests with public, hash-pinned samples."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
OSXPHOTOS = (
    "https://raw.githubusercontent.com/RhetTbull/osxphotos/"
    "e51a1570d39bf999eb37434a63f741e909f95b30/tests/test-images"
)
NIKON = "https://www.rawsamples.ch/raws/nikon"
RAW, JPEG = "Photodesk 39.NEF", "Photodesk 39.jpg"
STILL, MOVIE = "IMG_0841.HEIC", "IMG_0841.mov"
SAMPLES = {
    STILL: (f"{OSXPHOTOS}/IMG_1853.HEIC", "8e6fac35b135ca6756ce8fb3368ec4dc3613ea5c0db34aa1eeefd3988e26c625"),
    MOVIE: (f"{OSXPHOTOS}/IMG_1853.mov", "5d24de6ae010d9bdaff559ea8f736923967cbc8e3e382cc0489a5673adec436d"),
    RAW: (f"{NIKON}/d300/RAW_NIKON_D300.NEF", "119728b0d1aa1d1556d9da2eb2212cd2f4b713eb5ac494cb51a988b6a65765d2"),
    "nikon-e5700.nef": (f"{NIKON}/e5700/RAW_NIKON_E5700_SRGB.NEF", "232a78df1610ca3992b4eaac0f23046eee1d30fc517300862718eccb474a1239"),
}
TESTS = (
    "t_cmds::clipboard_pair_tests::external_media_pair_samples",
    "t_dedup::keep_rules_tests::external_sample_keep_rules",
    "t_image::offline_preview_tests::raw_fixture_batch_orientation",
    "t_image::offline_preview_tests::nikon_e5700_thumbnail_matches_preview_orientation",
    "t_sqlite::file_refresh_policy_tests::explicit_refresh_backfills_lens_on_unchanged_files",
    "t_utils::import_file_concurrency_tests::external_folder_import_pairs",
    "t_image::offline_preview_tests::editable_raw_preview_falls_back_to_embedded_pixels",
)


def cached_sample(cache, url, checksum):
    target = cache / checksum
    if target.is_file() and hashlib.sha256(target.read_bytes()).hexdigest() == checksum:
        return target
    print(f"Downloading {url}", flush=True)
    with tempfile.NamedTemporaryFile(dir=cache, delete=False) as download:
        staging = Path(download.name)
    try:
        with urllib.request.urlopen(url, timeout=60) as response, staging.open("wb") as output:
            shutil.copyfileobj(response, output)
        if hashlib.sha256(staging.read_bytes()).hexdigest() != checksum:
            raise ValueError(f"Unexpected SHA-256 while downloading {url}")
        staging.replace(target)
    finally:
        staging.unlink(missing_ok=True)
    return target


def sample_file(file_id, folder, sample, subtype=None, companion=None):
    return dict(id=file_id, folder=folder, sample=sample, created=100 + file_id,
                modified=100 + file_id, subtype=subtype, companion=companion)


def keep_case(name, files, keep, remove, *, similar=False, components=()):
    return dict(name=name, folders=[[1, "preferred"], [2, "secondary"]], files=files,
                rules=dict(folderIds=[1, 2], preferFormat="ignore", preferredDate="createdEarliest",
                           preferredSize="ignore"), group_raw_pairs=False, similar=similar,
                expected_keep=[keep], expected_remove=[remove], expected_components=components)


def prepare(cache, folder):
    cache.mkdir(parents=True, exist_ok=True)
    for name, (url, checksum) in SAMPLES.items():
        shutil.copyfile(cached_sample(cache, url, checksum), folder / name)
    thumbs = ROOT / "docs/public/demos/lap/thumbnails"
    shutil.copyfile(thumbs / "01.jpg", folder / JPEG)
    shutil.copyfile(thumbs / "02.jpg", folder / "separate.jpg")
    # Preserve the real embedded JPEG and truncate the D300's sensor data.
    with (folder / RAW).open("rb") as source:
        (folder / "embedded-only.nef").write_bytes(source.read(1_267_292))

    pair_cases = [
        dict(name="native HEIC Live Photo", files=[STILL, MOVIE], group_raw_pairs=False, groups=[[0, 1]]),
        dict(name="unrelated JPEG and movie", files=["separate.jpg", MOVIE], group_raw_pairs=True, groups=[[0], [1]]),
        dict(name="RAW and JPEG grouped", files=[RAW, JPEG], group_raw_pairs=True, groups=[[0, 1]]),
        dict(name="RAW and JPEG separate", files=[RAW, JPEG], group_raw_pairs=False, groups=[[0], [1]]),
    ]
    dedup_cases = [
        keep_case("exact JPEG duplicate", [sample_file(1, 1, "separate.jpg"), sample_file(2, 2, "separate.jpg")], 1, 2),
        keep_case("similar group folder priority", [sample_file(1, 2, JPEG), sample_file(2, 1, "separate.jpg")], 2, 1,
                  similar=True),
        keep_case("independent Live Photo duplicates", [
            sample_file(1, 1, STILL, "live_photo", 2), sample_file(2, 1, MOVIE),
            sample_file(3, 2, STILL, "live_photo", 4), sample_file(4, 2, MOVIE),
        ], 1, 3, components=[[3, 4]]),
    ]
    (folder / "pair-cases.json").write_text(json.dumps(pair_cases), encoding="utf-8")
    (folder / "dedup-cases.json").write_text(json.dumps(dict(samples=str(folder), cases=dedup_cases)), encoding="utf-8")
    (folder / "raw-paths.txt").write_text(f"{folder / 'nikon-e5700.nef'}\n{folder / RAW}\n", encoding="utf-8")


def run_test(binary, folder, name):
    with tempfile.TemporaryDirectory() as runtime:
        env = os.environ.copy()
        env.pop("TIMECUT_DATA_DIR", None)
        for key in ("TEMP", "TMP", "TMPDIR", "LOCALAPPDATA", "XDG_DATA_HOME", "XDG_CACHE_HOME", "HOME"):
            env[key] = runtime
        preview = ("embedded-only.nef" if name.endswith("::editable_raw_preview_falls_back_to_embedded_pixels")
                   else "nikon-e5700.nef")
        env.update(
            LAP_MEDIA_PAIR_CASES=str(folder / "pair-cases.json"),
            LAP_MEDIA_PAIR_SAMPLES=str(folder),
            LAP_DEDUP_SAMPLE_SUITE=str(folder / "dedup-cases.json"),
            LAP_RAW_ORIENTATION_MANIFEST=str(folder / "raw-paths.txt"),
            LAP_NIKON_LENS_SAMPLES=str(folder / RAW),
            LAP_RAW_PREVIEW_FIXTURE=str(folder / preview),
        )
        if os.name == "nt" and env.get("ORT_LIB_LOCATION"):
            ort = Path(env["ORT_LIB_LOCATION"])
            env["PATH"] = os.pathsep.join([str(ort), str(ort / "lib"), env["PATH"]])
        result = subprocess.run([binary, "--ignored", "--exact", name], env=env, cwd=ROOT,
                                capture_output=True, text=True, errors="replace", timeout=180)
        passed = (result.returncode == 0 and f"test {name} ... ok" in result.stdout
                  and "1 passed; 0 failed" in result.stdout)
        print(f"{'PASS' if passed else 'FAIL'}  {name}", flush=True)
        if not passed:
            print(f"Exit {result.returncode}\n{result.stdout}{result.stderr}", file=sys.stderr)
        return passed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("test_binary", type=Path, help="exact test executable printed by cargo test --no-run")
    parser.add_argument("--cache", type=Path, default=ROOT / ".timecut-build/public-media-cache",
                        help="directory for hash-verified downloads")
    args = parser.parse_args()
    binary = args.test_binary.resolve(strict=True)
    with tempfile.TemporaryDirectory() as temp:
        folder = Path(temp)
        prepare(args.cache, folder)
        passed = sum(run_test(binary, folder, name) for name in TESTS)
    print(f"{passed}/{len(TESTS)} external media compatibility tests passed.")
    return 0 if passed == len(TESTS) else 1


if __name__ == "__main__":
    sys.exit(main())
