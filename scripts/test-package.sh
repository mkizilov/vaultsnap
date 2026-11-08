#!/bin/sh
# Tests scripts/package.sh in a scratch copy of the repository with stand-ins for cargo and
# rustc: that it refuses to run without a Cargo.lock, that it asks cargo for a locked release
# build of the right package (and the right target), and that the archive and its checksum hold
# what a release should hold. It cannot test the real build; that is the release workflow's job.
#
#   sh scripts/test-package.sh
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/vaultsnap-package-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

repo=$work/repo
mkdir -p "$repo/scripts" "$work/bin"
cp "$here/scripts/package.sh" "$repo/scripts/"
cp "$here/Cargo.toml" "$here/LICENSE" "$here/README.md" "$here/CHANGELOG.md" "$repo/"
cp -R "$here/docs" "$here/contrib" "$repo/"

cat > "$work/bin/rustc" <<'STUB'
#!/bin/sh
echo "rustc 1.94.1 (stand-in)"
echo "host: x86_64-unknown-linux-gnu"
STUB
cat > "$work/bin/cargo" <<'STUB'
#!/bin/sh
# Records the call and puts a stand-in binary where cargo would have built one.
echo "$*" >> "$CARGO_LOG"
dir=target/release
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--target" ]; then
        dir=target/$2/release
    fi
    shift
done
mkdir -p "$dir"
printf '#!/bin/sh\necho "vaultsnap stand-in"\n' > "$dir/vaultsnap"
chmod +x "$dir/vaultsnap"
STUB
chmod +x "$work/bin/rustc" "$work/bin/cargo"

failures=0
fail() {
    failures=$((failures + 1))
    echo "FAIL: $*" >&2
}
pass() {
    echo "ok: $*"
}

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$here/Cargo.toml" | head -n 1)
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }
export CARGO_LOG=$work/cargo.log
: > "$CARGO_LOG"

# Without a Cargo.lock it refuses before building anything.
if PATH="$work/bin:$PATH" sh "$repo/scripts/package.sh" 2> "$work/err"; then
    fail "it built a package without a Cargo.lock"
elif grep -q 'Cargo.lock' "$work/err" && [ ! -s "$CARGO_LOG" ]; then
    pass "a missing Cargo.lock is refused before any build"
else
    fail "the refusal for a missing Cargo.lock is wrong: $(cat "$work/err")"
fi

echo "# stand-in lock file" > "$repo/Cargo.lock"

# A build for this machine.
PATH="$work/bin:$PATH" sh "$repo/scripts/package.sh" > "$work/out" 2>&1 || {
    fail "package.sh failed: $(cat "$work/out")"
}
name=vaultsnap-$version-x86_64-unknown-linux-gnu
archive=$repo/dist/$name.tar.gz
if [ "$(cat "$CARGO_LOG")" = "build --release --locked -p vaultsnap-cli" ]; then
    pass "a locked release build of the CLI package for the host"
else
    fail "unexpected cargo call: $(cat "$CARGO_LOG")"
fi
if [ -f "$archive" ] && [ -f "$archive.sha256" ]; then
    pass "the archive and its checksum exist"
else
    fail "no archive or checksum in $repo/dist"
fi
if [ -f "$archive" ]; then
    tar -tzf "$archive" | sort > "$work/listing"
    for entry in \
        "$name/vaultsnap" "$name/LICENSE" "$name/README.md" "$name/CHANGELOG.md" \
        "$name/docs/GUIDE.md" "$name/docs/DISASTER_RECOVERY.md" \
        "$name/contrib/vaultsnap-scheduled.sh" "$name/contrib/systemd/vaultsnap-backup.timer"; do
        if grep -qx "$entry" "$work/listing"; then
            pass "the archive holds $entry"
        else
            fail "the archive lacks $entry"
        fi
    done
    if (cd "$repo/dist" && if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$name.tar.gz.sha256"; else shasum -a 256 -c "$name.tar.gz.sha256"; fi) >/dev/null 2>&1; then
        pass "the checksum verifies"
    else
        fail "the checksum does not verify"
    fi
    tar -xzf "$archive" -C "$work"
    if [ -x "$work/$name/vaultsnap" ]; then
        pass "the packed binary is executable"
    else
        fail "the packed binary is not executable"
    fi
fi

# A build for another target goes through --target and a different output directory.
: > "$CARGO_LOG"
PATH="$work/bin:$PATH" sh "$repo/scripts/package.sh" aarch64-apple-darwin > "$work/out" 2>&1 || {
    fail "package.sh for another target failed: $(cat "$work/out")"
}
if [ "$(cat "$CARGO_LOG")" = "build --release --locked -p vaultsnap-cli --target aarch64-apple-darwin" ]; then
    pass "another target is built with --target"
else
    fail "unexpected cargo call for another target: $(cat "$CARGO_LOG")"
fi
if [ -f "$repo/dist/vaultsnap-$version-aarch64-apple-darwin.tar.gz" ]; then
    pass "the archive for another target is named after it"
else
    fail "no archive for the other target"
fi

if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed" >&2
    exit 1
fi
echo "all checks passed"
