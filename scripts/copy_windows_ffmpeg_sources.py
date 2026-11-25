"""Copy the pinned FFmpeg source parts from a public release to a release draft."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile

from fetch_timecut_ffmpeg import RELEASE_TAG, verify


def read_sums(path):
    parts = {}
    for line in path.read_text(encoding="ascii").splitlines():
        digest, name = line.split()
        if (name in parts or not re.fullmatch(r"[0-9a-f]{64}", digest)
                or not re.fullmatch(r"FFmpeg-[A-Za-z0-9.-]+-win64-source-part[1-9][0-9]*\.zip", name)):
            raise ValueError(f"Invalid FFmpeg source checksum line: {line}")
        parts[name] = digest
    prefixes = {name.rsplit("-part", 1)[0] for name in parts}
    if len(prefixes) != 1:
        raise ValueError("Expected one FFmpeg source archive prefix")
    return parts, next(iter(prefixes)) + ".sha256"


def uploaded(release):
    return {asset["name"]: asset["digest"] for asset in release["assets"] if asset["state"] == "uploaded"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository")
    parser.add_argument("draft_tag")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repository) or not RELEASE_TAG.fullmatch(args.draft_tag):
        parser.error("Expected an owner/repository and a timecut-v release tag")

    sums = Path(__file__).with_name("windows_ffmpeg_sources.sha256")
    parts, sums_name = read_sums(sums)
    expected = {name: "sha256:" + digest for name, digest in parts.items()}
    expected[sums_name] = "sha256:" + hashlib.sha256(sums.read_bytes()).hexdigest()
    pages = json.loads(subprocess.check_output([
        "gh", "api", "--paginate", "--slurp", f"repos/{args.repository}/releases?per_page=100",
    ]))
    releases = [release for page in pages for release in page]
    draft = next((release for release in releases if release["draft"] and release["tag_name"] == args.draft_tag), None)
    if draft is None:
        raise ValueError(f"Could not find draft {args.draft_tag}")
    existing = uploaded(draft)
    pending = {name: digest for name, digest in expected.items() if existing.get(name) != digest}
    if not pending:
        print("Draft already has all verified FFmpeg source assets.")
        return
    if any(name in existing for name in pending):
        raise ValueError("Draft has conflicting FFmpeg source assets; check the draft before replacing them")

    needed_parts = parts.keys() & pending.keys()
    source = None
    if needed_parts:
        for release in sorted(releases, key=lambda item: item.get("published_at") or "", reverse=True):
            if release["draft"] or not RELEASE_TAG.fullmatch(release["tag_name"]):
                continue
            assets = uploaded(release)
            if all(assets.get(name) == expected[name] for name in parts):
                source = release["tag_name"]
                break
        if source is None:
            raise ValueError("No public release has the pinned FFmpeg sources; attach matching parts to the draft manually")

    with tempfile.TemporaryDirectory() as temporary:
        folder = Path(temporary)
        for name in sorted(needed_parts):
            print(f"Downloading {name} from {source}", flush=True)
            subprocess.run([
                "gh", "release", "download", source, "--repo", args.repository, "--dir", folder, "--pattern", name,
            ], check=True)
            verify(folder / name, parts[name])
        if sums_name in pending:
            (folder / sums_name).write_bytes(sums.read_bytes())
        print(f"Uploading {len(pending)} verified FFmpeg source assets to {args.draft_tag}", flush=True)
        subprocess.run([
            "gh", "release", "upload", args.draft_tag, "--repo", args.repository, *(folder / name for name in pending),
        ], check=True)


if __name__ == "__main__":
    main()
