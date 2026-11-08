#!/bin/sh
# Tests vaultsnap-scheduled.sh against a stand-in for the vaultsnap executable that records its
# arguments and exits with a status the test chooses. It checks the commands the script runs, in
# order, and the exit status a scheduler would see. It needs no repository and no network.
#
#   sh contrib/test-scheduled.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
script=$here/vaultsnap-scheduled.sh
work=$(mktemp -d "${TMPDIR:-/tmp}/vaultsnap-scheduled-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

cat > "$work/vaultsnap" <<'STUB'
#!/bin/sh
echo "$*" >> "$STUB_LOG"
echo "$VAULTSNAP_REPO $VAULTSNAP_PASSWORD_FILE" > "$STUB_LOG.env"
for rule in ${STUB_STATUS:-}; do
    case $rule in
        "$1="*) exit "${rule#*=}" ;;
    esac
done
exit 0
STUB
chmod +x "$work/vaultsnap"

failures=0
code=0

# invoke NAME=VALUE...: runs the script with a clean environment plus the given assignments (an
# empty value counts as not set), and leaves its exit status in $code and its output in $work/err.
invoke() {
    : > "$work/log"
    code=0
    env -i PATH="$PATH" \
        VAULTSNAP_REPO=/repo VAULTSNAP_PASSWORD_FILE=/pw VAULTSNAP_PATHS=/data/a:/data/b \
        VAULTSNAP_BIN="$work/vaultsnap" STUB_LOG="$work/log" "$@" \
        sh "$script" 2> "$work/err" || code=$?
}

# expect NAME STATUS LOG: checks the exit status and the recorded commands.
expect() {
    name=$1
    want_code=$2
    want_log=$3
    got_log=$(cat "$work/log")
    if [ "$code" != "$want_code" ] || [ "$got_log" != "$want_log" ]; then
        failures=$((failures + 1))
        echo "FAIL: $name" >&2
        echo "  exit status: wanted $want_code, got $code" >&2
        echo "  wanted commands:" >&2
        echo "$want_log" | sed 's/^/    /' >&2
        echo "  got commands:" >&2
        echo "$got_log" | sed 's/^/    /' >&2
        sed 's/^/  stderr: /' "$work/err" >&2
    else
        echo "ok: $name"
    fi
}

invoke
expect "defaults" 0 "backup /data/a /data/b --tag scheduled
prune --yes --keep-daily 7 --keep-weekly 4 --keep-monthly 12
check"

if [ "$(cat "$work/log.env")" != "/repo /pw" ]; then
    failures=$((failures + 1))
    echo "FAIL: the repository and password file are not passed on" >&2
else
    echo "ok: the repository and password file are passed on"
fi

invoke VAULTSNAP_KEEP_LAST=3 VAULTSNAP_KEEP_DAILY= VAULTSNAP_KEEP_YEARLY=2
expect "custom retention" 0 "backup /data/a /data/b --tag scheduled
prune --yes --keep-last 3 --keep-weekly 4 --keep-monthly 12 --keep-yearly 2
check"

invoke VAULTSNAP_KEEP_DAILY= VAULTSNAP_KEEP_WEEKLY= VAULTSNAP_KEEP_MONTHLY=
expect "no retention rule: nothing is pruned" 0 "backup /data/a /data/b --tag scheduled
check"
grep -q 'nothing is pruned' "$work/err" || {
    failures=$((failures + 1))
    echo "FAIL: skipping the prune is not reported" >&2
}

invoke VAULTSNAP_PATHS=/only/one
expect "one path" 0 "backup /only/one --tag scheduled
prune --yes --keep-daily 7 --keep-weekly 4 --keep-monthly 12
check"

invoke "VAULTSNAP_PATHS=/with space/a:/b"
expect "paths with spaces" 0 "backup /with space/a /b --tag scheduled
prune --yes --keep-daily 7 --keep-weekly 4 --keep-monthly 12
check"

invoke STUB_STATUS=backup=3
expect "a backup with unreadable files still prunes and checks, and reports 3" 3 "backup /data/a /data/b --tag scheduled
prune --yes --keep-daily 7 --keep-weekly 4 --keep-monthly 12
check"

invoke STUB_STATUS=backup=1
expect "a failed backup stops everything" 1 "backup /data/a /data/b --tag scheduled"

invoke STUB_STATUS=prune=1
expect "a failed prune skips the check" 1 "backup /data/a /data/b --tag scheduled
prune --yes --keep-daily 7 --keep-weekly 4 --keep-monthly 12"

invoke STUB_STATUS=check=3
expect "a check that finds errors reports 3" 3 "backup /data/a /data/b --tag scheduled
prune --yes --keep-daily 7 --keep-weekly 4 --keep-monthly 12
check"

today=$(date +%u)
other=$((today % 7 + 1))
invoke VAULTSNAP_FULL_CHECK_DAY="$today"
expect "a full check on the chosen weekday" 0 "backup /data/a /data/b --tag scheduled
prune --yes --keep-daily 7 --keep-weekly 4 --keep-monthly 12
check --read-data"
invoke VAULTSNAP_FULL_CHECK_DAY="$other"
expect "a quick check on other days" 0 "backup /data/a /data/b --tag scheduled
prune --yes --keep-daily 7 --keep-weekly 4 --keep-monthly 12
check"

invoke VAULTSNAP_REPO=
expect "a missing repository is refused" 1 ""
grep -q 'VAULTSNAP_REPO' "$work/err" || {
    failures=$((failures + 1))
    echo "FAIL: the missing variable is not named" >&2
}
invoke VAULTSNAP_PASSWORD_FILE=
expect "a missing password file is refused" 1 ""
invoke VAULTSNAP_PATHS=:
expect "no directory to back up is refused" 1 ""

if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed" >&2
    exit 1
fi
echo "all checks passed"
