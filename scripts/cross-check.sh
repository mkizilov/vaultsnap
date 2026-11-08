#!/bin/sh
# Compiles the whole workspace, tests and benchmarks included, for other targets than this
# machine's, without linking or running anything. Linux targets are linted too (clippy with
# -D warnings); Windows is only type-checked, because Windows support is not finished and its
# test code still has unused-import warnings for the Unix-only helpers (ROADMAP.md, item 20).
#
#   scripts/cross-check.sh                                # both Linux targets
#   scripts/cross-check.sh x86_64-unknown-linux-gnu       # one target
#   scripts/cross-check.sh x86_64-pc-windows-gnu          # Windows, type-check only
#
# What it shows: the code type-checks (and for Linux, is lint-clean) for those targets, including
# the C code of the dependencies that have some (zstd-sys, aws-lc-sys), which is cross-compiled
# with zig. What it does not show: that anything runs there. That needs a real machine.
#
# Needs rustup (with the toolchain of rust-toolchain.toml) and python3. zig comes from the
# `ziglang` wheel, installed into .venv inside the repository (git-ignored, about 430 MB); nothing
# is installed anywhere else. Supported targets: x86_64-unknown-linux-gnu,
# aarch64-unknown-linux-gnu, x86_64-pc-windows-gnu.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
ZIGLANG_VERSION=0.17.1
venv=${VAULTSNAP_VENV:-$root/.venv}
py=$venv/bin/python

die() {
    echo "cross-check.sh: $*" >&2
    exit 1
}

[ "$#" -gt 0 ] || set -- x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
command -v python3 >/dev/null 2>&1 || die "python3 is needed to install zig"

if ! "$py" -m ziglang version 2>/dev/null | grep -qx "$ZIGLANG_VERSION"; then
    echo "installing ziglang $ZIGLANG_VERSION into $venv"
    python3 -m venv "$venv"
    "$venv/bin/pip" install --quiet --disable-pip-version-check "ziglang==$ZIGLANG_VERSION"
fi

toolchain=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
work=$(mktemp -d "${TMPDIR:-/tmp}/cross-check.XXXXXX")
trap 'rm -rf "$work"' EXIT

printf '#!/bin/sh\nexec "%s" -m ziglang ar "$@"\n' "$py" > "$work/zar"
chmod +x "$work/zar"
# zig keeps a cache of its compiled runtime pieces; target/ is git-ignored and survives between runs.
export ZIG_LOCAL_CACHE_DIR=$root/target/zig-cache ZIG_GLOBAL_CACHE_DIR=$root/target/zig-cache

status=0
for target in "$@"; do
    case "$target" in
        x86_64-unknown-linux-gnu) zig_target=x86_64-linux-gnu ;;
        aarch64-unknown-linux-gnu) zig_target=aarch64-linux-gnu ;;
        x86_64-pc-windows-gnu) zig_target=x86_64-windows-gnu ;;
        *) die "unsupported target $target (see the top of this file)" ;;
    esac
    if command -v rustup >/dev/null 2>&1; then
        rustup target add --toolchain "$toolchain" "$target" >/dev/null 2>&1 ||
            die "cannot add the Rust target $target"
    fi
    # cc-rs adds a clang-style --target=<rust triple>, which zig cannot parse; zig gets its own.
    cat > "$work/cc-$target" <<EOF
#!/bin/sh
for a; do shift; case "\$a" in --target=*) ;; *) set -- "\$@" "\$a" ;; esac; done
exec "$py" -m ziglang cc -target $zig_target "\$@"
EOF
    chmod +x "$work/cc-$target"
    var=$(printf '%s' "$target" | tr '-' '_')
    export "CC_$var=$work/cc-$target" "AR_$var=$work/zar"

    case "$target" in
        *windows*)
            echo "== $target: cargo check --workspace --all-targets"
            cargo check --workspace --all-targets --target "$target" --locked && ok=1 || ok=0
            ;;
        *)
            echo "== $target: cargo clippy --workspace --all-targets -- -D warnings"
            cargo clippy --workspace --all-targets --target "$target" --locked -- -D warnings &&
                ok=1 || ok=0
            ;;
    esac
    if [ "$ok" = 1 ]; then
        echo "== $target: ok"
    else
        echo "== $target: FAILED" >&2
        status=1
    fi
done
exit $status
