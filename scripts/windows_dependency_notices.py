"""Collect license texts from the resolved Windows Cargo graph and frozen pnpm installation."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


TARGET = "x86_64-pc-windows-msvc"
NOTICE = re.compile(r"^(?:licen[sc]e|copying|copyright|notice|unlicense|third[-_ ]?party[-_ ]?notices?)(?:$|[._-])", re.I)
TEXT_SUFFIXES = {"", ".txt", ".md", ".rst", ".html", ".spdx", ".ijg", ".apache", ".mit", ".lgpl", ".cddl", ".zlib", ".bsd", ".gpl", ".mpl"}


def contents(filename):
    data = filename.read_bytes()
    if data.startswith((b"\xff\xfe", b"\xfe\xff")):
        text = data.decode("utf-16")
    else:
        try:
            text = data.decode("utf-8-sig")
        except UnicodeDecodeError:
            text = data.decode("latin-1")
    if "\0" in text:
        raise ValueError(f"Unexpected binary notice: {filename}")
    return text.replace("\r\n", "\n").replace("\r", "\n").strip()


def notice_files(directory):
    for base, folders, files in os.walk(directory):
        folders[:] = sorted(name for name in folders if name not in {"node_modules", ".git", "target"})
        parent = Path(base)
        notice_dir = any(name.lower() in {"licenses", "licences"} for name in parent.relative_to(directory).parts)
        for name in sorted(files):
            path = parent / name
            # Extensionless files such as LICENSE-MIT and LICENSE-APACHE are common.
            suffix = path.suffix.lower()
            if (NOTICE.match(name) or notice_dir) and (suffix in TEXT_SUFFIXES or re.fullmatch(r"\.\d+", suffix)):
                yield path


def packages_rust(project):
    manifest = project / "src-tauri/Cargo.toml"
    data = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1",
                                               "--filter-platform", TARGET, "--manifest-path", manifest]))
    nodes = {node["id"]: node for node in data["resolve"]["nodes"]}
    root = data["resolve"]["root"]
    reachable, pending = set(), [root]
    while pending:
        package = pending.pop()
        if package in reachable:
            continue
        reachable.add(package)
        pending.extend(dep["pkg"] for dep in nodes[package]["deps"]
                       if any(kind["kind"] != "dev" for kind in dep["dep_kinds"]))
    for pkg in data["packages"]:
        if pkg["id"] == root or pkg["id"] not in reachable:
            continue
        directory = Path(pkg["manifest_path"]).parent
        origin = pkg["source"] or "workspace/" + directory.relative_to(project).as_posix()
        yield ("Rust", pkg["name"], pkg["version"]), directory, pkg.get("license"), origin, pkg.get("repository"), pkg.get("authors"), pkg.get("license_file")


def packages_js(project):
    source = project / "src-vite"
    lock = source / "node_modules/.pnpm/lock.yaml"
    if not lock.is_file() or contents(lock) != contents(source / "pnpm-lock.yaml"):
        raise ValueError("Run pnpm install --frozen-lockfile in src-vite before packaging.")
    pnpm = shutil.which("pnpm")
    command = [pnpm] if pnpm else [shutil.which("corepack") or "corepack", "pnpm"]
    root, = json.loads(subprocess.check_output([*command, "list", "--depth", "Infinity", "--json"], cwd=source))
    pending, emitted = [root], set()
    while pending:
        node = pending.pop()
        directory = Path(node["path"])
        manifest = json.loads((directory / "package.json").read_text(encoding="utf-8-sig"))
        for kind in ("dependencies", "optionalDependencies", "devDependencies"):
            for name, child in node.get(kind, {}).items():
                child_path = Path(child["path"])
                if child_path.is_dir():
                    pending.append(child)
                elif name not in manifest.get("optionalDependencies", {}):
                    raise ValueError(f"Missing required frontend package {name} for {manifest['name']}.")
        if node is root:
            continue
        name, version = manifest["name"], manifest["version"]
        if name != node["from"] or version != node["version"]:
            raise ValueError(f"Installed package and pnpm graph differ for {name} {version}.")
        if (name, version) in emitted:
            continue
        emitted.add((name, version))
        repo = manifest.get("repository")
        author = manifest.get("author")
        yield ("JS", name, version), directory, manifest.get("license"), node.get("resolved"), (
            repo.get("url") if isinstance(repo, dict) else repo), [author] if author else [], None


def reviewed_notices(project):
    # Version-specific supplements for packages whose published tarballs omit the upstream terms.
    text = contents(project / "THIRD_PARTY_NOTICES.txt")
    rows = re.findall(r"^SUPPLEMENT (Rust|JS) (\S+) (\S+) \| (.+)$", text, re.M)
    parts = re.split(r"\n={72}\nNOTICE \[(\d+)\]\n", text)
    notices = {index: body.strip() for index, body in zip(parts[1::2], parts[2::2])}
    result = {}
    for ecosystem, name, version, refs in rows:
        entries = []
        for ref in refs.split(" | "):
            number, origin = ref.split(" ", 1)
            entries.append((origin, notices[number]))
        result[(ecosystem, name, version)] = entries
    return result


def build(project):
    supplements = reviewed_notices(project)
    inventory, texts = [], {}
    packages = [*packages_rust(project), *packages_js(project)]
    for key, directory, license_id, origin, repo, authors, license_file in sorted(packages, key=lambda item: item[0]):
        paths = set(notice_files(directory))
        if license_file:
            paths.add(directory / license_file)
        records = [(f.relative_to(directory).as_posix(), contents(f)) for f in sorted(paths)]
        records.extend(supplements.get(key, []))
        if not records or not any(body for _, body in records):
            raise ValueError(f"No license text for {' '.join(key)}. Review the exact package before distribution.")
        inventory.append(" ".join(key) + " | " + str(license_id or "see package license file"))
        if origin:
            inventory.append("  Source: " + origin)
        if repo:
            inventory.append("  Repository: " + repo)
        if authors:
            inventory.append("  Package authors: " + ", ".join(
                author if isinstance(author, str) else str(author.get("name", "")) for author in authors))
        for path, body in records:
            if body:
                sha = hashlib.sha256(body.encode("utf-8")).hexdigest()
                texts[sha] = body
                inventory.append(f"  {path} [{sha}]")
    inventory.append("\nSTATIC NATIVE LIBRARY NOTICES")
    native_files = {
        "LibRaw": ["COPYRIGHT", "LICENSE.CDDL", "LICENSE.LGPL"],
        "libde265": ["COPYING"],
        "libheif": ["COPYING"],
        "libjpeg-turbo": ["LICENSE.md", "README.ijg"],
    }
    for library, files in native_files.items():
        for filename in files:
            body = contents(project / "src-tauri/third_party" / library / filename)
            sha = hashlib.sha256(body.encode("utf-8")).hexdigest()
            texts[sha] = body
            inventory.append(f"  {library}/{filename} [{sha}]")
    header = f"""Time-cut Windows x64 dependency inventory

Cargo target: {TARGET}. Includes reachable normal and build dependencies, including procedural macros.
Frontend: packages reachable in the installed frozen pnpm graph, including development/build tools.
Inclusion records build inputs as well as code distributed in the executable or browser bundle.
Source package license files and reviewed supplements are reproduced below. Identical text appears once.
See THIRD_PARTY_NOTICES.txt and the adjacent license files for native libraries, media tools, models and data.
"""
    return "\n".join([header, *inventory, "", "FULL LICENSE TEXTS", "",
                      *(f"[{sha}]\n{body}\n" for sha, body in sorted(texts.items()))])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", default=Path(__file__).resolve().parents[1], type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    text = build(args.project.resolve())
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=output.parent, delete=False) as temp:
        temporary = Path(temp.name)
    try:
        temporary.write_text(text, encoding="utf-8", newline="\n")
        os.replace(temporary, output)
    finally:
        temporary.unlink(missing_ok=True)
    print(f"Windows dependency notices written to {output.name} ({output.stat().st_size:,} bytes)")


if __name__ == "__main__":
    main()
