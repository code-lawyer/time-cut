import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "package_release_source.py"


class ReleaseSourceTests(unittest.TestCase):
    def test_source_uses_recorded_git_objects_including_submodules(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project, library, out = (root / name for name in ("project", "library", "out"))
            def git(repo, *args):
                return subprocess.check_output(["git", "-C", repo, "-c", "user.email=test@localhost",
                    "-c", "user.name=test", "-c", "protocol.file.allow=always", *args], stderr=subprocess.DEVNULL)
            for repo in project, library:
                repo.mkdir()
                git(repo, "init", "--quiet")
            (library / "COPYING").write_bytes(b"recorded library source")
            git(library, "add", ".")
            git(library, "commit", "-qm", "library")
            (project / "src-tauri").mkdir()
            (project / "src-tauri/tauri.conf.json").write_text('{"version":"1.2.3"}')
            git(project, "submodule", "add", str(library), "vendored")
            git(project, "commit", "-qam", "release")
            # Include the product manifest in the recorded revision.
            git(project, "add", "src-tauri")
            git(project, "commit", "-qm", "manifest")
            (project / "vendored/COPYING").write_bytes(b"uncommitted change")
            (project / "private.txt").write_bytes(b"untracked")
            subprocess.run([sys.executable, SCRIPT, "--project", project, "--output", out], check=True, capture_output=True)
            with tarfile.open(out / "Time-cut-1.2.3-source.tar.gz") as source:
                label = "Time-cut-1.2.3-source/"
                self.assertEqual(source.extractfile(label + "vendored/COPYING").read(), b"recorded library source")
                self.assertNotIn(label + "private.txt", source.getnames())
                provenance = json.load(source.extractfile(label + "TIME-CUT-SOURCE.json"))
                self.assertEqual(provenance["submodules"], {"vendored": git(library, "rev-parse", "HEAD").decode().strip()})


if __name__ == "__main__":
    unittest.main()
