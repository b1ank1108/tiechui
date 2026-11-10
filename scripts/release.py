#!/usr/bin/env python3
"""Local packaging. Python 3.10+, standard library only; never publishes.

Dist runs on POSIX and Windows; install and uninstall are POSIX-only. Dist and ripgrep
download the pinned ripgrep archive once into .local/ripgrep and verify it against RIPGREP_ASSETS."""
import argparse
from contextlib import contextmanager
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import tarfile
import tempfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[1]
NAMES = ("chui", "tiechui")
WINDOWS = os.name == "nt"
STATE = ".tiechui-install"
MIN_FREE = 6 * 1024 ** 3
# Release targets and their npm platforms, which scripts/npm/chui.js lists too.
NPM = {"aarch64-apple-darwin": "darwin-arm64", "x86_64-apple-darwin": "darwin-x64",
       "aarch64-unknown-linux-gnu": "linux-arm64", "x86_64-unknown-linux-gnu": "linux-x64",
       "x86_64-pc-windows-msvc": "win32-x64"}
REPOSITORY = "https://github.com/b1ank1108/tiechui"
# The ripgrep release bundled beside chui: upstream asset target, size and SHA-256 per release target.
RIPGREP = "15.2.0"
RIPGREP_ASSETS = {
    "aarch64-apple-darwin": ("aarch64-apple-darwin", 1764284,
                             "3750b2e93f37e0c692657da574d7019a101c0084da05a790c83fd335bad973e4"),
    "x86_64-apple-darwin": ("x86_64-apple-darwin", 1878284,
                            "af7825fcc69a2afc7a7aea55fc9af90e26421d8f20fe59df32e233c0b8a231c1"),
    "aarch64-unknown-linux-gnu": ("aarch64-unknown-linux-gnu", 1854661,
                                  "a740b91c82eaf9914cfedd353572f2791cbe0162c84101ee0951058f4dcbc90d"),
    # The static musl build also runs on glibc systems.
    "x86_64-unknown-linux-gnu": ("x86_64-unknown-linux-musl", 2265718,
                                 "33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c"),
    "x86_64-pc-windows-msvc": ("x86_64-pc-windows-msvc", 1789611,
                               "71b2fef860abe467217a538ff31de02f5258807c0129f771846f87bd029aafc5"),
}


def run(args, cwd=None, env=None, data=None):
    environment = dict(os.environ, CARGO_INCREMENTAL="0", CARGO_NET_OFFLINE="true",
                       CARGO_TARGET_DIR=str(ROOT / "target"), GIT_CONFIG_NOSYSTEM="1",
                       GIT_CONFIG_GLOBAL=os.devnull, GIT_TERMINAL_PROMPT="0")
    environment.update(env or {})
    print("+ " + " ".join(map(str, args)), flush=True)
    with subprocess.Popen(list(map(str, args)), cwd=cwd or ROOT, env=environment,
                          stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=None, start_new_session=not WINDOWS) as child:
        try:
            output, _ = child.communicate(data)
        except BaseException:
            if WINDOWS:
                subprocess.run(["taskkill", "/T", "/F", "/PID", str(child.pid)], check=False)
            else:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
            child.wait()
            raise
        if child.returncode:
            print(output.decode(errors="replace"), flush=True)
            raise RuntimeError(f"command failed ({child.returncode}): {args}")
        return output


def digest(data):
    return hashlib.sha256(data).hexdigest()


