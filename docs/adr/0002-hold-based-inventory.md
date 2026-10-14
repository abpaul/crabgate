# Capacity is reserved by a Hold at checkout start, not at payment

When a Buyer starts checkout, CrabGate reserves capacity with a Hold in a single database write transaction, and only then creates the Stripe Checkout Session. The Hold lives 30 minutes, which is the minimum Stripe Checkout Session lifetime. When the Buyer abandons checkout, the session is expired early. Payment turns the Hold into Tickets. We chose this so that concurrent Buyers can never pay for the same last place. Counting capacity only at payment would force automatic refunds whenever two Buyers raced for the last Ticket.

## Consequences

- A popular Event can look sold out while Holds are open, then free up again when they expire.
- Stripe webhooks are processed once per Checkout Session ID, so retries and out-of-order delivery cannot create duplicate Tickets.
- A Late Payment (confirmed after its Hold expired) becomes Tickets if capacity is still free. Otherwise it is refunded automatically and the Buyer is emailed.
