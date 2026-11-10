#!/bin/sh
# Installs chui and tiechui, plus the bundled rg, from a GitHub release.
# Usage: curl -fsSL https://github.com/b1ank1108/tiechui/releases/latest/download/install.sh | sh
# Environment: TIECHUI_VERSION (default: latest), TIECHUI_INSTALL_DIR (default: ~/.local/bin).
set -eu

repo="https://github.com/b1ank1108/tiechui"
dir="${TIECHUI_INSTALL_DIR:-$HOME/.local/bin}"
# chui looks for its bundled rg here, so the user's own rg on PATH is never replaced.
libexec="$(dirname "$dir")/libexec/tiechui"

fail() {
    echo "install.sh: $*" >&2
    exit 1
}

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        fail "curl or wget is required"
    fi
}

case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-gnu ;;
    *) fail "unsupported OS $(uname -s); on Windows use install.ps1" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) cpu=x86_64 ;;
    arm64 | aarch64) cpu=aarch64 ;;
    *) fail "unsupported CPU $(uname -m)" ;;
esac
# An x86_64 shell under Rosetta still runs on Apple silicon.
if [ "$os" = apple-darwin ] && [ "$cpu" = x86_64 ] &&
    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
    cpu=aarch64
fi

version="${TIECHUI_VERSION:-}"
if [ -z "$version" ]; then
    if command -v curl >/dev/null 2>&1; then
        latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$repo/releases/latest")
    else
        latest=$(wget -S --spider "$repo/releases/latest" 2>&1 | sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1)
    fi
    version=${latest##*/}
fi
version=${version#v}
case "$version" in
    [0-9]*) ;;
    *) fail "could not determine the latest version; set TIECHUI_VERSION" ;;
esac

asset="tiechui-$version-$cpu-$os.tar.gz"
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT INT TERM
echo "Downloading $asset"
fetch "$repo/releases/download/v$version/$asset" "$temp/$asset"
fetch "$repo/releases/download/v$version/SHA256SUMS" "$temp/SHA256SUMS"

expected=$(awk -v name="$asset" '$2 == name { print $1 }' "$temp/SHA256SUMS")
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temp/$asset" | awk '{ print $1 }')
else
    actual=$(shasum -a 256 "$temp/$asset" | awk '{ print $1 }')
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || fail "checksum mismatch for $asset"

tar -xzf "$temp/$asset" -C "$temp" chui tiechui
# Older releases do not bundle rg.
if tar -tzf "$temp/$asset" | grep -qx rg; then
    tar -xzf "$temp/$asset" -C "$temp" rg
    mkdir -p "$libexec"
    mv -f "$temp/rg" "$libexec/rg"
    chmod 755 "$libexec/rg"
fi
mkdir -p "$dir"
for name in chui tiechui; do
    mv -f "$temp/$name" "$dir/$name"
    chmod 755 "$dir/$name"
done
echo "Installed chui $version to $dir"

case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "Add $dir to PATH, for example: export PATH=\"$dir:\$PATH\"" ;;
esac
