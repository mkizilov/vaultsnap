#!/bin/sh
# Builds the release binary and packs it, with the documentation and the scheduler files, into
#
#   dist/vaultsnap-<version>-<target>.tar.gz
#   dist/vaultsnap-<version>-<target>.tar.gz.sha256
#
# Usage:
#
#   scripts/package.sh                          # for this machine
#   scripts/package.sh aarch64-apple-darwin     # for another target (it must be installed:
#                                               #   rustup target add aarch64-apple-darwin)
#
# The build uses the locked dependency versions, so a Cargo.lock must exist. The binary reports
# the git commit it was built from; when building from a source archive without git, set
# VAULTSNAP_BUILD_COMMIT to say which commit that is. docs/RELEASING.md describes the release
# process around this script.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

die() {
    echo "package.sh: $*" >&2
    exit 1
}

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || die "cannot read the version from Cargo.toml"
[ -f Cargo.lock ] || die "Cargo.lock is missing: build once with network access and commit it"

host=$(rustc -vV | sed -n 's/^host: //p')
[ -n "$host" ] || die "cannot tell which platform rustc builds for"
target=${1:-$host}

if [ "$target" = "$host" ]; then
    cargo build --release --locked -p vaultsnap-cli
    bin=target/release/vaultsnap
else
    cargo build --release --locked -p vaultsnap-cli --target "$target"
    bin=target/$target/release/vaultsnap
fi
[ -x "$bin" ] || die "the build did not produce $bin"

if [ "$target" = "$host" ]; then
    # A binary that cannot run on the machine that built it is not worth shipping.
    "$bin" --version >/dev/null || die "the new binary does not run"
    "$bin" version >/dev/null || die "the new binary cannot describe itself"
fi

name=vaultsnap-$version-$target
rm -rf "dist/$name" "dist/$name.tar.gz" "dist/$name.tar.gz.sha256"
mkdir -p "dist/$name/docs" "dist/$name/contrib"
cp "$bin" "dist/$name/vaultsnap"
cp LICENSE README.md CHANGELOG.md "dist/$name/"
cp docs/*.md "dist/$name/docs/"
cp -R contrib/. "dist/$name/contrib/"
tar -C dist -czf "dist/$name.tar.gz" "$name"
rm -rf "dist/$name"

cd dist
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
else
    shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"
fi
echo "dist/$name.tar.gz"
cat "$name.tar.gz.sha256"
