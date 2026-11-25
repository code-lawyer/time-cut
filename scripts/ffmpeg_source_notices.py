"""Extract license notices from source archives used by the pinned Windows FFmpeg build."""
import argparse
import concurrent.futures
import hashlib
from pathlib import Path, PurePosixPath
import re
import tarfile
import zipfile

LEGAL = re.compile(r"^(?:licen[cs]e|copying|copyright|notice|patents)(?:$|[.\-_ ].*)", re.I)


def notices(item):
    bundle, entry = item
    docs = {}
    with zipfile.ZipFile(bundle) as zip_file, zip_file.open(entry) as src, tarfile.open(fileobj=src, mode="r|xz") as tar:
        for member in tar:
            path = PurePosixPath(member.name)
            if ("/50-ffnvcodec_" in entry and member.isfile() and path.suffix == ".h"
                    and "include" in path.parts and ".git" not in path.parts):
                header = tar.extractfile(member).read(16384).decode("utf8", errors="replace")
                if header.startswith("/*") and "*/" in header and "copyright" in header.split("*/")[0].lower():
                    docs[str(path)] = header.split("*/", 1)[0] + "*/"
                continue
            if (not member.isfile() or ".git" in path.parts or not 0 < member.size <= 1_000_000
                    or (not LEGAL.fullmatch(path.name) and "licenses" not in [p.lower() for p in path.parts[:-1]])):
                continue
            data = tar.extractfile(member).read()
            if b"\0" in data:
                continue
            text = data.decode("utf-8-sig", errors="replace").replace("\r\n", "\n").strip()
            if text:
                docs[str(path)] = text
    return PurePosixPath(entry).name.rsplit("_", 1)[0], docs


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundles", nargs="+", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    entries = []
    for bundle in args.bundles:
        with zipfile.ZipFile(bundle) as zip_file:
            entries.extend((bundle, item) for item in zip_file.namelist() if item.startswith("downloads/"))
    files = []
    texts = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        for name, docs in sorted(pool.map(notices, entries)):
            files.append("\n" + name)
            for filename, text in sorted(docs.items()):
                sha = hashlib.sha256(text.encode()).hexdigest()
                texts[sha] = text
                files.append(f"  {filename} [{sha}]")
    args.output.write_text("\n".join([
        "FFmpeg Windows x64 dependency notices",
        "Source inputs of upstream FFmpeg-Builds run 45390806074 for win64 gpl 8.1.",
        "Includes build tools. Original files and full sources accompany this release.",
        *files, "", "FULL TEXTS", *(f"\n[{sha}]\n{body}" for sha, body in sorted(texts.items()))
    ]) + "\n", encoding="utf8", newline="\n")
    print(f"{len(entries)} source inputs, {len(texts)} distinct texts, {args.output.stat().st_size} bytes.")
