#!/usr/bin/env python3
"""Maintain and test the sibling plugins checkout using isolated package copies."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PLUGINS = ROOT.parent / "tiechui-plugins"
SDK_FILES = ("index.js", "index.d.ts", "protocol.ts", "package.json")


def copy_repository(destination):
    shutil.copytree(PLUGINS, destination,
                    ignore=shutil.ignore_patterns(".git", ".local", "target", "server"))


def commit_fixture(repository, home):
    env = {"PATH": os.environ["PATH"], "HOME": str(home),
           "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
           "GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
           "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid"}

    def git(*args):
        return subprocess.run(["git", "-c", "core.hooksPath=/dev/null", *args],
                              cwd=repository, env=env, check=True, capture_output=True).stdout.decode().strip()

    git("init", "-q", "-b", "main")
    for path in sorted(repository.rglob("*")):
        relative = path.relative_to(repository)
        if ".git" in relative.parts or not path.is_file():
            continue
        mode = "100755" if os.access(path, os.X_OK) else "100644"
        blob = git("hash-object", "-w", str(relative))
        git("update-index", "--add", "--cacheinfo", mode + "," + blob + "," + str(relative))
    revision = git("commit-tree", git("write-tree"), "-m", "Plugin marketplace fixture")
    git("update-ref", "refs/heads/main", revision)
    git("tag", "fixture-v1", revision)
    return revision


def build_rust():
    base = ROOT / "target/plugin-build"
    base.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=base) as directory:
        package = Path(directory) / "rust"
        shutil.copytree(PLUGINS / "plugins/sdk-rust", package,
                        ignore=shutil.ignore_patterns("target", "server"))
        manifest = package / "Cargo.toml"
        with manifest.open("a") as file:
            file.write('\n[workspace]\n\n[patch.crates-io]\n'
                       'tiechui-plugin-sdk = { path = ' +
                       json.dumps(str(ROOT / "crates/tiechui-plugin-sdk")) + ' }\n')
        shutil.copy2(ROOT / "Cargo.lock", package / "Cargo.lock")
        env = {**os.environ, "CARGO_INCREMENTAL": "0", "CARGO_TARGET_DIR": str(ROOT / "target")}
        subprocess.run(["cargo", "build", "--offline", "--manifest-path", str(manifest)],
                       cwd=ROOT, env=env, check=True)


def check():
    sdk = PLUGINS / "plugins/sdk-typescript/sdk"
    for name in SDK_FILES:
        if (sdk / name).read_bytes() != (ROOT / "sdk/typescript" / name).read_bytes():
            raise ValueError("Vendored SDK drift: " + name + "; run python3 -B scripts/plugins.py refresh-sdk")
    base = ROOT / ".local/plugins-check"
    base.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=base) as directory:
        temp = Path(directory)
        repository, home = temp / "repository", temp / "home"
        home.mkdir()
        copy_repository(repository)
        catalog = json.loads((repository / "marketplace.json").read_text())
        names = set()
        for entry in catalog["plugins"]:
            package = (repository / entry["source"]).resolve()
            if not package.is_relative_to(repository) or package == repository:
                raise ValueError("Marketplace path escapes plugins: " + entry["source"])
            manifest = json.loads((package / ".tiechui-plugin/plugin.json").read_text())
            if entry["name"] != manifest["name"] or entry["name"] in names:
                raise ValueError("Mismatched or duplicate marketplace name")
            names.add(entry["name"])
        if names != {"sdk-typescript", "sdk-rust"}:
            raise ValueError("Marketplace must list both examples")
        revision = commit_fixture(repository, home)
        env = {"PATH": os.environ["PATH"], "HOME": str(home), "TIECHUI_HOME": str(home)}

        def cli(*args):
            result = subprocess.run([str(ROOT / "target/debug/chui"), *args], cwd=temp,
                                    env=env, check=True, capture_output=True, text=True)
            return result.stdout

        cli("plugin", "marketplace", "add", str(repository), "--ref", "fixture-v1")
        for name in sorted(names):
            cli("plugin", "add", name + "@" + catalog["name"])
        installed = json.loads(cli("plugin", "list", "--json"))["plugins"]
        if {plugin["name"] for plugin in installed} != names:
            raise ValueError("Installed marketplace entries differ")
        for plugin in installed:
            if plugin["diagnostics"] or not plugin["extension"]:
                raise ValueError("Invalid plugin manifest: " + plugin["name"])
            if revision not in json.dumps(plugin["installation"]):
                raise ValueError("Installation is missing its marketplace pin")
        package = home / "plugins/sdk-typescript"
        subprocess.run(["node", "--test", "tests/plugin.test.ts"], cwd=package, env=env, check=True)
    print("Plugins OK: manifests, marketplace installs, SDK drift and TypeScript tests")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "refresh-sdk", "build-rust"))
    command = parser.parse_args().command
    if not PLUGINS.is_dir():
        if command == "refresh-sdk":
            parser.error("Sibling tiechui-plugins repository is missing")
        print("SKIP plugins: sibling tiechui-plugins repository is absent")
        return
    if command == "refresh-sdk":
        destination = PLUGINS / "plugins/sdk-typescript/sdk"
        destination.mkdir(parents=True, exist_ok=True)
        for name in SDK_FILES:
            shutil.copy2(ROOT / "sdk/typescript" / name, destination / name)
    elif command == "build-rust":
        build_rust()
    else:
        check()


if __name__ == "__main__":
    main()
