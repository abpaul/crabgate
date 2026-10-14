#!/bin/sh
set -eu

usage='Usage: restore-organizer.sh <Organizer UUID> <UTC timestamp RFC3339> [--apply]'
[ "$#" -ge 2 ] && [ "$#" -le 3 ] || { echo "$usage" >&2; exit 2; }
organizer_id=$1
timestamp=$2
mode=${3:---dry-run}
case "$organizer_id" in *[!a-f0-9-]* | '' | *..*) echo "Invalid Organizer ID." >&2; exit 2 ;; esac
case "$organizer_id" in ????????-????-????-????-????????????) ;; *) echo "Invalid Organizer ID." >&2; exit 2 ;; esac
compact_id=$(printf '%s' "$organizer_id" | tr -d '-')
[ "${#compact_id}" -eq 32 ] || { echo "Invalid Organizer ID." >&2; exit 2; }
case "$timestamp" in ????-??-??T??:??:??Z) ;; *) echo "Use a UTC timestamp such as 2026-09-30T10:00:00Z." >&2; exit 2 ;; esac
case "$mode" in --dry-run | --apply) ;; *) echo "$usage" >&2; exit 2 ;; esac

install_dir=${CRABGATE_INSTALL_DIR:-/opt/crabgate}
cd "$install_dir"
target="/data/organizers/$organizer_id.db"
restore="/data/organizers/$organizer_id.db.restore"

unusable() {
  echo "$1 Nothing was changed." >&2
  exit 1
}
if [ -f .deploy.env ]; then
  for file in .env .deploy.env compose.yaml compose.hosted.yaml; do
    [ -r "$file" ] || unusable "Deployed Compose context is incomplete: $install_dir/$file is not readable."
  done
  deployed_image=$(sed -n 's/^CRABGATE_IMAGE=//p' .deploy.env)
  [ -n "$deployed_image" ] || unusable "$install_dir/.deploy.env does not name CRABGATE_IMAGE."
  compose() {
    docker compose --env-file .env --env-file .deploy.env -f compose.yaml -f compose.hosted.yaml "$@"
  }
  echo "Compose context: deployed image $deployed_image with .env, .deploy.env, compose.yaml and compose.hosted.yaml in $install_dir." >&2
else
  deployed_image=""
  compose() {
    docker compose "$@"
  }
  echo "Compose context: Docker Compose defaults in $install_dir (no .deploy.env)." >&2
fi
images=$(compose config --images app litestream) || unusable "Docker Compose cannot resolve this configuration."
[ -n "$images" ] || unusable "Docker Compose resolved no app or litestream image."
if [ -n "$deployed_image" ]; then
  printf '%s\n' "$images" | grep -Fxq -- "$deployed_image" || unusable "The Compose configuration does not run the deployed image $deployed_image."
fi
for image in $images; do
  docker image inspect "$image" >/dev/null || unusable "Image $image is not available to Docker on this server."
done

if [ "$mode" = --dry-run ]; then
  compose run -T --rm --no-deps -e ORGANIZER_ID="$organizer_id" litestream restore -config /etc/litestream-restore.yml -dry-run -timestamp "$timestamp" -o "$restore" "$target"
  exit 0
fi

printf 'Restore Organizer %s to %s? Type RESTORE: ' "$organizer_id" "$timestamp" > /dev/tty
IFS= read -r answer < /dev/tty
[ "$answer" = RESTORE ] || { echo "Cancelled." >&2; exit 1; }

