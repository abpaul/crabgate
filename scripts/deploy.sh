#!/bin/sh
set -eu

usage='usage: deploy.sh <image> <tenant-origin> <organizer-uuid> <cookie-file>'
refuse() {
  echo "$1 Nothing was changed." >&2
  echo "$usage" >&2
  exit 2
}
[ "$#" -eq 4 ] || refuse "Tenant acceptance needs the image and all three tenant smoke arguments."
for argument in "$@"; do
  [ -n "$argument" ] || refuse "Empty argument."
done
image=$1
case "$image" in *[!!-~]*) refuse "The image reference holds a space, control character, or non-ASCII character." ;; esac
shift
cd "$(dirname "$0")/.."
[ -r scripts/tenant-smoke.sh ] || refuse "scripts/tenant-smoke.sh is missing."
[ -r "$3" ] || refuse "Operator cookie file is not readable."

compose() {
  docker compose --env-file .env --env-file .deploy.env -f compose.yaml -f compose.hosted.yaml "$@"
}

previous=""
if [ -f .deploy.env ]; then
  previous="$(sed -n 's/^CRABGATE_IMAGE=//p' .deploy.env)"
fi
if [ -z "$previous" ]; then
  recorded="No previous image is recorded."
elif [ "$previous" = "$image" ]; then
  recorded="$image was already the recorded image."
else
  recorded="Previously recorded image: $previous."
fi

use_image() {
  printf 'CRABGATE_IMAGE=%s\n' "$1" > .deploy.env
}

record_previous() {
  if [ -n "$previous" ]; then
    use_image "$previous"
  else
    rm -f .deploy.env
  fi
}

healthy() {
  attempts=0
  while [ "$attempts" -lt 60 ]; do
    status="$(docker inspect --format '{{.State.Health.Status}}' "$(compose ps -q app)" 2>/dev/null || true)"
    [ "$status" = "healthy" ] && return 0
    attempts=$((attempts + 1))
    sleep 3
  done
  return 1
}

use_image "$image"
if ! compose pull app; then
  record_previous
  echo "Could not pull $image. The running stack was not changed." >&2
  exit 1
fi

if compose up -d --no-build && healthy; then
  echo "Image health: passed. $image is running and its container health check passed. This does not verify Organizer migrations."
  if ! sh scripts/tenant-smoke.sh "$@"; then
    echo "Tenant acceptance: FAILED for Organizer $2 at $1. $image remains running and no image was rolled back, because starting an older image does not reverse database migrations. $recorded Follow the tenant recovery procedure of the deployment runbook, then repeat scripts/tenant-smoke.sh." >&2
    exit 1
  fi
  echo "Tenant acceptance: passed for Organizer $2 at $1. Other Organizers were not checked."
  echo "Deployed $image. $recorded"
  exit 0
fi

echo "Image health: FAILED. $image did not become healthy. Tenant acceptance was not run." >&2
compose logs --tail 80 app >&2 || true
if [ -z "$previous" ]; then
  rm -f .deploy.env
  echo "No previous image is recorded, so no rollback is possible." >&2
elif [ "$previous" = "$image" ]; then
  echo "$image was already the recorded image, so no rollback is possible." >&2
else
  echo "Starting the previous image again: $previous. Database migrations that $image applied are not reversed." >&2
  use_image "$previous"
  if compose up -d --no-build && healthy; then
    echo "Rollback image health: passed for $previous." >&2
  else
    echo "Rollback image health: FAILED for $previous. The service needs operator recovery." >&2
  fi
fi
exit 1