def sync_dir(path):
    if WINDOWS:
        return  # Directories cannot be opened for fsync; NTFS journals the rename.
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def atomic(path, data, mode=0o600):
    fd, name = tempfile.mkstemp(prefix=".write-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            if not WINDOWS:
                os.fchmod(stream.fileno(), mode)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
        sync_dir(path.parent)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def write_json(path, value):
    atomic(path, (json.dumps(value, indent=2, sort_keys=True) + "\n").encode())


def regular(path):
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(info.st_mode):
        raise RuntimeError(f"refusing non-regular file: {path}")
    return {"sha256": digest(path.read_bytes()), "mode": stat.S_IMODE(info.st_mode)}


def directory(path):
    if path.is_symlink() or (path.exists() and not path.is_dir()):
        raise RuntimeError(f"refusing non-directory: {path}")


def record(path):
    if regular(path) is None:
        return None
    value = json.loads(path.read_bytes())
    if value.get("schema_version") != 1 or set(value.get("files", {})) != set(NAMES):
        raise RuntimeError(f"invalid ownership record: {path}")
    for entry in value["files"].values():
        if (not re.fullmatch(r"[0-9a-f]{64}", entry.get("sha256", ""))
                or entry.get("mode") != 0o755):
            raise RuntimeError(f"invalid binary fingerprint: {path}")
    if not isinstance(value.get("bin_created"), bool):
        raise RuntimeError(f"invalid ownership record: {path}")
    return value


@contextmanager
def locked(prefix):
    # Do not resolve symlinks: a moved prefix works, a redirected bin/state does not.
    prefix = Path(os.path.abspath(prefix))
    directory(prefix)
    prefix = prefix.parent.resolve() / prefix.name
    prefix.mkdir(parents=True, exist_ok=True)
    state = prefix / STATE
    directory(state)
    state.mkdir(mode=0o700, exist_ok=True)
    descriptor = os.open(state / "lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise RuntimeError("invalid install lock")
        import fcntl
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError("another install owns this prefix") from error
        directory(prefix / "bin")
        yield prefix, state
    finally:
        os.close(descriptor)


def recover(prefix, state):
    journal = state / "journal.json"
    if regular(journal) is None:
        return
    transaction = json.loads(journal.read_bytes())
    if transaction.get("schema_version") != 1 or set(transaction.get("before", {})) != set(NAMES):
        raise RuntimeError("invalid install journal")
    directory(state / "rollback")
    # Validate everything before restoring anything. External edits are never discarded.
    for name in NAMES:
        before = transaction["before"][name]
        after = transaction["after"][name]
        current = regular(prefix / "bin" / name)
        if current not in (before, after):
            raise RuntimeError(f"external modification during interrupted install: {name}")
        if before is not None and regular(state / "rollback" / name) != before:
            raise RuntimeError(f"damaged rollback copy: {name}")
    for name in NAMES:
        path = prefix / "bin" / name
        before = transaction["before"][name]
        if before is None:
            path.unlink(missing_ok=True)
        else:
            atomic(path, (state / "rollback" / name).read_bytes(), before["mode"])
    if (prefix / "bin").exists():
        sync_dir(prefix / "bin")
    if transaction["record"] is None:
        (state / "ownership.json").unlink(missing_ok=True)
    else:
        write_json(state / "ownership.json", transaction["record"])
    journal.unlink()
    sync_dir(state)
    if transaction["bin_created"]:
        try:
            (prefix / "bin").rmdir()
        except OSError:
            pass  # Other files in the prefix remain untouched.


def unpack(artifact):
    """Contents include the bundled rg when present; older releases have none."""
    payload = Path(artifact).read_bytes()
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
        entries = archive.getmembers()
        names = [e.name for e in entries]
        required = {*NAMES, "manifest.json"}
        if (len(names) != len(set(names)) or not required <= set(names) <= required | {"rg"}
                or any(not e.isfile() or e.size > 512 * 1024 ** 2 for e in entries)):
            raise RuntimeError("artifact must contain only regular chui, tiechui, rg and manifest.json files")
        contents = {e.name: archive.extractfile(e).read() for e in entries}
        manifest = json.loads(contents.pop("manifest.json"))
        expected = dict(manifest.get("files", {}), **manifest.get("bundled", {}))
        if (manifest.get("schema_version") != 1 or set(manifest.get("files", {})) != set(NAMES)
                or set(expected) != set(contents)):
            raise RuntimeError("unsupported artifact manifest")
        for name, data in contents.items():
            if {key: expected[name].get(key) for key in ("sha256", "mode")} != {"sha256": digest(data), "mode": 0o755}:
                raise RuntimeError(f"artifact checksum mismatch: {name}")
        return manifest, contents, digest(payload)


def transact(prefix, state, old, contents, new, checkpoint=lambda _: None):
    rollback = state / "rollback"
    directory(rollback)
    rollback.mkdir(mode=0o700, exist_ok=True)
    before = {name: regular(prefix / "bin" / name) for name in NAMES}
    for name in NAMES:
        if before[name] is not None:
            atomic(rollback / name, (prefix / "bin" / name).read_bytes(), before[name]["mode"])
    bin_created = not (prefix / "bin").exists()
    after = {name: {"sha256": digest(contents[name]), "mode": 0o755}
             if contents is not None else None for name in NAMES}
    write_json(state / "journal.json", {"schema_version": 1, "before": before,
               "after": after, "record": old, "bin_created": bin_created})
    checkpoint("journal")
    (prefix / "bin").mkdir(exist_ok=True)
    for name in NAMES:
        path = prefix / "bin" / name
        if regular(path) != before[name]:
            raise RuntimeError(f"file changed during install: {name}")
        if contents is None:
            path.unlink(missing_ok=True)
            sync_dir(path.parent)
        else:
            atomic(path, contents[name], 0o755)
        checkpoint(name)
    if new is None:
        (state / "ownership.json").unlink(missing_ok=True)
    else:
        write_json(state / "ownership.json", new)
    checkpoint("record")
    (state / "journal.json").unlink()
    sync_dir(state)
    shutil.rmtree(rollback)
    if new is None and old["bin_created"]:
        try:
            (prefix / "bin").rmdir()
        except OSError:
            pass


def install(artifact, prefix, checkpoint=lambda _: None):
    manifest, contents, artifact_hash = unpack(artifact)
    with locked(prefix) as (prefix, state):
        recover(prefix, state)
        old = record(state / "ownership.json")
        for name in NAMES:
            current = regular(prefix / "bin" / name)
            if current is not None and (old is None or current != old["files"][name]):
                raise RuntimeError(f"refusing to overwrite unowned or modified file: {name}")
        new = {"schema_version": 1, "files": manifest["files"],
               "artifact_sha256": artifact_hash,
               "bin_created": old["bin_created"] if old else not (prefix / "bin").exists()}
        transact(prefix, state, old, contents, new, checkpoint)


def uninstall(prefix, checkpoint=lambda _: None):
    with locked(prefix) as (prefix, state):
        recover(prefix, state)
        old = record(state / "ownership.json")
        if old is None:
            return
        for name in NAMES:
            current = regular(prefix / "bin" / name)
            if current is not None and current != old["files"][name]:
                raise RuntimeError(f"refusing to remove modified file: {name}")
        transact(prefix, state, old, None, None, checkpoint)


def space():
    free = shutil.disk_usage(ROOT).free
    if free < MIN_FREE:
        raise RuntimeError(f"release build needs at least 6 GiB free; available {free / 1024 ** 3:.2f} GiB")


@contextmanager
def source(commit=None):
    """Export an immutable Git tree; candidate objects/index never touch .git."""
    local = ROOT / ".local"
    local.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="source-", dir=local) as temporary:
        temp = Path(temporary)
        base = run(["git", "rev-parse", "--verify", "--end-of-options",
                    (commit or "HEAD") + "^{commit}"]).decode().strip()
        environment = {}
        tree = run(["git", "rev-parse", base + "^{tree}"]).decode().strip()
        if commit is None:
            (temp / "objects").mkdir()
            objects = run(["git", "rev-parse", "--git-path", "objects"]).decode().strip()
            environment = {"GIT_INDEX_FILE": str(temp / "index"),
                           "GIT_OBJECT_DIRECTORY": str(temp / "objects"),
                           "GIT_ALTERNATE_OBJECT_DIRECTORIES": str((ROOT / objects).resolve())}
            paths = run(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"]).split(b"\0")
            entries = []
            for raw in sorted(set(paths) - {b""}):
                path = ROOT / os.fsdecode(raw)
                if not path.exists() and not path.is_symlink():
                    continue
                if path.is_symlink():
                    data, mode = os.fsencode(os.readlink(path)), "120000"
                else:
                    data = path.read_bytes()
                    mode = "100755" if path.stat().st_mode & 0o111 else "100644"
                # Hash locally first; only invoke git for objects absent from the base store.
                oid = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
                if not (Path(environment["GIT_ALTERNATE_OBJECT_DIRECTORIES"]) / oid[:2] / oid[2:]).exists():
                    oid = run(["git", "hash-object", "-w", "--stdin"], env=environment, data=data).decode().strip()
                entries.append(mode.encode() + b" " + oid.encode() + b"\t" + raw + b"\0")
            run(["git", "update-index", "-z", "--index-info"], env=environment, data=b"".join(entries))
            tree = run(["git", "write-tree"], env=environment).decode().strip()
        archive = temp / "source.tar"
        run(["git", "archive", "--format=tar", "--mtime=@0", "--output", archive, tree], env=environment)
        yield archive, {"commit": base, "tree": tree,
                        "source_kind": "commit" if commit else "review-candidate",
                        "source_sha256": digest(archive.read_bytes())}


def binaries(host):
    return tuple(name + ".exe" for name in NAMES) if "windows" in host else NAMES


def rg_name(host):
    return "rg.exe" if "windows" in host else "rg"


def ripgrep(host, destination):
    """Places the pinned rg for host in destination, verifying the upstream archive first."""
    if host not in RIPGREP_ASSETS:
        raise RuntimeError(f"no bundled ripgrep for {host}")
    target, size, checksum = RIPGREP_ASSETS[host]
    asset = f"ripgrep-{RIPGREP}-{target}." + ("zip" if "windows" in host else "tar.gz")
    cache = ROOT / ".local/ripgrep" / asset
    data = cache.read_bytes() if cache.is_file() else b""
    if len(data) != size or digest(data) != checksum:
        url = f"https://github.com/BurntSushi/ripgrep/releases/download/{RIPGREP}/{asset}"
        print(f"Downloading {url}", flush=True)
        with urllib.request.urlopen(url, timeout=120) as response:
            data = response.read(size + 1)
        if len(data) != size or digest(data) != checksum:
            raise RuntimeError(f"ripgrep archive does not match its pinned checksum: {asset}")
        cache.parent.mkdir(parents=True, exist_ok=True)
        atomic(cache, data, 0o644)
    member = f"ripgrep-{RIPGREP}-{target}/{rg_name(host)}"
    if "windows" in host:
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            binary = archive.read(member)
    else:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
            binary = archive.extractfile(member).read()
    destination.mkdir(parents=True, exist_ok=True)
    (destination / rg_name(host)).write_bytes(binary)
    (destination / rg_name(host)).chmod(0o755)
    return destination / rg_name(host)


def build(archive, destination, jobs=2):
    space()
    with tempfile.TemporaryDirectory(prefix="dist-build-", dir=ROOT / ".local") as temporary:
        export = Path(temporary)
        with tarfile.open(archive) as sources:
            # The archive is produced by git from the selected local tree.
            sources.extractall(export)
        target = export / "target"
        rust = run(["rustc", "-vV"]).decode()
        host = re.search(r"^host: (.+)$", rust, re.M).group(1)
        run(["cargo", "build", "--release", "--locked", "--offline", "-p", "tiechui-cli", "--bins"],
            cwd=export, env={"CARGO_TARGET_DIR": str(target), "CARGO_BUILD_JOBS": str(jobs),
                             "RUSTFLAGS": f"--remap-path-prefix={export}=/tiechui"})
        for name in binaries(host):
            shutil.copy2(target / "release" / name, destination / name)
        version = re.search(r'(?m)^version = "([^"]+)"', (export / "Cargo.toml").read_text()).group(1)
        return version, host, rust, digest((export / "Cargo.lock").read_bytes())


def package(output, version, host, metadata, identical):
    if host not in NPM:
        raise RuntimeError(f"unsupported packaging host: {host}")
    names = binaries(host)
    rg = rg_name(host)
    files = {name: {"sha256": digest((output / name).read_bytes()), "mode": 0o755} for name in names}
    bundled = {rg: {"sha256": digest((output / rg).read_bytes()), "mode": 0o755, "version": RIPGREP}}
    manifest = dict(metadata, schema_version=1, version=version, target=host, files=files,
                    bundled=bundled, reproducible=identical)
    write_json(output / "manifest.json", manifest)
    members = (*names, rg)
    if "windows" in host:
        artifact = output / f"tiechui-{version}-{host}.zip"
        with zipfile.ZipFile(artifact, "w", zipfile.ZIP_DEFLATED) as archive:
            for name in (*members, "manifest.json"):
                archive.writestr(zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0)), (output / name).read_bytes(),
                                 zipfile.ZIP_DEFLATED)
    else:
        artifact = output / f"tiechui-{version}-{host}.tar.gz"
        with artifact.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for name in (*members, "manifest.json"):
                    data = (output / name).read_bytes()
                    info = tarfile.TarInfo(name)
                    info.size, info.mode = len(data), 0o755 if name in members else 0o644
                    archive.addfile(info, io.BytesIO(data))
    checksum = digest(artifact.read_bytes())
    (output / "SHA256SUMS").write_text("".join(
        f"{digest(path.read_bytes())}  {path.name}\n" for path in
        (artifact, output / "manifest.json", *(output / name for name in members))))
    if "windows" in host:
        return artifact
    (output / "tiechui.rb").write_text(
        'class Tiechui < Formula\n  desc "Rust terminal coding agent"\n'
        f'  url "{artifact.as_uri()}"\n  version "{version}"\n  sha256 "{checksum}"\n'
        '  def install\n    bin.install "chui", "tiechui"\n    (libexec/"tiechui").install "rg"\n  end\nend\n')
    return artifact


