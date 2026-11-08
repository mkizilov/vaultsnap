#!/bin/sh
# One scheduled VaultSnap run: back up, apply the retention policy, check the repository.
#
# It is meant for cron, a systemd timer, or launchd (docs/GUIDE.md). Everything comes from the
# environment, nothing from arguments, and the passphrase is only ever a file:
#
#   VAULTSNAP_REPO            repository: a directory, or s3://BUCKET[/FOLDER]        (required)
#   VAULTSNAP_PASSWORD_FILE   file holding the passphrase, mode 0600                   (required)
#   VAULTSNAP_PATHS           directories to back up, separated by colons              (required)
#   VAULTSNAP_BIN             the vaultsnap executable                      (default: vaultsnap)
#   VAULTSNAP_KEEP_LAST       retention: newest N snapshots              (default: not used)
#   VAULTSNAP_KEEP_DAILY      retention: one per day for N days                (default: 7)
#   VAULTSNAP_KEEP_WEEKLY     retention: one per week for N weeks              (default: 4)
#   VAULTSNAP_KEEP_MONTHLY    retention: one per month for N months           (default: 12)
#   VAULTSNAP_KEEP_YEARLY     retention: one per year for N years        (default: not used)
#   VAULTSNAP_FULL_CHECK_DAY  weekday 1-7 (Monday is 1) for a check that reads all data;
#                             on other days the check is the quick one     (default: never full)
#
# A retention variable that is set but empty switches that rule off. Directory names with a
# colon in them cannot be listed in VAULTSNAP_PATHS.
#
# Exit status: 0 everything worked; 3 the backup is missing some unreadable files or the check
# found errors (the snapshot exists, a person should look); anything else is a failure. After a
# failed backup nothing is pruned.
set -eu

for name in VAULTSNAP_REPO VAULTSNAP_PASSWORD_FILE VAULTSNAP_PATHS; do
    eval "value=\${$name:-}"
    if [ -z "$value" ]; then
        echo "vaultsnap-scheduled: $name is not set (see the comments at the top of this file)" >&2
        exit 1
    fi
done
vaultsnap=${VAULTSNAP_BIN:-vaultsnap}
export VAULTSNAP_REPO VAULTSNAP_PASSWORD_FILE

status=0

# Runs a command and remembers a "result has a problem" status (3) instead of stopping.
run() {
    code=0
    "$@" || code=$?
    case $code in
        0) ;;
        3) echo "vaultsnap-scheduled: '$2' finished with problems (status 3)" >&2; status=3 ;;
        *) echo "vaultsnap-scheduled: '$2' failed (status $code)" >&2; exit "$code" ;;
    esac
}

# --- back up ---------------------------------------------------------------------------------
old_ifs=$IFS
IFS=:
set -f
set --
for path in $VAULTSNAP_PATHS; do
    if [ -n "$path" ]; then
        set -- "$@" "$path"
    fi
done
set +f
IFS=$old_ifs
if [ "$#" -eq 0 ]; then
    echo "vaultsnap-scheduled: VAULTSNAP_PATHS names no directory" >&2
    exit 1
fi
run "$vaultsnap" backup "$@" --tag scheduled

# --- retention -------------------------------------------------------------------------------
set -- prune --yes
if [ -n "${VAULTSNAP_KEEP_LAST:-}" ]; then
    set -- "$@" --keep-last "$VAULTSNAP_KEEP_LAST"
fi
if [ -n "${VAULTSNAP_KEEP_DAILY-7}" ]; then
    set -- "$@" --keep-daily "${VAULTSNAP_KEEP_DAILY-7}"
fi
if [ -n "${VAULTSNAP_KEEP_WEEKLY-4}" ]; then
    set -- "$@" --keep-weekly "${VAULTSNAP_KEEP_WEEKLY-4}"
fi
if [ -n "${VAULTSNAP_KEEP_MONTHLY-12}" ]; then
    set -- "$@" --keep-monthly "${VAULTSNAP_KEEP_MONTHLY-12}"
fi
if [ -n "${VAULTSNAP_KEEP_YEARLY:-}" ]; then
    set -- "$@" --keep-yearly "$VAULTSNAP_KEEP_YEARLY"
fi
if [ "$#" -gt 2 ]; then
    run "$vaultsnap" "$@"
else
    echo "vaultsnap-scheduled: no retention rule is set, nothing is pruned" >&2
fi

# --- check -----------------------------------------------------------------------------------
if [ -n "${VAULTSNAP_FULL_CHECK_DAY:-}" ] && [ "$(date +%u)" = "$VAULTSNAP_FULL_CHECK_DAY" ]; then
    run "$vaultsnap" check --read-data
else
    run "$vaultsnap" check
fi

exit "$status"
