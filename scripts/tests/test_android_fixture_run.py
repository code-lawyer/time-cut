import hashlib
import json
from pathlib import Path
import sqlite3
from tempfile import TemporaryDirectory
import unittest

from scripts.verify_android_fixture_run import verify


class AndroidFixtureRunTests(unittest.TestCase):
    def setUp(self):
        self.temporary = TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.archive = root / "archive"
        state = self.archive / ".time-cut"
        state.mkdir(parents=True)
        self.db = sqlite3.connect(state / "imports.sqlite3")
        self.addCleanup(self.db.close)
        self.db.executescript("""
            CREATE TABLE phone_runs(id, source, discovered, safe_at);
            CREATE TABLE phone_lists(run_id, total);
            CREATE TABLE phone_inbox(run_id);
            CREATE TABLE files(path,digest,size);
            CREATE TABLE sidecar_paths(owner,digest,path);
            CREATE TABLE phone_manifest(run_id,ordinal,entry,version,digest,size);
            CREATE TABLE phone_list_items(run_id,ordinal,entry,version,digest,size,modified);
        """)
        folder = "TimecutQa_" + "a" * 32
        self.base = "Internal storage/" + folder + "/"
        original = {"sample.png": b"one", "alias.png": b"one", "nested/sample.png": b"dif",
                    "sample.xmp": b"xmp", "clip.mp4": b"clip"}
        changed = {**original, "sample.png": b"two"}
        self.receipt = root / "receipt.json"

        def inventory(items):
            return [dict(path=name, bytes=len(data), sha256=hashlib.sha256(data).hexdigest()) for name, data in items.items()]

        self.receipt.write_text(json.dumps(dict(format="timecut-android-fixture-v1", folder=folder,
                                                seed=inventory(original), mutation=inventory(changed))), encoding="utf8")
        for ordinal, (name, data) in enumerate([*original.items(), ("sample.png", b"two")]):
            if name == "alias.png":
                continue
            archive_path = str(ordinal) + Path(name).suffix
            (self.archive / archive_path).write_bytes(data)
            self.db.execute("INSERT INTO files VALUES(?,?,?)", (archive_path, self.receipt_digest(data), len(data)))
        self.db.execute("INSERT INTO sidecar_paths VALUES('0.xmp',?,'3.xmp')", (self.receipt_digest(b"xmp"),))
        for run, fixtures, background in (
            (1, {}, {"private1.jpg": b"first"}),
            (2, original, {"private1.jpg": b"first", "private2.jpg": b"second"}),
            (3, changed, {"private2.jpg": b"edited"}),
        ):
            all_entries = {**background, **{self.base + name: data for name, data in fixtures.items()}}
            self.db.execute("INSERT INTO phone_runs VALUES(?,'wpd:synthetic',?,?)", (run, len(all_entries), run))
            self.db.execute("INSERT INTO phone_lists VALUES(?,?)", (run, len(all_entries)))
            for ordinal, (entry, data) in enumerate(all_entries.items()):
                row = (run, ordinal, entry, "v", self.receipt_digest(data), len(data))
                self.db.execute("INSERT INTO phone_manifest VALUES(?,?,?,?,?,?)", row)
                self.db.execute("INSERT INTO phone_list_items VALUES(?,?,?,?,?,?,'time')", row)
        self.db.commit()

    @staticmethod
    def receipt_digest(data):
        # The offline helper links native digest columns and independently hashes bytes with SHA-256.
        return hashlib.sha256(b"mock native digest:" + data).hexdigest()

    def check(self, *runs):
        self.db.commit()
        return verify(self.receipt, self.archive, *runs)

    def test_stages_and_unrelated_changes(self):
        self.assertEqual(set(self.check(1)), {"before"})
        self.assertTrue(self.check(1, 2)["seed"]["aliasesShareOneStoredOriginal"])
        result = self.check(1, 2, 3)
        self.assertNotIn("private", json.dumps(result))
        self.assertEqual(result["seed"]["nonFixtureChanges"]["addedPaths"], 1)
        self.assertEqual(result["mutated"]["nonFixtureChanges"]["changedContentPaths"], 1)
        self.assertEqual(result["mutated"]["nonFixtureChanges"]["removedPaths"], 1)
        self.assertTrue(result["mutated"]["phoneMetadata"]["allThreeKnownAndSame"])
        self.assertTrue(result["mutated"]["oldAndNewOriginalsRetained"])

    def test_changed_or_unavailable_metadata_is_distinguished(self):
        entry = self.base + "sample.png"
        self.db.execute("UPDATE phone_list_items SET modified='later' WHERE run_id=3 AND entry=?", (entry,))
        self.assertFalse(self.check(1, 2, 3)["mutated"]["phoneMetadata"]["modifiedSame"])
        self.db.execute("UPDATE phone_list_items SET modified=NULL WHERE entry=?", (entry,))
        self.assertIsNone(self.check(1, 2, 3)["mutated"]["phoneMetadata"]["allThreeKnownAndSame"])

    def test_unregistered_duplicate_and_corruption_fail(self):
        extra = self.archive / "untracked.png"
        extra.write_bytes(b"one")
        with self.assertRaisesRegex(ValueError, "extra or missing copy"):
            self.check(1, 2, 3)
        extra.unlink()
        (self.archive / "3.xmp").write_bytes(b"bad")
        with self.assertRaisesRegex(ValueError, "SHA-256"):
            self.check(1, 2, 3)

    def test_changed_receipt_rejected(self):
        self.db.execute("UPDATE phone_manifest SET version='mismatch' WHERE run_id=2 AND ordinal=2")
        with self.assertRaisesRegex(ValueError, "matching receipt"):
            self.check(1, 2)

    def test_xmp_attached_to_the_wrong_same_name_image_is_rejected(self):
        self.db.execute("UPDATE sidecar_paths SET owner='2.xmp'")
        with self.assertRaisesRegex(ValueError, "not attached"):
            self.check(1, 2)


if __name__ == "__main__":
    unittest.main()
