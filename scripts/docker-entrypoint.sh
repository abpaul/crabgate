#!/bin/sh
set -eu

mkdir -p /data/organizers
chown -R crabgate:crabgate /data
exec gosu crabgate "$@"
