#!/bin/sh
set +x
set -eu

[ "$#" -eq 3 ] || { echo "usage: tenant-smoke.sh <tenant-origin> <organizer-uuid> <cookie-file>" >&2; exit 2; }
tenant_origin=$1
organizer_id=$2
cookies=$3
case "$organizer_id" in *[!a-fA-F0-9-]*|'') echo "Invalid Organizer UUID" >&2; exit 2;; esac
printf '%s\n' "$organizer_id" | LC_ALL=C grep -Eq '^[a-fA-F0-9]{8}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{12}$' || { echo "Invalid Organizer UUID" >&2; exit 2; }
[ -r "$cookies" ] || { echo "Operator cookie file is not readable" >&2; exit 2; }
case "$tenant_origin" in *[!\ -~]*) echo "Invalid tenant origin" >&2; exit 2;; esac
printf '%s\n' "$tenant_origin" | LC_ALL=C grep -Eq '^(https://([A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?|\[[A-Fa-f0-9:]+\])(:[0-9]+)?|http://(localhost|127\.0\.0\.1):[0-9]+)$' || { echo "Use an authority-only HTTPS or loopback test origin (no path, query, fragment, or userinfo)" >&2; exit 2; }

newline='
'
response=$(curl -q --silent --show-error --max-time 30 --max-filesize 65536 --write-out '\n%{http_code}' --cookie "$cookies" -- "$tenant_origin/operator/organizers/$organizer_id/readiness") || { echo "Organizer $organizer_id readiness request failed" >&2; exit 1; }
status=${response##*"$newline"}
[ "$status" = 200 ] || { echo "Organizer $organizer_id is not ready (HTTP $status); inspect migration logs and the operator suspension" >&2; exit 1; }
body=${response%"$newline"*}
printf '%s\n' "$body" | LC_ALL=C awk -v id="$organizer_id" '
  { body = body $0 " " }
  END {
    if (body !~ /^[[:space:]]*\{/ || body !~ /\}[[:space:]]*$/) exit 1
    sub(/^[[:space:]]*\{/, "", body)
    sub(/\}[[:space:]]*$/, "", body)
    if (split(body, fields, ",") != 3) exit 1
    for (i = 1; i <= 3; i++) {
      if (fields[i] ~ "^[[:space:]]*\"organizer_id\"[[:space:]]*:[[:space:]]*\"" id "\"[[:space:]]*$") organizer++
      else if (fields[i] ~ /^[[:space:]]*"ready"[[:space:]]*:[[:space:]]*true[[:space:]]*$/) ready++
      else if (fields[i] ~ /^[[:space:]]*"latest_migration"[[:space:]]*:[[:space:]]*"[A-Za-z0-9_]+"[[:space:]]*$/) migration++
      else exit 1
    }
    if (organizer != 1 || ready != 1 || migration != 1) exit 1
  }
' || { echo "Organizer $organizer_id readiness response is invalid or does not certify the requested target" >&2; exit 1; }
status=$(curl -q --silent --show-error --max-time 30 --output /dev/null --write-out '%{http_code}' -- "$tenant_origin/") || { echo "Organizer $organizer_id tenant route request failed" >&2; exit 1; }
[ "$status" = 200 ] || { echo "Organizer $organizer_id tenant route failed (HTTP $status)" >&2; exit 1; }
echo "Organizer $organizer_id readiness and tenant route passed"
