"""Package the exact FFmpeg source, recipes and enabled dependency archives from the upstream build."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request
import zipfile

TOOLS = Path(__file__).resolve().parent
LOCK = json.loads((TOOLS / "windows_ffmpeg.json").read_text(encoding="utf8"))
LABEL = "FFmpeg-8.1.3-a8029b65-win64-source"
PART_LIMIT = 1_200_000_000


def digest(path):
    with path.open("rb") as src:
        return hashlib.file_digest(src, "sha256").hexdigest()


def get(directory, name, expected, url=None):
    path = directory / name
    if not path.exists():
        with tempfile.NamedTemporaryFile(dir=directory, delete=False) as raw:
            download = Path(raw.name)
        try:
            if url:
                urllib.request.urlretrieve(url, download)
            else:
                with download.open("wb") as out:
                    subprocess.run(["gh", "api", LOCK["dependencyArtifact"]], stdout=out, check=True)
            if digest(download) != expected:
                raise ValueError("Unverified source download: " + name)
            os.replace(download, path)
        finally:
            download.unlink(missing_ok=True)
    if digest(path) != expected:
        raise ValueError("Source archive checksum differs: " + name)
    return path


def member(zip_file, name, source, size):
    # A constant member timestamp makes identical release source inputs reproduce the same ZIP.
    item = zipfile.ZipInfo(name)
    item.file_size = size
    item.external_attr = 0o100644 << 16
    with zip_file.open(item, "w", force_zip64=True) as out:
        shutil.copyfileobj(source, out, length=1024 * 1024)


def package(cache, output):
    recipe = get(cache, "FFmpeg-Builds.tar.gz", LOCK["buildSourceSha256"],
                 "https://codeload.github.com/FFmpeg-Builds/FFmpeg-Builds/tar.gz/" + LOCK["buildCommit"])
    source = get(cache, "FFmpeg.tar.gz", LOCK["ffmpegSourceSha256"],
                 "https://codeload.github.com/FFmpeg/FFmpeg/tar.gz/" + LOCK["ffmpegCommit"])
    inputs = get(cache, "upstream-download-cache.zip", LOCK["dependencyArtifactSha256"])
    with tempfile.TemporaryDirectory(dir=output) as directory:
        stage = Path(directory)
        with tarfile.open(recipe) as tar:
            tar.extractall(stage, filter="data")
        checkout = stage / ("FFmpeg-Builds-" + LOCK["buildCommit"])
        bash = str(Path(shutil.which("git")).resolve().parents[1] / "bin/bash.exe") if os.name == "nt" else "bash"
        enabled = subprocess.check_output([bash, str(TOOLS / "ffmpeg_enabled_sources.sh")], cwd=checkout, text=True)
        needed = {".cache/downloads/" + name for name in enabled.splitlines()}
        if not needed or any(not name.endswith(".tar.xz") for name in needed):
            raise ValueError("No upstream source selection")
        number = 1
        def part():
            return zipfile.ZipFile(stage / f"{LABEL}-part{number}.zip", "x")
        current = part()
        try:
            total = 0
            for file in (source, recipe, TOOLS / "rebuild_ffmpeg_from_source.sh", TOOLS / "windows_ffmpeg.json"):
                with file.open("rb") as data:
                    member(current, file.name, data, file.stat().st_size)
                total += file.stat().st_size
            with zipfile.ZipFile(inputs) as upstream, upstream.open("cache.tar.gz") as data, tarfile.open(fileobj=data, mode="r|gz") as tar:
                for entry in tar:
                    if entry.name not in needed:
                        continue
                    if not entry.isfile() or entry.size > PART_LIMIT:
                        raise ValueError("Unexpected upstream source entry: " + entry.name)
                    if total + entry.size > PART_LIMIT:
                        current.close()
                        number += 1
                        current = part()
                        total = 0
                    with tar.extractfile(entry) as src:
                        member(current, entry.name.removeprefix(".cache/"), src, entry.size)
                    needed.remove(entry.name)
                    total += entry.size
        finally:
            current.close()
        if needed:
            raise ValueError("Upstream build cache is missing enabled sources: " + ", ".join(sorted(needed)))
        sums = []
        for zip_name in sorted(stage.glob(LABEL + "-part*.zip")):
            sums.append(digest(zip_name) + "  " + zip_name.name)
            os.replace(zip_name, output / zip_name.name)
        (output / (LABEL + ".sha256")).write_text("\n".join(sums) + "\n", encoding="ascii", newline="\n")
        print("\n".join(sums))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for folder in args.cache, args.output:
        folder.mkdir(parents=True, exist_ok=True)
    package(args.cache.resolve(), args.output.resolve())
