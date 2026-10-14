# CrabGate

CrabGate is a server-rendered ticket Shop for small rooms, independent conferences, and repeating workshops. An Organizer publishes Events, sells Tickets through its own Stripe account, checks Attendees in at the Door, and exports its data. This repository holds the source of the hosted service at [crabgate.com](https://crabgate.com). One deployment serves many Organizers, each at its own subdomain.

The code is licensed under [FSL-1.1-Apache-2.0](LICENSE).

| Read | For |
|---|---|
| `CONTEXT.md` | The terms |
| `CONTRIBUTING.md` | How a change gets into this repository |
| `DESIGN.md` | The visual system |
| `docs/adr/` | The reasons for the decisions that are hard to change |
| `docs/decisions-log.md` | Smaller decisions and the dependencies they brought |
| `docs/performance.md` | The load test and its measurements |
| `docs/blockers.md` | What still needs verification against real external services |

## Local development

Requires Rust 1.98, Docker Compose, and OpenSSL. Create `.env` once:

```sh
if [ ! -f .env ]; then
  umask 077
  printf 'CRABGATE_MASTER_KEY=%s\nCRABGATE_EDITION=hosted\nSHOP_BASE_DOMAIN=localhost\nSHOP_SCHEME=http\nPORT=5150\nSHOP_PORT=5150\n' "$(openssl rand -base64 32)" > .env
fi
```

Then start Mailpit and the app:

```sh
./scripts/dev.sh
```

Open `http://localhost:5150/signup` to name your Organizer and receive the first sign-in link; open `http://localhost:8025` for Mailpit. After verification, use the Organizer subdomain shown in the signup flow; Member sign-in is at `http://<subdomain>.localhost:5150/office/sign-in`. Keep the same `.env` so the master key can decrypt existing data. The local control database is `crabgate_control_development.sqlite`; Organizer databases are under `data/organizers/`.

The marketing pages are at `http://localhost:5150/` and each Organizer's Shop is at `http://<subdomain>.localhost:5150/`. Keep `PORT` and `SHOP_PORT` equal if you change the port. Any other host shows "Page not found".

## Settings

Production needs `DATABASE_URL`, `HOST`, `CRABGATE_MASTER_KEY`, and the `MAILER_*` values. `config/production.yaml` takes no default for a secret or an address, so a missing one stops the boot.

| Setting | Purpose |
|---|---|
| `SHOP_BASE_DOMAIN` | Base domain for marketing, signup, and Organizer subdomains |
| `CRABGATE_ORGANIZER_DATABASE_DIR` | Persistent directory for one SQLite file per Organizer |
| `CRABGATE_BILLING_SECRET_KEY` | The Stripe Billing secret key of the service |
| `CRABGATE_BILLING_PRICE_ID` | Stripe Price ID of the yearly Subscription |
| `CRABGATE_BILLING_WEBHOOK_SECRET` | Signing secret for `/webhooks/billing` |
| `CRABGATE_OPERATOR_EMAILS` | Comma-separated Operator sign-in addresses |
| `CRABGATE_MAIL_FROM` | Sender of every email. The address must be a verified sender at the mail provider |
| `MAILER_TLS` | `starttls` (default, port 587) or `implicit` (port 465) |

`compose.yaml` with `compose.hosted.yaml` runs the image behind Caddy with `deploy/Caddyfile.hosted`. Check the merged configuration with `docker compose -f compose.yaml -f compose.hosted.yaml config` before starting it.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings

test_root=$(mktemp -d)
mkdir "$test_root/tenants" "$test_root/evidence"
CRABGATE_MASTER_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA= \
DATABASE_URL="sqlite://$test_root/control.db?mode=rwc" \
CRABGATE_ORGANIZER_DATABASE_DIR="$test_root/tenants" \
CRABGATE_TEST_EVIDENCE_DIR="$test_root/evidence" \
CG032_TEST_ROOT="$test_root/evidence" \
  cargo test --locked --no-fail-fast

python3 -m unittest discover -s tests -p test_ci_tests.py
python3 -m unittest discover -s tests -p test_tenant_smoke.py
python3 -m unittest discover -s tests -p test_restore_organizer.py
node tests/free_tool_math.cjs
```

The Rust tests need a master key and their own storage; `cargo test` without these variables fails. The key above is 32 zero bytes and is for synthetic test data only. The tests need no credentials: Stripe and email are local fakes. `.github/workflows/ci.yml` runs the same checks and uses `python3 scripts/ci-test.py --workers 4` to run each Rust test in separate storage.

## Layout

| Path | Content |
|---|---|
| `src/` | Application code |
| `migration/` | Database migrations |
| `assets/views/` | HTML templates |
| `assets/static/` | CSS, scripts, fonts, and images, served at `/static` |
| `config/` | Loco configuration for each environment |
| `deploy/` | Caddy and Litestream configuration for the Compose files |
| `scripts/` | Development, test runner, smoke test, restore, and load test scripts |
| `tests/` | Request, model, and script tests |
| `docs/` | Decisions, performance, and open verification |