backup="/data/organizers/$organizer_id.db.pre-restore-$(date -u +%Y%m%dT%H%M%SZ)"
work="$target.restore-work"
checks='
set -eu
target=$1; backup=$2; restore=$3; work=$4
parent=${target%/*}; data=${parent%/*}
fail() { echo "$*" >&2; exit 1; }
regular() {
  test ! -L "$1" && test -f "$1" || fail "Unsafe file: $1"
  test "$(stat -c %h "$1")" = 1 || fail "Linked file: $1"
}
absent() { test ! -e "$1" && test ! -L "$1" || fail "Conflicting path: $1"; }
parents() {
  test ! -L "$data" && test ! -L "$parent" || fail "Symlinked data parent"
  test "$(cd "$parent" && pwd -P)" = "$parent" || fail "Unexpected data parent"
}
identity() {
  parents
  test ! -L "$work" && test -d "$work" || fail "Unsafe restore workspace"
  regular "$work/parent-identity"
  test "$(stat -c %d:%i "$parent")" = "$(cat "$work/parent-identity")" || fail "Data parent identity changed"
}
stage() { printf "%s\n" "$1" > "$work/stage"; echo "Restore stage: $1" >&2; }
'
phase=preparation
failure() {
  echo "Restore failed during $phase. Services must remain stopped. Do not restart or rerun until recovery is verified." >&2
  echo "Inspect $work/stage and $work/recovery-complete. Candidate recovery set: $backup (with matching -wal/-shm); complete only with its recovery-complete marker, or after successful workspace cleanup. See the deployment runbook for stage-specific recovery." >&2
}
if ! compose stop app litestream; then
  echo "Could not confirm services stopped. Stop all writers before recovery." >&2
  exit 1
fi
trap failure 0
trap 'exit 1' HUP INT TERM

compose run -T --rm --no-deps --user 0 --entrypoint sh app -c "$checks
parents
regular \"\$target\"
for suffix in -wal -shm; do
  if test -e \"\$target\$suffix\" || test -L \"\$target\$suffix\"; then regular \"\$target\$suffix\"; fi
done
for path in \"\$restore\" \"\$restore-wal\" \"\$restore-shm\" \"\$work\" \"\$backup\" \"\$backup-wal\" \"\$backup-shm\"; do absent \"\$path\"; done
mkdir -m 700 \"\$work\"
stat -c %d:%i \"\$parent\" > \"\$work/parent-identity\"
stage copying-recovery
cp -p \"\$target\" \"\$backup\"
for suffix in -wal -shm; do if test -f \"\$target\$suffix\"; then cp -p \"\$target\$suffix\" \"\$backup\$suffix\"; fi; done
printf complete > \"\$work/recovery-complete\"
stage recovery-ready
" _ "$target" "$backup" "$restore" "$work"
phase=remote-restore
compose run -T --rm --no-deps -e ORGANIZER_ID="$organizer_id" litestream restore -config /etc/litestream-restore.yml -integrity-check full -timestamp "$timestamp" -o "$restore" "$target"
phase=installation
compose run -T --rm --no-deps --user 0 --entrypoint sh app -c "$checks
identity
regular \"\$target\"; regular \"\$restore\"; regular \"\$backup\"
regular \"\$work/recovery-complete\"; regular \"\$work/stage\"
for suffix in -wal -shm; do
  if test -e \"\$backup\$suffix\" || test -L \"\$backup\$suffix\"; then regular \"\$backup\$suffix\"; fi
  absent \"\$restore\$suffix\"
  absent \"\$work/retired\$suffix\"
  if test -e \"\$target\$suffix\" || test -L \"\$target\$suffix\"; then regular \"\$target\$suffix\"; fi
done
chown --reference \"\$target\" \"\$restore\"
chmod --reference \"\$target\" \"\$restore\"
stage detaching-sidecars
for suffix in -wal -shm; do if test -f \"\$target\$suffix\"; then mv \"\$target\$suffix\" \"\$work/retired\$suffix\"; fi; done
stage installing
identity
mv \"\$restore\" \"\$target\"
stage installed
rm -f \"\$work/retired-wal\" \"\$work/retired-shm\"
stage cleanup-complete
rm \"\$work/parent-identity\" \"\$work/recovery-complete\" \"\$work/stage\"
rmdir \"\$work\"
" _ "$target" "$backup" "$restore" "$work"
phase=service-start
if ! compose start app litestream; then
  if ! compose stop app litestream; then
    echo "Could not confirm services stopped after failed start. Stop all writers before recovery." >&2
  fi
  exit 1
fi
trap - 0 HUP INT TERM
printf 'Restored %s. Complete previous recovery set: %s\n' "$target" "$backup"
