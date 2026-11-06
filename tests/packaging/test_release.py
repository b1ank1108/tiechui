"""Offline packaging regressions, with disposable artifacts and prefixes."""
import io
import json
import os
from pathlib import Path
import shutil
import sys
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
import release


class Packaging(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.prefix = self.root / "prefix"
        self.artifact = self.artifact_for("first")

    def artifact_for(self, tag):
        output = self.root / tag
        output.mkdir()
        for name in release.NAMES:
            (output / name).write_bytes((name + tag).encode())
            (output / name).chmod(0o755)
        return release.package(output, "0.1.0", "aarch64-apple-darwin", {"commit": tag}, True)

    def install(self, artifact=None, checkpoint=lambda _: None):
        release.install(artifact or self.artifact, self.prefix, checkpoint)

    def test_ownership_move_repair_and_restore(self):
        self.prefix.mkdir()
        (self.prefix / "config").write_bytes(b"untouched")
        self.install()
        ownership = release.record(self.prefix / release.STATE / "ownership.json")
        self.assertEqual(set(ownership["files"]), set(release.NAMES))
        self.assertEqual(ownership["artifact_sha256"], release.digest(self.artifact.read_bytes()))
        moved = self.root / "moved"
        self.prefix.rename(moved)
        self.prefix = moved
        (moved / "bin/tc").unlink()
        self.install()
        self.assertEqual((moved / "bin/tc").read_bytes(), b"tcfirst")
        self.install(self.artifact_for("second"))
        release.uninstall(moved)
        self.assertFalse((moved / "bin").exists())
        self.assertEqual((moved / "config").read_bytes(), b"untouched")
        self.assertFalse((moved / release.STATE / "ownership.json").exists())

    def test_foreign_files_and_external_edits_refused(self):
        (self.prefix / "bin").mkdir(parents=True)
        foreign = self.prefix / "bin/tiechui"
        foreign.write_bytes(b"foreign")
        with self.assertRaisesRegex(RuntimeError, "unowned"):
            self.install()
        self.assertFalse((self.prefix / "bin/tc").exists())
        self.assertEqual(foreign.read_bytes(), b"foreign")
        foreign.unlink()
        self.install()
        foreign.write_bytes(b"external edit")
        for operation in (self.install, lambda: release.uninstall(self.prefix)):
            with self.assertRaisesRegex(RuntimeError, "modified"):
                operation()
        self.assertEqual(foreign.read_bytes(), b"external edit")

    def test_existing_bin_and_other_commands_survive(self):
        (self.prefix / "bin").mkdir(parents=True)
        other = self.prefix / "bin/other"
        other.write_bytes(b"existing")
        self.install()
        release.uninstall(self.prefix)
        self.assertEqual(list((self.prefix / "bin").iterdir()), [other])

    def test_interrupt_at_each_boundary_rolls_back_then_retries(self):
        second = self.artifact_for("second")
        self.install()
        for boundary in ("journal", "tc", "tiechui", "record"):
            def interrupt(stage):
                if stage == boundary:
                    raise InterruptedError("simulated power loss")
            with self.assertRaises(InterruptedError):
                self.install(second, interrupt)
            journal = self.prefix / release.STATE / "journal.json"
            self.assertTrue(journal.exists())
            with release.locked(self.prefix) as (prefix, state):
                release.recover(prefix, state)
            self.assertFalse(journal.exists())
            self.assertEqual((self.prefix / "bin/tc").read_bytes(), b"tcfirst")
            self.assertEqual((self.prefix / "bin/tiechui").read_bytes(), b"tiechuifirst")
        self.install(second)
        self.assertEqual((self.prefix / "bin/tc").read_bytes(), b"tcsecond")

    def test_interrupted_initial_install_and_uninstall(self):
        def interrupt(stage):
            if stage == "tc":
                raise InterruptedError()
        with self.assertRaises(InterruptedError):
            self.install(checkpoint=interrupt)
        self.install()
        with self.assertRaises(InterruptedError):
            release.uninstall(self.prefix, interrupt)
        release.uninstall(self.prefix)
        self.assertFalse((self.prefix / "bin").exists())

    def test_recovery_refuses_external_changes(self):
        self.install()
        def interrupt(stage):
            if stage == "tc":
                raise InterruptedError()
        with self.assertRaises(InterruptedError):
            self.install(self.artifact_for("second"), interrupt)
        (self.prefix / "bin/tc").write_bytes(b"external")
        with self.assertRaisesRegex(RuntimeError, "external modification"):
            self.install()
        self.assertEqual((self.prefix / "bin/tc").read_bytes(), b"external")

    def test_prefix_lock_excludes_other_installers(self):
        with release.locked(self.prefix):
            with self.assertRaisesRegex(RuntimeError, "another install"):
                self.install()
        self.install()

    def test_symlink_and_archive_traversal_refused(self):
        self.prefix.mkdir()
        destination = self.root / "destination"
        destination.mkdir()
        (self.prefix / "bin").symlink_to(destination, target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError, "non-directory"):
            self.install()
        self.assertEqual(list(destination.iterdir()), [])
        malicious = self.root / "malicious.tar.gz"
        with tarfile.open(malicious, "w:gz") as archive:
            entry = tarfile.TarInfo("../escaped")
            entry.size = 1
            archive.addfile(entry, io.BytesIO(b"x"))
        with self.assertRaisesRegex(RuntimeError, "only regular"):
            release.unpack(malicious)

    def test_checksum_and_schema_fail_before_mutation(self):
        output = self.root / "first"
        with tarfile.open(self.artifact, "w:gz") as archive:
            for name in (*release.NAMES, "manifest.json"):
                data = b"corrupted" if name == "tc" else (output / name).read_bytes()
                entry = tarfile.TarInfo(name)
                entry.size = len(data)
                archive.addfile(entry, io.BytesIO(data))
        with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
            self.install()
        self.assertFalse(self.prefix.exists())

    def test_deterministic_tarball_and_unpublished_formula(self):
        initial = self.artifact.read_bytes()
        release.package(self.root / "first", "0.1.0", "aarch64-apple-darwin", {"commit": "first"}, True)
        self.assertEqual(self.artifact.read_bytes(), initial)
        self.assertIn(self.artifact.as_uri(), (self.root / "first/tiechui.rb").read_text())

    def test_windows_zip(self):
        output = self.root / "windows"
        output.mkdir()
        for name in ("tc.exe", "tiechui.exe"):
            (output / name).write_bytes(b"MZ" + name.encode())
        artifact = release.package(output, "0.1.0", "x86_64-pc-windows-msvc", {"commit": "windows"}, True)
        first = artifact.read_bytes()
        release.package(output, "0.1.0", "x86_64-pc-windows-msvc", {"commit": "windows"}, True)
        self.assertEqual(artifact.read_bytes(), first)
        self.assertEqual(artifact.name, "tiechui-0.1.0-x86_64-pc-windows-msvc.zip")
        with zipfile.ZipFile(artifact) as archive:
            self.assertEqual(archive.namelist(), ["tc.exe", "tiechui.exe", "manifest.json"])
        self.assertFalse((output / "tiechui.rb").exists())

    def test_npm_packages_launch_the_platform_build(self):
        node = shutil.which("node")
        here = subprocess.run([node, "-p", "process.platform + '-' + process.arch"], capture_output=True,
                              text=True, check=True).stdout.strip() if node else None
        assets = self.root / "assets"
        assets.mkdir()
        sums = []
        for host, platform in release.NPM.items():
            output = self.root / host
            output.mkdir()
            for name in release.binaries(host):
                script = name == "tc" and platform == here
                (output / name).write_bytes(b'#!/bin/sh\necho "$TIECHUI_INSTALL $*"\nexit 3\n'
                                            if script else (name + host).encode())
                (output / name).chmod(0o755)
            artifact = release.package(output, "0.1.0", host, {"commit": host}, True)
            shutil.copy2(artifact, assets / artifact.name)
            sums.append(f"{release.digest(artifact.read_bytes())}  {artifact.name}\n")
        (assets / "SHA256SUMS").write_text("".join(sums))
        npm = self.root / "npm"
        main = release.npm(assets, "0.1.0", npm)
        package = json.loads((main / "package.json").read_bytes())
        self.assertEqual(package["bin"], {"tc": "bin/tc.js", "tiechui": "bin/tc.js"})
        self.assertEqual(package["optionalDependencies"]["@tiechui/cli-win32-x64"],
                         "npm:@tiechui/cli@0.1.0-win32-x64")
        windows = json.loads((npm / "win32-x64/package.json").read_bytes())
        self.assertEqual((windows["name"], windows["version"], windows["os"], windows["cpu"], windows["files"]),
                         ("@tiechui/cli", "0.1.0-win32-x64", ["win32"], ["x64"], ["tc.exe"]))
        self.assertEqual((npm / "win32-x64/tc.exe").read_bytes(), b"tc.exex86_64-pc-windows-msvc")
        self.assertEqual(json.loads((npm / "linux-x64/package.json").read_bytes())["libc"], ["glibc"])
        launcher = (main / "bin/tc.js").read_text()
        self.assertIn(json.dumps(list(release.NPM.values())).replace('","', '", "'), launcher)
        with self.assertRaisesRegex(RuntimeError, "mismatched release archive"):
            (assets / "SHA256SUMS").write_text("".join(sums[1:]))
            release.npm(assets, "0.1.0", self.root / "again")
        if here not in release.NPM.values() or os.name == "nt":
            self.skipTest("node is missing or cannot run the shell fixture")
        modules = self.root / "global/node_modules/@tiechui"
        shutil.copytree(main, modules / "cli")
        shutil.copytree(npm / here, modules / "cli/node_modules/@tiechui" / f"cli-{here}")
        launched = subprocess.run([node, modules / "cli/bin/tc.js", "exec", "hi"], capture_output=True, text=True)
        self.assertEqual((launched.returncode, launched.stdout), (3, "npm exec hi\n"))
        shutil.rmtree(modules / "cli/node_modules")
        missing = subprocess.run([node, modules / "cli/bin/tc.js"], capture_output=True, text=True)
        self.assertEqual(missing.returncode, 1)
        self.assertIn("npm i -g @tiechui/cli", missing.stderr)

    def test_space_gate_precedes_build(self):
        with patch.object(release.shutil, "disk_usage", return_value=shutil._ntuple_diskusage(100, 99, 1)):
            with self.assertRaisesRegex(RuntimeError, "at least 6 GiB"):
                release.build(self.artifact, self.root)

    def test_source_export_selects_commit_or_candidate_without_touching_git(self):
        repository = self.root / "repository"
        repository.mkdir()
        environment = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                           GIT_AUTHOR_NAME="Fixture", GIT_AUTHOR_EMAIL="fixture@example.invalid",
                           GIT_COMMITTER_NAME="Fixture", GIT_COMMITTER_EMAIL="fixture@example.invalid")

        def git(*args, data=None):
            return subprocess.run(["git", *args], cwd=repository, env=environment,
                                  input=data, capture_output=True, check=True).stdout.decode().strip()

        git("init", "-q")
        blob = git("hash-object", "-w", "--stdin", data=b"committed")
        ignore = git("hash-object", "-w", "--stdin", data=b".local/\n")
        tree = git("mktree", data=f"100644 blob {blob}\tfile\n100644 blob {ignore}\t.gitignore\n".encode())
        commit = git("commit-tree", tree, "-m", "Fixture")
        git("update-ref", "HEAD", commit)
        git("read-tree", "HEAD")
        (repository / ".gitignore").write_text(".local/\n")
        (repository / "file").write_bytes(b"candidate")
        (repository / "new").write_bytes(b"new source")
        before = {p.relative_to(repository): p.read_bytes() for p in (repository / ".git").rglob("*") if p.is_file()}
        with patch.object(release, "ROOT", repository):
            for revision, expected in [(commit, b"committed"), (None, b"candidate")]:
                with release.source(revision) as (archive, metadata):
                    export = archive
                    with tarfile.open(archive) as files:
                        self.assertEqual(files.extractfile("file").read(), expected)
                        self.assertEqual("new" in files.getnames(), revision is None)
                        self.assertFalse(any(name.startswith(".local") for name in files.getnames()))
                    self.assertEqual(metadata["commit"], commit)
                self.assertFalse(export.exists())
        after = {p.relative_to(repository): p.read_bytes() for p in (repository / ".git").rglob("*") if p.is_file()}
        self.assertEqual(before, after)

    def test_failed_build_removes_export_and_its_target(self):
        (self.root / ".local").mkdir()
        source = self.root / "source.tar"
        with tarfile.open(source, "w") as archive:
            info = tarfile.TarInfo("Cargo.toml")
            data = b'[workspace.package]\nversion = "0.1.0"\n'
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        exports = []

        def fail(args, cwd=None, env=None):
            if args[0] == "rustc":
                return b"host: aarch64-apple-darwin\n"
            exports.append(cwd)
            self.assertEqual(env["CARGO_TARGET_DIR"], str(cwd / "target"))
            (cwd / "target").mkdir()
            raise RuntimeError("simulated build failure")

        with patch.object(release, "ROOT", self.root), patch.object(release, "run", side_effect=fail), patch.object(release, "space"):
            with self.assertRaisesRegex(RuntimeError, "simulated build failure"):
                release.build(source, self.root)
        self.assertTrue(exports)
        self.assertFalse(exports[0].exists())


if __name__ == "__main__":
    unittest.main()