def dist(archive, metadata, output, ci=False):
    """CI builds once on every core; local runs compare two isolated builds on two jobs."""
    output.mkdir(parents=True, exist_ok=True)
    version, host, rust, lock = build(archive, output, (os.cpu_count() or 2) if ci else 2)
    ripgrep(host, output)
    identical = None
    if not ci:
        with tempfile.TemporaryDirectory(prefix="dist-compare-", dir=ROOT / ".local") as second:
            build(archive, Path(second))
            identical = all((output / name).read_bytes() == (Path(second) / name).read_bytes()
                            for name in binaries(host))
    metadata = dict(metadata, rustc=rust, lock_sha256=lock)
    artifact = package(output, version, host, metadata, identical)
    if identical is not None:
        print(f"Two isolated builds byte-identical: {identical}", flush=True)
    print(f"Artifact: {artifact}", flush=True)
    return artifact, identical


def npm(assets, version, output):
    """@tiechui/cli and one version of it per platform, from verified release archives."""
    sums = {name: checksum for checksum, name in
            (line.split("  ", 1) for line in (assets / "SHA256SUMS").read_text().splitlines())}
    common = {"license": "AGPL-3.0-only", "homepage": REPOSITORY,
              "repository": {"type": "git", "url": f"git+{REPOSITORY}.git"}}
    optional = {}
    for host, platform in NPM.items():
        name = f"tiechui-{version}-{host}." + ("zip" if "windows" in host else "tar.gz")
        archive = assets / name
        if not archive.is_file() or sums.get(name) != digest(archive.read_bytes()):
            raise RuntimeError(f"missing or mismatched release archive: {name}")
        # chui finds the bundled rg beside itself.
        names = (binaries(host)[0], rg_name(host))
        if "windows" in host:
            with zipfile.ZipFile(archive) as files:
                contents = {name: files.read(name) for name in names}
        else:
            contents = unpack(archive)[1]
        os_name, cpu = platform.split("-")
        package = output / platform
        package.mkdir(parents=True)
        for name in names:
            (package / name).write_bytes(contents[name])
            (package / name).chmod(0o755)
        manifest = dict(common, name="@tiechui/cli", version=f"{version}-{platform}",
                        os=[os_name], cpu=[cpu], files=list(names))
        if os_name == "linux":
            manifest["libc"] = ["glibc"]
        (package / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
        optional[f"@tiechui/cli-{platform}"] = f"npm:@tiechui/cli@{version}-{platform}"
    main = output / "cli"
    (main / "bin").mkdir(parents=True)
    shutil.copy2(ROOT / "scripts/npm/chui.js", main / "bin/chui.js")
    (main / "bin/chui.js").chmod(0o755)
    for name in ("README.md", "LICENSE"):
        shutil.copy2(ROOT / name, main / name)
    manifest = dict(common, name="@tiechui/cli", version=version,
                    description="A terminal coding agent written in Rust",
                    bin={name: "bin/chui.js" for name in NAMES}, files=["bin"],
                    engines={"node": ">=16"}, optionalDependencies=optional)
    (main / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return main


def deny_check():
    # A local config copy avoids mutating deny.toml even on cancellation.
    source_db = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))) / "advisory-dbs"
    databases = sorted(source_db.glob("advisory-db-*"))
    if not databases:
        raise RuntimeError("offline advisory database is missing")
    with tempfile.TemporaryDirectory(prefix="advisories-", dir=ROOT / ".local") as temporary:
        target_db = Path(temporary) / "db"
        target_db.mkdir()
        for database in databases:
            if database.is_dir():
                shutil.copytree(database, target_db / database.name)
        config = (ROOT / "deny.toml").read_text().replace(
            "[advisories]", "[advisories]\ndb-path = " + json.dumps(str(target_db)), 1)
        path = Path(temporary) / "deny.toml"
        path.write_text(config)
        run(["cargo", "deny", "--locked", "--offline", "--config", path, "check"])


