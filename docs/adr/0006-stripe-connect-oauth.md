---
status: accepted
---

# Organizers connect their existing Stripe account through Connect OAuth

Organizers paste a Stripe secret key and set up a webhook by hand, and the webhook step is the hardest part of setup. We will let an Admin press **Connect your Stripe account** instead. CrabGate's Stripe account acts as a Connect platform, uses Connect OAuth with `read_write` scope to link the Organizer's own Stripe account (or a new one created in the same flow), and stores only the returned account ID. Checkout Sessions and Refunds are made on the Organizer's account with the `Stripe-Account` header and no application fee, so ticket money and Stripe's fee stay with the Organizer as today. One platform webhook endpoint for connected accounts replaces each Organizer's own webhook; events are routed by their top-level `account` ID.

We chose OAuth because most Organizers we sell to already have a Stripe account. Stripe documents OAuth as the way to link an existing account, and states that OAuth requires the Accounts v1 API.

## Considered Options

- Keep pasted keys and per-Organizer webhooks: works today, but the webhook step is where first-time setup fails, and CrabGate holds a full secret key for every Organizer.
- Accounts v2 with Account Links: Stripe's recommended route for new platforms, but it creates accounts. Its documentation describes no way to link an existing account, and it lists OAuth under "You must use Accounts v1". The examples use a preview API version.
- Standard accounts through Account Links (Accounts v1): marked legacy by Stripe, and it also creates a new account rather than linking the existing one.

## Consequences

- The CrabGate Stripe account must exist, be activated, and have Connect enabled with its OAuth settings (client ID, redirect URIs for each Shop host or one fixed host) before this can be built against real Stripe.
- Stripe: "Platforms using OAuth with `read_write` scope won't be able to connect to accounts that are controlled by another platform." An Organizer whose Stripe account is controlled by another platform cannot connect it.
- Pasted keys stay supported until every live Organizer has connected; then the key and webhook steps and their stored secrets are removed.
- Disconnects arrive as `account.application.deauthorized`; the Shop must stop selling and the Back Office must ask the Admin to reconnect.

Sources, read 2026-10-03: [Connect OAuth reference](https://docs.stripe.com/connect/oauth-reference), [Accounts v2](https://docs.stripe.com/connect/accounts-v2), [Standard accounts](https://docs.stripe.com/connect/standard-accounts), [Connect webhooks](https://docs.stripe.com/connect/webhooks).

Accepted 2026-10-03. Build it after launch, once Connect is enabled on the CrabGate Stripe account. Until then Organizers paste a key, and the setup call covers the webhook step.
