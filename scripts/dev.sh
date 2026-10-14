#!/bin/sh
set -eu

cd "$(dirname "$0")/.."

if [ ! -f .env ]; then
  echo "Missing .env. Follow the one-time setup in README.md before running this script." >&2
  exit 1
fi

docker compose -f compose.dev.yaml up -d
set -a
. ./.env
set +a
exec cargo loco start
