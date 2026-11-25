"""Prepare the two hash-pinned Windows x64 executables from a distributor or Time-cut ZIP."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import urllib.error
import urllib.request
import zipfile

RELEASE = json.loads(Path(__file__).with_name("windows_ffmpeg.json").read_text(encoding="utf8"))
TRIPLE = "x86_64-pc-windows-msvc"
PREVIOUS = {
    "ffmpeg": "1a65d5b0b10d8d9a81d2824a3538046a40ed3607c906b335a166add87613f705",
    "ffprobe": "54b944d6095c4588d7548424d7acd5118390a9d4f01b1c92f841547fc9a7429c",
}
PORTABLE = ("https://github.com/code-lawyer/time-cut/releases/download/timecut-v0.1.0-preview.39/"
            "Time-cut-0.1.0-preview.39-windows-x64.zip")


def sha256(file):
    with file.open("rb") as src:
        return hashlib.file_digest(src, "sha256").hexdigest()


def verify(file, expected):
    if sha256(file) != expected:
        raise ValueError(f"SHA-256 mismatch for {file.name}")


def prepare(target, archive=None, verify_only=False):
    pending = {}
    for name, previous in PREVIOUS.items():
        dest = target / f"{name}-{TRIPLE}.exe"
        if dest.exists():
            actual = sha256(dest)
            if actual == RELEASE[name + "Sha256"]:
                continue
            if verify_only or actual != previous:
                raise ValueError(f"SHA-256 mismatch for {dest.name}")
        elif verify_only:
            raise FileNotFoundError(f"Missing sidecar: {dest.name}")
        pending[name] = dest
    if not pending:
        return
    target.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=target, prefix=".ffmpeg-") as temporary:
        temp = Path(temporary)
        if not archive:
            archive = temp / "distribution.zip"
            try:
                urllib.request.urlretrieve(RELEASE["distribution"] + "/" + RELEASE["archive"], archive)
                verify(archive, RELEASE["archiveSha256"])
            except urllib.error.HTTPError as error:
                if error.code not in (404, 410):
                    raise
                # Daily upstream builds expire. This release preserves both pinned executables.
                urllib.request.urlretrieve(PORTABLE, archive)
        upstream = Path(RELEASE["archive"]).stem
        with zipfile.ZipFile(archive) as zip_file:
            for name, dest in pending.items():
                entries = [entry for entry in zip_file.infolist() if entry.filename == f"{upstream}/bin/{name}.exe"
                           or entry.filename.endswith(f"/ffmpeg/{dest.name}")]
                if len(entries) != 1:
                    raise ValueError(f"Expected one {name} executable in the ZIP")
                with zip_file.open(entries[0]) as src, (temp / name).open("wb") as out:
                    shutil.copyfileobj(src, out)
                verify(temp / name, RELEASE[name + "Sha256"])
        # Both files have passed before the first cache entry can change.
        for name, dest in pending.items():
            os.replace(temp / name, dest)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", type=Path)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--verify-only", action="store_true")
    args = parser.parse_args()
    try:
        prepare(args.target, args.archive, args.verify_only)
    except (OSError, ValueError, zipfile.BadZipFile) as error:
        sys.exit(str(error))
