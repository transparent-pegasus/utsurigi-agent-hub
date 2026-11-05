#!/bin/sh
set -eu
umask 077
directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mode=$(cat "$directory/mode")
# The budget acceptance case uses only synthetic R2 URLs and maps every restic
# operation back to genuine local storage. This wrapper never contacts R2.
if [ -f "$directory/repo-shared" ]; then
    shared_url=s3:https://ownerless-acceptance-42.r2.cloudflarestorage.com/budget/shared
    bot_url=s3:https://ownerless-acceptance-42.r2.cloudflarestorage.com/budget/alpha
    case "${RESTIC_REPOSITORY-}" in
        "$shared_url") RESTIC_REPOSITORY=$(cat "$directory/repo-shared"); export RESTIC_REPOSITORY ;;
        "$bot_url") RESTIC_REPOSITORY=$(cat "$directory/repo-bot"); export RESTIC_REPOSITORY ;;
    esac
    original_count=$#
    for argument do
        case "$argument" in
            "$shared_url") argument=$(cat "$directory/repo-shared") ;;
            "$bot_url") argument=$(cat "$directory/repo-bot") ;;
            "--repo=$shared_url") argument="--repo=$(cat "$directory/repo-shared")" ;;
            "--repo=$bot_url") argument="--repo=$(cat "$directory/repo-bot")" ;;
            s3:*) printf '%s\n' 'unmapped synthetic storage URL refused' >&2; exit 97 ;;
        esac
        set -- "$@" "$argument"
    done
    shift "$original_count"
fi
operation=other
fence=false
for argument do
    case "$argument" in
        --stdin-from-command) fence=true ;;
        version|--version|-V) operation=version ;;
        init|backup|restore|snapshots|forget|prune|unlock|check|cat|ls) operation=$argument ;;
    esac
done
printf '%s\n' "$operation" >> "$directory/calls"
wait_for_release() {
    : > "$directory/ready"
    count=0
    while [ ! -f "$directory/release" ]; do
        count=$((count + 1))
        if [ "$count" -ge 3000 ]; then
            printf '%s\n' 'test barrier exceeded 30 seconds' >&2
            exit 99
        fi
        sleep 0.01
    done
}
case "$mode:$operation" in
    host-xattrs:restore|content-failure:restore)
        verify=false
        narrow=false
        previous=
        for argument do
            if [ "$previous" = --exclude-xattr ]; then
                if [ "$argument" != 'user.hatch_tainted*' ]; then
                    printf '%s\n' 'fixture: unrelated xattrs must remain protected' >&2
                    exit 98
                fi
                narrow=true
            fi
            case "$argument" in
                --verify) verify=true ;;
                --exclude-xattr=user.hatch_tainted\*) narrow=true ;;
                --exclude-xattr=*)
                    printf '%s\n' 'fixture: unrelated xattrs must remain protected' >&2
                    exit 98 ;;
                --include-xattr|--include-xattr=*)
                    printf '%s\n' 'fixture: an xattr allowlist would hide unrelated attributes' >&2
                    exit 98 ;;
            esac
            previous=$argument
        done
        if [ "$verify" != true ]; then
            printf '%s\n' 'fixture: restore must retain --verify' >&2
            exit 98
        fi
        if [ "$mode" = content-failure ]; then
            "$directory/real-restic" "$@"
            printf '%s\n' 'synthetic restored content verification failure' >&2
            exit 1
        fi
        if [ "$narrow" != true ]; then
            printf '%s\n' 'cannot remove enforced user.hatch_tainted.test: operation not permitted' >&2
            exit 1
        fi
        : > "$directory/host-xattrs-checked"
        ;;
    swap-config-before-fence:backup)
        if [ "$fence" = true ]; then
            cp "$directory/replacement-config" "$(cat "$directory/config-path")"
            : > "$directory/config-swapped"
        fi
        ;;
    barrier-backup:backup)
        if mkdir "$directory/first-backup" 2>/dev/null; then wait_for_release; fi
        ;;
    durable-fence:backup)
        if [ "$fence" = true ]; then
            "$directory/real-restic" "$@" > "$directory/fence-result"
            wait_for_release
            cat "$directory/fence-result"
            exit 0
        fi
        ;;
    fail-snapshots:snapshots)
        printf '%s\n' "ownerless-raw-stderr-sentinel ${RESTIC_PASSWORD-}" >&2
        exit 12
        ;;
    offline:version) ;;
    offline:*) printf '%s\n' 'status attempted forbidden storage access' >&2; exit 98 ;;
esac
exec "$directory/real-restic" "$@"
