#!/bin/sh
set -eu
umask 077

reject() {
  echo 'CrabGate writer admission rejected; explicit recovery is required' >&2
  sleeper=''
  trap 'if [ -n "$sleeper" ]; then kill "$sleeper" 2>/dev/null || :; wait "$sleeper" 2>/dev/null || :; fi; exit 0' TERM INT
  while :; do
    sleep 60 &
    sleeper=$!
    wait "$sleeper" || :
  done
}

[ "$#" -gt 0 ] || reject
mkdir /tmp/crabgate-writer-rejected 2>/dev/null || reject
exec 9>>/data/.crabgate-writer.lock || reject
flock --nonblock 9 || reject
rmdir /tmp/crabgate-writer-rejected || reject
exec "$@"
