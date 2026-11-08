#!/bin/sh
# Runs MinIO natively, without Docker, for the S3 integration tests (docs/S3.md).
#
#   scripts/minio-local.sh install   # a pinned Go, then the pinned MinIO built from source (once)
#   scripts/minio-local.sh start     # serve on 127.0.0.1:21100 with random credentials
#   scripts/minio-local.sh env       # the lines to `eval`/source so the tests find the server
#   scripts/minio-local.sh stop      # stop the server
#   scripts/minio-local.sh clean     # stop it and delete its data and credentials
#
#   eval "$(scripts/minio-local.sh env)"
#   cargo test -p vaultsnap-core --test s3_minio
#
# Everything lives under $VAULTSNAP_TOOLS (default: .agent-local/tools in the repository, which
# is git-ignored): the Go toolchain, its caches, the MinIO binary, the data, and the credentials.
# Nothing is installed globally and the server listens on loopback only (port 21100, console 21101,
# both in the range this project may use). MinIO's own binary downloads were withdrawn when the
# project was archived, which is why this builds the pinned release from source (about two minutes,
# two build workers). Needs curl, python3, tar, and a network for `install`.
set -eu

MINIO_RELEASE=RELEASE.2025-04-22T22-12-26Z
GO_VERSION=go1.24.2
PORT=21100
CONSOLE_PORT=21101

root=$(cd "$(dirname "$0")/.." && pwd)
tools=${VAULTSNAP_TOOLS:-$root/.agent-local/tools}
goroot=$tools/goroot
gopath=$tools/gopath
minio=$gopath/bin/minio
data=$tools/minio-data
creds=$tools/minio-creds.sh
pidfile=$tools/minio.pid
log=$tools/minio-server.log

die() {
    echo "minio-local.sh: $*" >&2
    exit 1
}

go_platform() {
    case "$(uname -s)" in
        Darwin) os=darwin ;;
        Linux) os=linux ;;
        *) die "unsupported system $(uname -s)" ;;
    esac
    case "$(uname -m)" in
        arm64 | aarch64) arch=arm64 ;;
        x86_64 | amd64) arch=amd64 ;;
        *) die "unsupported processor $(uname -m)" ;;
    esac
}

install_go() {
    [ -x "$goroot/bin/go" ] && return 0
    go_platform
    mkdir -p "$tools"
    echo "looking up $GO_VERSION for $os/$arch"
    curl -fsSL --max-time 60 "https://go.dev/dl/?mode=json&include=all" -o "$tools/go-releases.json"
    fields=$(python3 - "$GO_VERSION" "$os" "$arch" "$tools/go-releases.json" <<'PY'
import json, sys
version, os_name, arch, path = sys.argv[1:5]
for release in json.load(open(path)):
    if release["version"] == version:
        for entry in release["files"]:
            if entry["os"] == os_name and entry["arch"] == arch and entry["kind"] == "archive":
                print(entry["filename"], entry["sha256"])
                raise SystemExit(0)
raise SystemExit(1)
PY
    ) || die "$GO_VERSION has no archive for $os/$arch"
    rm -f "$tools/go-releases.json"
    file=${fields% *}
    sum=${fields#* }
    curl -fsSL --max-time 600 -o "$tools/go.tgz" "https://go.dev/dl/$file"
    got=$(if command -v sha256sum >/dev/null 2>&1; then sha256sum "$tools/go.tgz"; else shasum -a 256 "$tools/go.tgz"; fi)
    [ "${got%% *}" = "$sum" ] || die "the Go archive does not match the published checksum"
    rm -rf "$goroot"
    mkdir -p "$goroot"
    tar -xzf "$tools/go.tgz" -C "$goroot" --strip-components=1
    rm -f "$tools/go.tgz"
    "$goroot/bin/go" version
}

go_env() {
    export GOROOT=$goroot GOPATH=$gopath GOCACHE=$tools/gocache GOMODCACHE=$gopath/pkg/mod
    export GOTOOLCHAIN=local CGO_ENABLED=0 GOFLAGS=-trimpath GOMAXPROCS=2
    PATH=$goroot/bin:$PATH
}

install_minio() {
    [ -x "$minio" ] && return 0
    install_go
    go_env
    echo "building MinIO $MINIO_RELEASE (a couple of minutes)"
    go install -p 2 "github.com/minio/minio@$MINIO_RELEASE"
    [ -x "$minio" ] || die "the build did not produce $minio"
}

running() {
    [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null
}

start() {
    [ -x "$minio" ] || die "MinIO is not installed: run '$0 install' first"
    if running; then
        echo "already running (pid $(cat "$pidfile"))"
        return 0
    fi
    rm -rf "$data"
    mkdir -p "$data/vaultsnap-test"
    user=ci$(openssl rand -hex 4)
    secret=$(openssl rand -hex 16)
    (umask 077 && printf 'AWS_ACCESS_KEY_ID=%s\nAWS_SECRET_ACCESS_KEY=%s\n' "$user" "$secret" > "$creds")
    MINIO_ROOT_USER=$user MINIO_ROOT_PASSWORD=$secret MINIO_BROWSER=off MINIO_UPDATE=off \
        nohup "$minio" server "$data" --address "127.0.0.1:$PORT" \
        --console-address "127.0.0.1:$CONSOLE_PORT" --quiet > "$log" 2>&1 &
    echo $! > "$pidfile"
    i=0
    while [ "$i" -lt 30 ]; do
        if curl -fsS --max-time 2 "http://127.0.0.1:$PORT/minio/health/ready" >/dev/null 2>&1; then
            echo "MinIO is ready on http://127.0.0.1:$PORT (pid $(cat "$pidfile"), bucket vaultsnap-test)"
            return 0
        fi
        i=$((i + 1))
        sleep 1
    done
    stop
    die "MinIO did not become ready; see $log"
}

stop() {
    if running; then
        pid=$(cat "$pidfile")
        kill "$pid" 2>/dev/null || true
        i=0
        while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 20 ]; do
            i=$((i + 1))
            sleep 1
        done
        kill -9 "$pid" 2>/dev/null || true
        echo "stopped (pid $pid)"
    fi
    rm -f "$pidfile"
}

print_env() {
    running || die "MinIO is not running: run '$0 start' first"
    sed 's/^/export /' "$creds"
    echo "export VAULTSNAP_S3_TEST_ENDPOINT=http://127.0.0.1:$PORT"
}

case "${1:-}" in
    install) install_minio ;;
    start) start ;;
    stop) stop ;;
    env) print_env ;;
    clean)
        stop
        rm -rf "$data" "$creds" "$log"
        ;;
    *) die "usage: $0 install|start|env|stop|clean" ;;
esac
