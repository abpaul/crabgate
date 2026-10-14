import hashlib
import hmac
import json
import os
import time
import urllib.parse
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


SESSIONS = {}
IDEMPOTENCY = {}
REFUNDS = []
WEBHOOK_ENDPOINTS = {}
REFUSE_WEBHOOK_ENDPOINTS = os.environ.get("STRIPE_FAKE_REFUSE_WEBHOOK_ENDPOINTS") == "1"
KEY =os.environ["STRIPE_TEST_SECRET_KEY"]
WEBHOOK_SECRET = os.environ["STRIPE_TEST_WEBHOOK_SECRET"]
WEBHOOK_URL = os.environ.get("STRIPE_FAKE_WEBHOOK_URL", "http://localhost:5150/webhooks/stripe")
FAKE_BASE_URL = os.environ.get("STRIPE_FAKE_BASE_URL", "http://127.0.0.1:5151")


class Handler(BaseHTTPRequestHandler):
    def respond(self, status, body, content_type="application/json", location=None):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(data)

    def authorized(self):
        import base64

        expected = "Basic " + base64.b64encode((KEY + ":").encode()).decode()
        return self.headers.get("Authorization") == expected

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        form = urllib.parse.parse_qs(self.rfile.read(length).decode())
        if self.path.startswith("/v1/") and not self.authorized():
            return self.respond(401, '{"error":"invalid key"}')
        if self.path == "/v1/checkout/sessions":
            key = self.headers.get("Idempotency-Key")
            if key in IDEMPOTENCY:
                session_id = IDEMPOTENCY[key]
                return self.respond(200, json.dumps({"id": session_id, "url": f"{FAKE_BASE_URL}/checkout/{session_id}", "expires_at": SESSIONS[session_id]["expires_at"]}))
            expires = int(form.get("expires_at", ["0"])[0])
            if expires < int(time.time()) + 1800 or expires > int(time.time()) + 86400:
                return self.respond(400, '{"error":"invalid expires_at"}')
            session_id = "cs_test_" + uuid.uuid4().hex
            mode = form.get("mode", ["payment"])[0]
            amount = 0
            for key, values in form.items():
                if key.endswith("[unit_amount]"):
                    index = key.split("[")[1].split("]")[0]
                    amount += int(values[0]) * int(form[f"line_items[{index}][quantity]"][0])
            SESSIONS[session_id] = {
                "mode": mode,
                "hold_id": form.get("metadata[hold_id]", [None])[0],
                "organizer_id": form.get("client_reference_id", [None])[0],
                "success_url": form["success_url"][0],
                "cancel_url": form["cancel_url"][0],
                "amount": amount,
                "currency": form.get("line_items[0][price_data][currency]", ["eur"])[0],
                "status": "open",
                "expires_at": expires,
            }
            if key:
                IDEMPOTENCY[key] = session_id
            return self.respond(200, json.dumps({"id": session_id, "url": f"{FAKE_BASE_URL}/checkout/{session_id}", "expires_at": expires}))
        if self.path.startswith("/v1/checkout/sessions/") and self.path.endswith("/expire"):
            session_id = self.path.split("/")[4]
            session = SESSIONS.get(session_id)
            if not session or session["status"] != "open":
                return self.respond(400, '{"error":"not open"}')
            session["status"] = "expired"
            return self.respond(200, json.dumps({"id": session_id, "status": "expired"}))
        if self.path == "/v1/webhook_endpoints":
            if REFUSE_WEBHOOK_ENDPOINTS:
                return self.respond(403, '{"error":{"message":"The provided key does not have the required permissions"}}')
            endpoint = {
                "id": "we_test_" + uuid.uuid4().hex,
                "object": "webhook_endpoint",
                "url": form.get("url", [""])[0],
                "enabled_events": form.get("enabled_events[]", []),
                "status": "enabled",
            }
            WEBHOOK_ENDPOINTS[endpoint["id"]] = endpoint
            return self.respond(200, json.dumps({**endpoint, "secret": WEBHOOK_SECRET}))
        if self.path == "/v1/refunds":
            REFUNDS.append(form.get("payment_intent", [""])[0])
            return self.respond(200, json.dumps({"id": "re_test_" + uuid.uuid4().hex, "status": "succeeded"}))
        if self.path.startswith("/fake-pay/"):
            session_id = self.path.rsplit("/", 1)[-1]
            session = SESSIONS.get(session_id)
            if not session or session["status"] != "open":
                return self.respond(400, "Session is not open", "text/plain")
            body = json.dumps({
                "id": "evt_test_" + uuid.uuid4().hex,
                "type": "checkout.session.completed",
                "created": int(time.time()),
                "data": {"object": {
                    "id": session_id,
                    "mode": session["mode"],
                    "payment_status": "paid",
                    "amount_total": session["amount"],
                    "currency": session["currency"],
                    "payment_intent": "pi_test_" + session_id,
                    "metadata": {"hold_id": session["hold_id"]},
                    "client_reference_id": session["organizer_id"],
                    "customer": "cus_test_" + session_id,
                    "subscription": "sub_test_" + session_id,
                }},
            }).encode()
            timestamp = str(int(time.time()))
            secret = os.environ.get("STRIPE_FAKE_BILLING_WEBHOOK_SECRET", WEBHOOK_SECRET) if session["mode"] == "subscription" else WEBHOOK_SECRET
            url = os.environ.get("STRIPE_FAKE_BILLING_WEBHOOK_URL", "http://localhost:5150/webhooks/billing") if session["mode"] == "subscription" else WEBHOOK_URL
            signature = hmac.new(secret.encode(), timestamp.encode() + b"." + body, hashlib.sha256).hexdigest()
            request = urllib.request.Request(url, body, {"Content-Type": "application/json", "Stripe-Signature": f"t={timestamp},v1={signature}"}, method="POST")
            try:
                with urllib.request.urlopen(request, timeout=10) as response:
                    if response.status != 200:
                        return self.respond(502, "Webhook failed", "text/plain")
            except Exception as error:
                return self.respond(502, str(error), "text/plain")
            session["status"] = "paid"
            return self.respond(303, "", location=session["success_url"].replace("{CHECKOUT_SESSION_ID}", session_id))
        return self.respond(404, "Not found", "text/plain")

    def do_DELETE(self):
        if not self.authorized():
            return self.respond(401, '{"error":"invalid key"}')
        if self.path.startswith("/v1/webhook_endpoints/"):
            endpoint_id = self.path.rsplit("/", 1)[-1]
            if WEBHOOK_ENDPOINTS.pop(endpoint_id, None) is None:
                return self.respond(404, '{"error":{"code":"resource_missing"}}')
            return self.respond(200, json.dumps({"id": endpoint_id, "deleted": True}))
        return self.respond(404, "Not found", "text/plain")

    def do_GET(self):
        if self.path.split("?")[0] == "/v1/webhook_endpoints":
            if not self.authorized():
                return self.respond(401, '{"error":"invalid key"}')
            return self.respond(200, json.dumps({"object": "list", "has_more": False, "data": list(WEBHOOK_ENDPOINTS.values())}))
        if self.path == "/v1/account":
            if not self.authorized():
                return self.respond(401, '{"error":"invalid key"}')
            return self.respond(200, '{"id":"acct_test_local","object":"account"}')
        if self.path.startswith("/checkout/"):
            session_id = self.path.rsplit("/", 1)[-1]
            session = SESSIONS.get(session_id)
            if not session:
                return self.respond(404, "Not found", "text/plain")
            html = f'<html><head><meta name="viewport" content="width=device-width,initial-scale=1"></head><body style="font:20px sans-serif;max-width:540px;margin:60px auto"><h1>Stripe test Checkout</h1><p>Pay €{session["amount"] / 100:.2f}</p><form method="post" action="/fake-pay/{session_id}"><button style="font:inherit;padding:16px 32px">Pay test amount</button></form><p><a href="{session["cancel_url"]}">Return without paying</a></p></body></html>'
            return self.respond(200, html, "text/html")
        if self.path == "/refunds":
            return self.respond(200, json.dumps(REFUNDS))
        return self.respond(404, "Not found", "text/plain")


ThreadingHTTPServer(("127.0.0.1", 5151), Handler).serve_forever()
