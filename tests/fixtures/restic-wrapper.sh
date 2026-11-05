#!/bin/sh
set -eu
umask 077
fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mode=$(cat "$fixture_dir/mode")
# Default ACLs can override umask; restrict the log before recording secrets.
: >> "$fixture_dir/boundary"
chmod 600 "$fixture_dir/boundary"
{
    printf '%s\n' '__CALL__'
    for argument do printf 'arg:%s\n' "$argument"; done
    env
    printf '%s\n' '__END__'
} >> "$fixture_dir/boundary"
case "${RESTIC_REPOSITORY-}" in
    s3:https://compat-acceptance-42.r2.cloudflarestorage.com/fake/shared) RESTIC_REPOSITORY=$(cat "$fixture_dir/repo-shared"); export RESTIC_REPOSITORY ;;
    s3:https://compat-acceptance-42.r2.cloudflarestorage.com/fake/alpha) RESTIC_REPOSITORY=$(cat "$fixture_dir/repo-bot"); export RESTIC_REPOSITORY ;;
    s3:*) printf '%s\n' 'unmapped synthetic repository refused' >&2; exit 97 ;;
esac
original_count=$#
for argument do
    case "$argument" in
        s3:https://compat-acceptance-42.r2.cloudflarestorage.com/fake/shared) argument=$(cat "$fixture_dir/repo-shared") ;;
        s3:https://compat-acceptance-42.r2.cloudflarestorage.com/fake/alpha) argument=$(cat "$fixture_dir/repo-bot") ;;
        --repo=s3:https://compat-acceptance-42.r2.cloudflarestorage.com/fake/shared) argument="--repo=$(cat "$fixture_dir/repo-shared")" ;;
        --repo=s3:https://compat-acceptance-42.r2.cloudflarestorage.com/fake/alpha) argument="--repo=$(cat "$fixture_dir/repo-bot")" ;;
        s3:*|--repo=s3:*) printf '%s\n' 'unmapped synthetic repository refused' >&2; exit 97 ;;
    esac
    set -- "$@" "$argument"
done
shift "$original_count"
operation=other
for argument do
    case "$argument" in
        version|--version|-V) operation=version; break ;;
        init|backup|restore|snapshots|forget|prune|unlock|check|cat|ls)
            operation=$argument
            break
            ;;
    esac
done
printf '%s\n' "$operation" >> "$fixture_dir/calls"
case "$mode:$operation" in
    offline:version) ;;
    offline:*) printf '%s\n' 'synthetic network failure' >&2; exit 1 ;;
    network:backup)
        printf '%s\n' 'synthetic network failure synthetic-ALPHA-secret-9c2d synthetic-SHARED-secret-7f0b' >&2
        exit 1
        ;;
    auth:backup) printf '%s\n' 'Fatal: wrong password or no key found' >&2; exit 1 ;;
    partial:backup)
        "$fixture_dir/real-restic" "$@"
        exit 3
        ;;
    ambiguous:backup)
        "$fixture_dir/real-restic" "$@"
        printf '%s\n' 'synthetic connection lost after upload' >&2
        exit 1
        ;;
    restore-failure:restore)
        # Optional exact target keeps control validation real before the data fault.
        if [ -f "$fixture_dir/restore-failure-snapshot" ]; then
            snapshot=
            previous=
            for argument do
                if [ "$previous" = restore ]; then snapshot=$argument; break; fi
                previous=$argument
            done
            if [ "$snapshot" != "$(cat "$fixture_dir/restore-failure-snapshot")" ]; then
                exec "$fixture_dir/real-restic" "$@"
            fi
        fi
        "$fixture_dir/real-restic" "$@"
        printf '%s\n' 'synthetic interrupted restore' >&2
        exit 1
        ;;
esac
# Record TLS filename delivery without opening certificate files in local storage tests.
if [ "$mode" = tls-boundary ]; then unset RESTIC_CACERT RESTIC_TLS_CLIENT_CERT; fi
exec "$fixture_dir/real-restic" "$@"
