#!/usr/bin/env bash
# Extract both matching source ZIPs in the same empty directory first.
# Run on a Linux x86_64 Docker buildx host with Python 3 and GNU tar.
set -euo pipefail
cd "$(dirname "$0")"
mkdir recipe
tar -xzf FFmpeg-Builds.tar.gz --strip-components=1 -C recipe
mkdir -p recipe/.cache
mv downloads recipe/.cache/
cd recipe
# The archived upstream build normally clones a moving release branch.
# Build this copy from the exact release source archive supplied alongside it.
python3 - <<'PY'
from pathlib import Path
script = Path("build.sh").read_text()
clone = "    git clone --filter=blob:none --branch='$GIT_BRANCH' '$FFMPEG_REPO' ffmpeg"
if script.count(clone) != 1 or script.count("mkdir ffbuild\n") != 1:
    raise SystemExit("Unexpected upstream build recipe")
script = script.replace("mkdir ffbuild\n", "mkdir ffbuild\ncp ../FFmpeg.tar.gz ffbuild/source.tar.gz\n")
script = script.replace(clone, "    mkdir ffmpeg\n    tar -xzf /ffbuild/source.tar.gz --strip-components=1 -C ffmpeg")
Path("build-supplied-source.sh").write_text(script)
PY
./makeimage.sh win64 gpl 8.1
bash ./build-supplied-source.sh win64 gpl 8.1