def verify():
    space()
    output = Path(tempfile.mkdtemp(prefix="release-", dir=ROOT / ".local"))
    report = {"status": "FAIL", "steps": [], "deny_database": "temporary copy under .local/"}

    def step(name, action):
        print(f"Release verification: {name}", flush=True)
        with (output / (name + ".log")).open("wb") as log:
            # Keep subprocess output as evidence without buffering the entire build in the report.
            saved_out, saved_err = os.dup(1), os.dup(2)
            try:
                os.dup2(log.fileno(), 1)
                os.dup2(log.fileno(), 2)
                result = action()
                if isinstance(result, bytes):
                    log.write(result)
            finally:
                os.dup2(saved_out, 1)
                os.dup2(saved_err, 2)
                os.close(saved_out)
                os.close(saved_err)
        report["steps"].append(name)
        return result

    try:
        with source() as (archive, metadata):
            report.update(metadata)
            step("locked-dependencies", lambda: run(["cargo", "metadata", "--locked", "--offline", "--format-version", "1"]))
            step("checks", lambda: run(["just", "fmt", "lint", "test", "schema-check", "sdk-check", "packaging-check"]))
            step("deny", deny_check)
            step("pty", lambda: run(["just", "pty"]))
            artifact, identical = step("dist", lambda: dist(archive, metadata, output / "dist"))
            report.update(artifact=str(artifact), artifact_sha256=digest(artifact.read_bytes()), reproducible=identical)
            with tempfile.TemporaryDirectory(prefix="release-install-", dir=ROOT / ".local") as temporary:
                prefix = Path(temporary) / "prefix"
                step("install", lambda: install(artifact, prefix))
                home = Path(temporary) / "profile"
                home.mkdir(mode=0o700)
                step("doctor", lambda: run([prefix / "bin/chui", "doctor", "--prefix", prefix, "--profile", home]))
                for theme in ("dark", "light"):
                    step("installed-" + theme, lambda theme=theme: run(
                        ["python3", "-B", "tests/pty/smoke.py", "streaming", "--background", theme],
                        env={"TIECHUI_BIN": str(prefix / "bin/chui")}))
                step("uninstall", lambda: uninstall(prefix))
            with source() as (_, final_source):
                if final_source["tree"] != metadata["tree"]:
                    raise RuntimeError("source changed during verification; rerun for the current candidate")
            report["status"] = "PASS"
    except BaseException as error:
        report["error"] = str(error)
        raise
    finally:
        write_json(output / "report.json", report)
        (output / "report.md").write_text(
            f'# Release verification: {report["status"]}\n\n'
            + "```json\n" + json.dumps(report, indent=2) + "\n```\n")
        print(f"Release report: {output / 'report.md'}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    command = commands.add_parser("dist")
    command.add_argument("--commit", default="HEAD")
    command.add_argument("--ci", action="store_true")
    commands.add_parser("verify-release")
    command = commands.add_parser("npm")
    command.add_argument("assets", type=Path)
    command.add_argument("--version", required=True)
    command.add_argument("--output", type=Path, required=True)
    command = commands.add_parser("ripgrep")
    command.add_argument("target", choices=sorted(RIPGREP_ASSETS))
    command.add_argument("--output", type=Path, required=True)
    for name in ("install", "uninstall"):
        command = commands.add_parser(name)
        command.add_argument("--prefix", type=Path, required=True)
        if name == "install":
            command.add_argument("artifact", type=Path)
    args = parser.parse_args()
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt("terminated")))
    (ROOT / ".local").mkdir(exist_ok=True)
    if args.command == "dist":
        space()
        with source(args.commit) as (archive, metadata):
            output = Path(tempfile.mkdtemp(prefix="dist-", dir=ROOT / ".local"))
            dist(archive, metadata, output, args.ci)
    elif args.command == "verify-release":
        verify()
    elif args.command == "npm":
        npm(args.assets, args.version, args.output)
    elif args.command == "ripgrep":
        print(ripgrep(args.target, args.output), flush=True)
    elif args.command == "install":
        install(args.artifact, args.prefix)
    else:
        uninstall(args.prefix)


if __name__ == "__main__":
    main()
