"""Export a Git revision and each recorded submodule, independent of worktree files."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import tempfile


def git(root, *args):
    return subprocess.check_output(["git", "-C", root, *args])


def export(repository, revision, prefix, output, submodules):
    with tempfile.TemporaryFile() as contents:
        subprocess.run(["git", "-C", repository, "archive", "--format=tar", revision], stdout=contents, check=True)
        contents.seek(0)
        with tarfile.open(fileobj=contents) as tree:
            for member in tree:
                # Git emits a directory for a submodule, followed here by its recorded contents.
                member.name = str(prefix / member.name)
                output.addfile(member, tree.extractfile(member) if member.isfile() else None)
    for record in git(repository, "ls-tree", "-r", "-z", revision).split(b"\0"):
        if not record.startswith(b"160000 commit "):
            continue
        header, name = record.split(b"\t", 1)
        name = name.decode()
        child = repository / name
        if not (child / ".git").exists():
            raise ValueError(f"Initialize the submodule at {name} before exporting its source")
        commit = header.split()[2].decode()
        submodules[str(prefix / name)] = commit
        export(child, commit, prefix / name, output, submodules)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--revision", default="HEAD")
    args = parser.parse_args()
    project = args.project.resolve()
    commit = git(project, "rev-parse", "--verify", args.revision + "^{commit}").decode().strip()
    version = json.loads(git(project, "show", commit + ":src-tauri/tauri.conf.json"))["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("Invalid release version")
    epoch = int(git(project, "show", "-s", "--format=%ct", commit))
    label = "Time-cut-" + version + "-source"
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    destination = output / (label + ".tar.gz")
    submodules = {}
    with tempfile.NamedTemporaryFile(dir=output, delete=False) as raw:
        temporary = Path(raw.name)
    try:
        with temporary.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as gzipfile:
            with tarfile.open(mode="w|", fileobj=gzipfile) as archive:
                export(project, commit, PurePosixPath(label), archive, submodules)
                paths = {name.removeprefix(label + "/"): sha for name, sha in submodules.items()}
                source = {"version": version, "commit": commit, "submodules": paths}
                metadata = json.dumps(source, ensure_ascii=False, sort_keys=True, indent=2).encode()
                member = tarfile.TarInfo(label + "/TIME-CUT-SOURCE.json")
                member.size, member.mtime = len(metadata), epoch
                archive.addfile(member, io.BytesIO(metadata))
        with temporary.open("rb") as raw:
            digest = hashlib.file_digest(raw, "sha256").hexdigest()
        os.replace(temporary, destination)
        Path(str(destination) + ".sha256").write_text(f"{digest}  {destination.name}\n", encoding="ascii", newline="\n")
        print(f"{destination.name}  {digest}  ({len(submodules)} submodules)")
    finally:
        temporary.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
