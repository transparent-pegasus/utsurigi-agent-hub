#!/bin/sh
set -eu
umask 077
fixture_dir=${0%/*}
destination=
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o|--output) destination=$2; shift 2 ;;
        --output=*) destination=${1#*=}; shift ;;
        https://*) url=$1; shift ;;
        *) shift ;;
    esac
done
printf '%s\n' "$url" >> "$fixture_dir/requests"
release=https://github.com/restic/restic/releases/download/v0.19.1
case "$url" in
    "$release/SHA256SUMS") kind=checksums ;;
    "$release/$(cat "$fixture_dir/asset-name")") kind=archive ;;
    *) printf '%s\n' 'unexpected download URL; network is disabled' >&2; exit 64 ;;
esac
source=$fixture_dir/$kind
status=0
mode=$(cat "$fixture_dir/mode")
case "$mode:$kind" in
    checksum-failure:checksums) source=$fixture_dir/partial; status=22 ;;
    archive-failure:archive) source=$fixture_dir/partial; status=18 ;;
esac
if [ -n "$destination" ]; then
    cat "$source" > "$destination"
else
    cat "$source"
fi
exit "$status"
