"""Check completed Android fixture runs offline. No phone or application process is used."""

import argparse
from collections import Counter, defaultdict
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import sys


LEAVES = {"sample.png", "alias.png", "nested/sample.png", "sample.xmp", "clip.mp4"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read_receipt(path):
    receipt = json.loads(path.read_text(encoding="utf-8-sig"))
    require(receipt["format"] == "timecut-android-fixture-v1"
            and re.fullmatch(r"TimecutQa_[0-9a-f]{32}", receipt["folder"]), "Invalid fixture receipt")
    rounds = {}
    for phase in ("seed", "mutation"):
        items = {row["path"]: (row["bytes"], row["sha256"]) for row in receipt[phase]}
        require(len(receipt[phase]) == len(items) and items.keys() == LEAVES, "Invalid fixture inventory")
        for size, digest in items.values():
            require(isinstance(size, int) and size > 0 and re.fullmatch(r"[0-9a-f]{64}", digest),
                    "Invalid fixture size or SHA-256")
        rounds[phase] = items
    old, new = rounds["seed"], rounds["mutation"]
    require(old["sample.png"] == old["alias.png"] and len({item[1] for item in old.values()}) == 4
            and new["sample.png"][0] == old["sample.png"][0]
            and new["sample.png"][1] not in {item[1] for item in old.values()}
            and all(old[name] == new[name] for name in LEAVES - {"sample.png"}),
            "Receipt does not describe the expected duplicate and same-size replacement")
    return receipt["folder"], rounds


def read_run(db, run_id, folder):
    run = db.execute("SELECT source,discovered,safe_at FROM phone_runs WHERE id=?", (run_id,)).fetchone()
    require(run and run["source"].startswith("wpd:") and run["safe_at"], "A selected WPD run is incomplete")
    total = db.execute("SELECT total FROM phone_lists WHERE run_id=?", (run_id,)).fetchone()
    require(total and total[0] == run["discovered"] > 0, "Complete phone list is missing")
    require(not db.execute("SELECT 1 FROM phone_inbox WHERE run_id=? LIMIT 1", (run_id,)).fetchone(),
            "A selected run still has unfiled receipts")
    manifest_count = db.execute("SELECT count(*) FROM phone_manifest WHERE run_id=?", (run_id,)).fetchone()[0]
    require(manifest_count == total[0], "Receipt total differs from phone inventory")
    rows = db.execute("""
        SELECT l.ordinal,l.entry,l.version,l.size,l.modified,m.digest,m.size AS bytes,
            (l.entry=m.entry AND l.version IS m.version AND l.digest=m.digest AND
             (l.size IS NULL OR l.size=m.size)) AS matches
        FROM phone_list_items l LEFT JOIN phone_manifest m
          ON l.run_id=m.run_id AND l.ordinal=m.ordinal
        WHERE l.run_id=? ORDER BY l.ordinal""", (run_id,))
    scoped, others, stores = {}, defaultdict(Counter), set()
    seen = 0
    for row in rows:
        require(row["ordinal"] == seen and row["matches"] == 1, "A USB list row has no matching receipt")
        seen += 1
        parts = row["entry"].replace("\\", "/").split("/")
        if folder in parts:
            require(parts.index(folder) == 1 and parts.count(folder) == 1 and parts[0],
                    "Fixture directory is outside its expected storage root")
            stores.add(parts[0])
            leaf = "/".join(parts[2:])
            require(leaf in LEAVES and leaf not in scoped, "Unexpected or ambiguous entry in fixture folder")
            scoped[leaf] = row
        else:
            others["/".join(parts)][(row["digest"], row["bytes"], row["version"], row["size"], row["modified"])] += 1
    require(seen == total[0] and len(stores) <= 1, "Incomplete inventory or ambiguous fixture storage")
    return run, scoped, others, next(iter(stores), None)


def background_delta(old, new):
    shared = old.keys() & new.keys()
    content_changes = metadata_only = 0
    for key in shared:
        if old[key] == new[key]:
            continue
        content = []
        for rows in (old[key], new[key]):
            counts = Counter()
            for (digest, size, *_), number in rows.items():
                counts[digest, size] += number
            content.append(counts)
        if content[0] != content[1]:
            content_changes += 1
        else:
            metadata_only += 1
    return dict(addedPaths=len(new.keys() - old.keys()), removedPaths=len(old.keys() - new.keys()),
                changedContentPaths=content_changes, metadataOnlyChangedPaths=metadata_only,
                oldEntries=sum(sum(rows.values()) for rows in old.values()),
                newEntries=sum(sum(rows.values()) for rows in new.values()))


def sha256(path):
    with path.open("rb") as stream:
        hasher = hashlib.sha256()
        for block in iter(lambda: stream.read(256 * 1024), b""):
            hasher.update(block)
        return hasher.hexdigest()


def verify_single_physical_copy(archive, size, digest, expected, hashed):
    copies = set()

    def check_scan_error(error):
        raise error

    def external_link(path):
        info = path.lstat()
        return stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT

    for directory, folders, files in os.walk(archive, onerror=check_scan_error, followlinks=False):
        parent = Path(directory)
        folders[:] = [name for name in folders if name.casefold() != ".time-cut" and not external_link(parent / name)]
        for name in files:
            candidate = parent / name
            if external_link(candidate) or not candidate.is_file() or candidate.stat().st_size != size:
                continue
            actual = hashed.get(candidate)
            if (actual[1] if actual else sha256(candidate)) == digest:
                copies.add(candidate)
    require(copies == expected, "The archive on disk contains an extra or missing copy of the duplicate PNG")


def verify_files(db, archive, scoped, expected, hashed):
    require(scoped.keys() == LEAVES, "Controlled inventory is missing one or more of the five fixture leaves")
    verified, path_sets = {}, {}
    for leaf in sorted(LEAVES):
        size, digest = expected[leaf]
        row = scoped[leaf]
        require(row["bytes"] == size and re.fullmatch(r"[0-9a-f]{64}", row["digest"]),
                f"Invalid USB content receipt for controlled {leaf}")
        paths = set()
        for (relative,) in db.execute("SELECT path FROM files WHERE digest=? AND size=?", (row["digest"], size)):
            relative = Path(relative.replace("\\", "/"))
            require(not relative.is_absolute() and ".." not in relative.parts and relative.parts
                    and relative.parts[0].casefold() != ".time-cut", "Invalid path in fixture archive")
            candidate = (archive / relative).resolve(strict=True)
            require(archive in candidate.parents and candidate.is_file(), "Fixture archive path is outside its archive")
            if candidate not in hashed:
                hashed[candidate] = candidate.stat().st_size, sha256(candidate)
            require(hashed[candidate] == (size, digest), f"Archived bytes differ from the fixture SHA-256 for {leaf}")
            require(candidate.suffix.lower() == Path(leaf).suffix, f"Unexpected archive media type for {leaf}")
            paths.add(candidate)
        require(paths, f"No matching filed original for controlled {leaf}")
        path_sets[leaf] = paths
        verified[leaf] = {"bytes": size, "sha256Matched": True, "archivePaths": len(paths)}
    require(path_sets["nested/sample.png"].isdisjoint(path_sets["sample.png"]),
            "Different same-name images share an archive path")
    return verified, path_sets


def check_xmp_owner(db, archive, digest, images, attachments):
    targets = {p.with_suffix(".xmp") for p in images}
    if db.execute("SELECT 1 FROM sqlite_schema WHERE type='table' AND name='sidecar_paths'").fetchone():
        return any((archive / owner.replace("\\", "/")).resolve() in targets
                   and (archive / path.replace("\\", "/")).resolve() in attachments
                   for owner, path in db.execute("SELECT owner,path FROM sidecar_paths WHERE digest=?", (digest,)))
    # Earlier archives lack the owner journal; the ordinary adjacent name is still verifiable.
    return bool(targets & attachments)


def compare_observation(a, b):
    return None if a is None and b is None else a == b


def verify(receipt, archive, before, seed=None, mutated=None):
    require(mutated is None or seed is not None, "A mutation comparison needs a seed run")
    ids = [i for i in (before, seed, mutated) if i is not None]
    require(all(i > 0 for i in ids) and ids == sorted(set(ids)), "Run IDs must be distinct and chronological")
    folder, expected = read_receipt(receipt)
    archive = archive.resolve(strict=True)
    database = (archive / ".time-cut" / "imports.sqlite3").resolve(strict=True)
    require(archive in database.parents, "Manifest is outside the archive")
    with closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as db:
        db.row_factory = sqlite3.Row
        db.execute("BEGIN")
        initial, absent, before_other, _ = read_run(db, before, folder)
        require(not absent, "Controlled directory already has media in the before run")
        result = {"before": {"runId": before, "controlledEntries": 0, "inventoryEntries": initial["discovered"]}}
        if seed is None:
            return result

        seeded, original, seed_other, storage = read_run(db, seed, folder)
        require(initial["source"] == seeded["source"] and initial["safe_at"] <= seeded["safe_at"],
                "Before and seed runs do not belong to the same chronological WPD inventory")
        hashed = {}
        checks, old_paths = verify_files(db, archive, original, expected["seed"], hashed)
        require(original["sample.png"]["digest"] == original["alias.png"]["digest"]
                and old_paths["sample.png"] == old_paths["alias.png"] and len(old_paths["sample.png"]) == 1,
                "The duplicate aliases did not resolve to exactly one stored original")
        verify_single_physical_copy(archive, *expected["seed"]["sample.png"], old_paths["sample.png"], hashed)
        require(check_xmp_owner(db, archive, original["sample.xmp"]["digest"], old_paths["sample.png"], old_paths["sample.xmp"]),
                "Seed XMP is not attached to the top-level image")
        result["seed"] = dict(runId=seed, controlledEntries=5, distinctReceiptDigests=len({row["digest"] for row in original.values()}),
                              aliasesShareOneStoredOriginal=True, sameNameInSubfolderDistinct=True, xmpOwnerVerified=True, files=checks,
                              nonFixtureChanges=background_delta(before_other, seed_other))
        if mutated is None:
            return result

        latest, changed, latest_other, latest_storage = read_run(db, mutated, folder)
        require(latest["source"] == seeded["source"] and storage == latest_storage and latest["safe_at"] >= seeded["safe_at"],
                "Mutation run uses a different device, storage or chronology")
        checks, new_paths = verify_files(db, archive, changed, expected["mutation"], hashed)
        a, b = original["sample.png"], changed["sample.png"]
        require(a["entry"] == b["entry"] and a["digest"] != b["digest"] and a["bytes"] == b["bytes"]
                and new_paths["sample.png"].isdisjoint(old_paths["sample.png"])
                and new_paths["alias.png"] == old_paths["sample.png"], "The same-path replacement lost an original")
        require(all(original[leaf]["digest"] == changed[leaf]["digest"] for leaf in LEAVES - {"sample.png"}),
                "Another controlled item also changed")
        signature = {name: compare_observation(a[field], b[field]) for name, field in
                     (("listedSizeSame", "size"), ("versionSame", "version"), ("modifiedSame", "modified"))}
        values = tuple(signature.values())
        signature["allThreeKnownAndSame"] = False if False in values else None if None in values else True
        result["mutated"] = dict(runId=mutated, controlledEntries=5, oldAndNewOriginalsRetained=True,
                                 samePathAndByteLength=True, changedFixtureContents=1, phoneMetadata=signature,
                                 files=checks, nonFixtureChanges=background_delta(seed_other, latest_other))
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipt", required=True, type=Path)
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--before", required=True, type=int)
    parser.add_argument("--seed", type=int)
    parser.add_argument("--mutated", type=int)
    args = parser.parse_args()
    try:
        print(json.dumps(verify(**vars(args)), ensure_ascii=True))
    except (OSError, sqlite3.Error, ValueError, KeyError, TypeError) as exc:
        # Only our explicit validation errors contain public synthetic leaf names.
        reason = str(exc) if type(exc) is ValueError else type(exc).__name__
        print(json.dumps({"verified": False, "reason": reason}))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
