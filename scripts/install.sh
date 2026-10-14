#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
  echo "Run the installer as root." >&2
  exit 1
fi

if ! command -v openssl >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1; then
  if ! command -v apt-get >/dev/null 2>&1; then
    echo "Install OpenSSL first; automatic package setup supports Debian and Ubuntu." >&2
    exit 1
  fi
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssl curl ca-certificates
fi

ask() {
  key=$1
  prompt=$2
  current=$(printenv "$key" 2>/dev/null || true)
  if [ -z "$current" ]; then
    printf '%s: ' "$prompt" > /dev/tty
    IFS= read -r current < /dev/tty
  fi
  [ -n "$current" ] || { echo "$prompt is required." >&2; exit 1; }
  printf '%s' "$current"
}

dotenv() {
  key=$1
  value=$2
  case "$value" in *'
'*) echo "$key contains a newline." >&2; exit 1 ;; esac
  escaped=$(printf '%s' "$value" | sed "s/'/\\\\'/g")
  printf "%s='%s'\n" "$key" "$escaped" >> "$env_file"
}

DOMAIN=$(ask DOMAIN "Public domain, for example tickets.example.com")
CRABGATE_ADMIN_EMAIL=$(ask CRABGATE_ADMIN_EMAIL "Admin email")
MAILER_HOST=$(ask MAILER_HOST "SMTP host")
MAILER_USER=$(ask MAILER_USER "SMTP username")
MAILER_PASSWORD=$(ask MAILER_PASSWORD "SMTP password")
BACKUP_ENDPOINT=$(ask BACKUP_ENDPOINT "S3-compatible backup endpoint, including https://")
BACKUP_REGION=$(ask BACKUP_REGION "Backup region")
BACKUP_BUCKET=$(ask BACKUP_BUCKET "Backup bucket")
BACKUP_ACCESS_KEY_ID=$(ask BACKUP_ACCESS_KEY_ID "Backup access key ID")
BACKUP_SECRET_ACCESS_KEY=$(ask BACKUP_SECRET_ACCESS_KEY "Backup secret access key")

case "$DOMAIN" in
  *[!a-zA-Z0-9.-]* | .* | *..* | *.) echo "Invalid domain." >&2; exit 1 ;;
esac
case "$CRABGATE_ADMIN_EMAIL" in *@*.*) ;; *) echo "Invalid Admin email." >&2; exit 1 ;; esac
case "$BACKUP_ENDPOINT" in https://* | http://*) ;; *) echo "Backup endpoint must start with https:// or http://." >&2; exit 1 ;; esac
case "$BACKUP_BUCKET" in *[!a-z0-9.-]* | '' | .* | *..* | *.) echo "Invalid backup bucket." >&2; exit 1 ;; esac

install_dir=${CRABGATE_INSTALL_DIR:-/opt/crabgate}
if [ -e "$install_dir/.env" ]; then
  echo "Install already exists at $install_dir. No files were changed." >&2
  exit 1
fi
mkdir -p "$install_dir"

if [ -n "${CRABGATE_SOURCE_DIR:-}" ]; then
  for item in Dockerfile .dockerignore compose.yaml Cargo.toml Cargo.lock migration src assets config deploy scripts; do
    cp -R "$CRABGATE_SOURCE_DIR/$item" "$install_dir/$item"
  done
else
  : "${CRABGATE_BUNDLE_URL:?Set CRABGATE_BUNDLE_URL to a CrabGate source archive URL}"
  curl -fsSL "$CRABGATE_BUNDLE_URL" | tar -xz -C "$install_dir" --strip-components=1
fi

master_key=$(openssl rand -base64 32)
env_file=$install_dir/.env
umask 077
: > "$env_file"
dotenv DOMAIN "$DOMAIN"
dotenv ACME_EMAIL "$CRABGATE_ADMIN_EMAIL"
dotenv CRABGATE_ADMIN_EMAIL "$CRABGATE_ADMIN_EMAIL"
dotenv CRABGATE_ORGANIZER_NAME "${CRABGATE_ORGANIZER_NAME:-My Organizer}"
dotenv CRABGATE_MASTER_KEY "$master_key"
dotenv MAILER_HOST "$MAILER_HOST"
dotenv MAILER_USER "$MAILER_USER"
dotenv MAILER_PASSWORD "$MAILER_PASSWORD"
dotenv BACKUP_ENDPOINT "$BACKUP_ENDPOINT"
dotenv BACKUP_REGION "$BACKUP_REGION"
dotenv BACKUP_BUCKET "$BACKUP_BUCKET"
dotenv BACKUP_ACCESS_KEY_ID "$BACKUP_ACCESS_KEY_ID"
dotenv BACKUP_SECRET_ACCESS_KEY "$BACKUP_SECRET_ACCESS_KEY"
chmod 600 "$env_file"

if [ "${CRABGATE_INSTALL_DRY_RUN:-0}" != 1 ]; then
  if ! command -v docker >/dev/null 2>&1; then
    curl -fsSL https://get.docker.com | sh
  fi
  docker compose version >/dev/null
  (cd "$install_dir" && docker compose up -d --build)
fi

printf '\nCrabGate files: %s\n' "$install_dir"
printf 'Open https://%s/signup and use %s to set up your Organizer.\n' "$DOMAIN" "$CRABGATE_ADMIN_EMAIL"
printf '\nMASTER KEY — SAVE THIS NOW, OFFLINE AND SEPARATE FROM BACKUPS:\n%s\n' "$master_key"
printf 'This key is printed only now. Losing it makes encrypted Organizer data unreadable.\n'
