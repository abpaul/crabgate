# Decisions log

- The FSL-1.1-ALv2 template expressly permits professional services for a licensee. ADR 0003 describes a broader prohibition on paid setup by third parties. The license text controls what licensees may do; the product will use the unmodified FSL terms and will not claim the broader prohibition in user-facing copy.
- `aes-gcm` provides authenticated AES-256-GCM field encryption; `base64` stores encrypted fields in SQLite text columns; `zeroize` clears key buffers when they are dropped.
- `moka` evicts idle Organizer database connections while keeping repeated Host lookups inexpensive.
- `sha2` stores only hashes of magic-link and Member session tokens in the control database.
- `chrono-tz` converts Event and Sales Window times in the Organizer's IANA time zone; `percent-encoding` safely builds OpenStreetMap search links.
- `reqwest` sends Stripe's form-encoded API requests without a large Stripe SDK; `hmac` and `hex` verify webhook signatures; `qrcode` renders Ticket codes as SVG.
- Stripe's 30-minute minimum Session lifetime starts when Stripe creates the Session. CrabGate creates the Session immediately after its 30-minute Hold, then gives the Session a 31-minute expiry. A payment in the final minute enters the Late Payment path, which checks capacity before issuing Tickets.
- `jsQR` v1.4.0 is vendored with its Apache-2.0 license for Safari camera scanning when `BarcodeDetector` is unavailable; Door loads it from the same origin.
- Full data export writes a SQLite snapshot in a private temporary directory, decrypts its protected fields, and streams the file to the Admin.
- Saving a Stripe key creates the Organizer's webhook endpoint through the Stripe API (`POST /v1/webhook_endpoints`) at the Organizer's own `/webhooks/stripe` address and stores the returned signing secret and endpoint id. Stripe accepting the endpoint completes the Setup webhook step and sets `webhook_tested_at`; reachability is not proven until the first signed event arrives. A pasted signing secret still waits for a correctly signed Stripe event before the step is complete. After creating the endpoint, CrabGate deletes every other endpoint at the same address in that Stripe account, so a second key save leaves exactly one. If Stripe refuses creation, the key is saved, the existing secret is untouched, and Setup shows the manual step with the reason.
