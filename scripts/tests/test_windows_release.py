"""Exercise the Windows download and packaging scripts in an isolated checkout."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from zipfile import ZipFile


SCRIPTS = Path(__file__).resolve().parents[1]
PWSH = shutil.which("pwsh") or shutil.which("powershell")


@unittest.skipUnless(os.name == "nt" and PWSH, "requires Windows PowerShell")
class WindowsReleaseTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="timecut-release-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        (self.root / "scripts").mkdir()

    def run_ps(self, command, **variables):
        return subprocess.run(
            [PWSH, "-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'\n" + command],
            cwd=self.root, capture_output=True, text=True, errors="replace",
            env={**os.environ, **variables}, timeout=40,
        )

    def test_windows_sidecar_rejects_corrupt_cache_and_failed_transfers(self):
        shutil.copy2(SCRIPTS / "download_ffmpeg_sidecar.ps1", self.root / "scripts")
        shutil.copy2(SCRIPTS / "windows_ffmpeg.json", self.root / "scripts")
        shutil.copy2(SCRIPTS / "fetch_timecut_ffmpeg.py", self.root / "scripts")
        target = self.root / "src-tauri/resources/ffmpeg"
        target.mkdir(parents=True)
        for arch, triple in (("x64", "x86_64"), ("arm64", "aarch64")):
            cached = target / f"ffmpeg-{triple}-pc-windows-msvc.exe"
            cached.write_bytes(b"invalid cached binary")
            run_script = f"./scripts/download_ffmpeg_sidecar.ps1 -Arch {arch}"
            result = self.run_ps(run_script)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("SHA-256 mismatch", result.stderr)
            self.assertEqual(cached.read_bytes(), b"invalid cached binary")
            cached.unlink()
            if arch == "x64":
                # Both the downloaded and local release ZIP pass through the same
                # per-executable checks. A bad second file must leave no installed pair.
                fixture = self.root / "supplied.zip"
                release = json.loads((self.root / "scripts/windows_ffmpeg.json").read_text())
                release["ffmpegSha256"] = hashlib.sha256(b"test ffmpeg").hexdigest()
                (self.root / "scripts/windows_ffmpeg.json").write_text(json.dumps(release))
                with ZipFile(fixture, "w") as zipped:
                    for name in ("ffmpeg", "ffprobe"):
                        zipped.writestr(f"portable/ffmpeg/{name}-{triple}-pc-windows-msvc.exe", b"test " + name.encode())
                result = self.run_ps(run_script + " -Archive ./supplied.zip")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("SHA-256 mismatch for ffprobe", result.stderr)
                self.assertEqual(list(target.iterdir()), [])
                continue
            for behavior, expected in (("", "SHA-256 mismatch"), ("throw 'interrupted'", "interrupted")):
                with self.subTest(arch=arch, behavior=behavior):
                    mock = ("function Invoke-WebRequest { param($Uri, $Headers, $OutFile, [switch]$UseBasicParsing)\n"
                            "  [IO.File]::WriteAllText($OutFile, 'incomplete')\n" + behavior + "\n}\n")
                    result = self.run_ps(mock + run_script)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(expected, result.stderr)
                    self.assertEqual(list(target.iterdir()), [])

    def test_repack_keeps_unrelated_files_out_of_zip(self):
        shutil.copy2(SCRIPTS / "package_timecut_windows.ps1", self.root / "scripts")
        (self.root / "scripts/windows_dependency_notices.py").write_text(
            "import pathlib, sys\npathlib.Path(sys.argv[-1]).write_text('isolated fixture notices')\n")
        for script in ("download_ffmpeg_sidecar.ps1", "download_models.ps1"):
            (self.root / "scripts" / script).write_text(
                "param($Arch, [switch]$VerifyOnly)\nif (!$VerifyOnly) { throw 'Expected offline verification' }"
            )
        for group, names in {
            "": ["LICENSE", "PRIVACY.md", "THIRD_PARTY_NOTICES.txt"],
            "docs": ["Time-cut-Windows.md", "Windows-source.md"],
            "src-tauri/third_party": ["idevice-LICENSE.txt", "FFmpeg-Windows-NOTICES.txt"],
            "src-tauri/third_party/LibRaw": ["COPYRIGHT", "LICENSE.LGPL"],
            "src-tauri/third_party/libheif": ["COPYING"],
            "src-tauri/third_party/libde265": ["COPYING"],
            "src-tauri/third_party/libjpeg-turbo": ["LICENSE.md", "README.ijg"],
            "src-tauri/resources/models": ["vision_model.onnx", "text_model.onnx", "tokenizer.json",
                "face_detection_yunet_2026may.onnx", "face_recognition_sface_2021dec.onnx",
                "YuNet-LICENSE.txt", "SFace-LICENSE.txt"],
            "src-tauri/resources/ffmpeg": [f"{tool}-x86_64-pc-windows-msvc.exe" for tool in ("ffmpeg", "ffprobe")],
            "ort": ["onnxruntime.dll", "onnxruntime_providers_shared.dll", "LICENSE", "ThirdPartyNotices.txt"],
            "crt": ["vcruntime140.dll"], "src-tauri/target/release": ["Lap.exe"],
        }.items():
            directory = self.root / group
            directory.mkdir(exist_ok=True, parents=True)
            for name in names:
                (directory / name).write_bytes(name.encode())
        (self.root / "src-tauri/tauri.conf.json").write_text('{"version":"0.1.0-preview.1"}')
        (self.root / ".gitignore").write_text("release-test/\n")
        for args in (["init", "--quiet"], ["add", "."],
                     ["-c", "user.email=test@localhost", "-c", "user.name=test", "commit", "--quiet", "-m", "fixture"]):
            subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)
        package = "./scripts/package_timecut_windows.ps1 -OnnxRuntime ./ort -CrtDirectory ./crt"
        first = self.run_ps(package)
        self.assertEqual(first.returncode, 0, first.stderr)
        output = self.root / "release-test"
        label = "Time-cut-0.1.0-preview.1-windows-x64"
        (output / label / "keep.txt").write_text("user file")
        second = self.run_ps(package)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertFalse((output / label / "keep.txt").exists())
        preserved = list(output.glob(f"{label}.previous-*/keep.txt"))
        self.assertEqual([p.read_text() for p in preserved], ["user file"])
        archive = output / (label + ".zip")
        with ZipFile(archive) as zipped:
            self.assertIsNone(zipped.testzip())
            self.assertFalse(any(name.endswith("keep.txt") for name in zipped.namelist()))
            info = json.loads(zipped.read(label + "/BUILD-INFO.json").decode("utf-8-sig"))
            self.assertFalse(info["workingTreeModified"])
            self.assertEqual(len(info["commit"]), 40)
            self.assertEqual(info["executableSHA256"].lower(), hashlib.sha256(b"Lap.exe").hexdigest())
        self.assertEqual(Path(str(archive) + ".sha256").read_text().split()[0], hashlib.sha256(archive.read_bytes()).hexdigest())
