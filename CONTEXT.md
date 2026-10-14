# CrabGate

A ticket shop operated by CrabGate for Organizers. Organizers publish Events, sell Tickets through their own payment account, check guests in at the door, and export their data.

## Language

### Service

**CrabGate**:
The hosted ticketing service operated by the CrabGate team for Organizers.
_Avoid_: Edition, platform

**Subscription**:
An Organizer's flat yearly payment for CrabGate. It includes one onboarding call, in which the CrabGate team guides the Organizer through setup.
_Avoid_: Plan, license, membership

**Grace Period**:
The 14 days after a Subscription payment fails, during which the shop keeps working fully.
_Avoid_: Dunning period, trial

**Lapsed Organizer**:
An Organizer whose Grace Period has ended. It cannot sell or publish, but it can still Check in and export.
_Avoid_: Suspended, expired, inactive

**Suspension**:
An Operator's action that takes an Organizer's shop offline, for abuse or a failed migration.
_Avoid_: Ban, block, lapse

### Surfaces

**Shop**:
The Buyer-facing part of an Organizer's CrabGate: Event pages, checkout, Order pages, and Ticket emails. Every Shop has CrabGate's look and names its Organizer; Organizers cannot restyle it.
_Avoid_: Storefront, box office, site, public pages

**Back Office**:
The part of CrabGate where Admins manage Events, Orders, Members, and settings. It carries CrabGate's own brand.
_Avoid_: Dashboard, admin panel, backend

**Door**:
The part of CrabGate used at the entrance to check in Attendees. It shows the Organizer's name in CrabGate's fixed high-contrast style.
_Avoid_: Scanner app, check-in app, gate

### Organizers and people

**Organizer**:
The party that owns a ticket shop: its payment account, its Events, and its data.
_Avoid_: Tenant, venue, account, customer, client

**Member**:
A person with a login who belongs to one or more Organizers, with a role in each. The role is Admin or Door Staff.
_Avoid_: User, staff, account

**Admin**:
A Member with full control of an Organizer's shop.
_Avoid_: Owner, manager, superuser

**Door Staff**:
A Member of an Organizer who can only check in Attendees.
_Avoid_: Volunteer, scanner, gatekeeper

**Operator**:
A member of the CrabGate team who manages Organizers in CrabGate. An Operator sees an Organizer's data only under an active Access Grant.
_Avoid_: Support agent, staff, superadmin

**Access Grant**:
An Admin's revocable, time-limited consent for Operators to read that Organizer's Back Office data. It excludes changes to the shop, full data exports, and payment credentials.
_Avoid_: Impersonation permission, support access, consent

**Buyer**:
The person who pays for an Order, identified only by an email address. Buyers have no accounts.
_Avoid_: Customer, user, purchaser

**Attendee**:
The person a Ticket admits. The Attendee can be the Buyer or someone else.
_Avoid_: Guest, participant, visitor

### Selling

**Event**:
A single occasion at one date and time that an Organizer sells Tickets for.
_Avoid_: Show, occurrence, listing

**Series**:
A repeating schedule of independent Events, such as a workshop every Thursday at 20:00. Each date has its own Orders, capacity, and cancellation; a Series is not a course pass.
_Avoid_: Recurring event, repeating event, template

**Draft Event**:
An Event that only Members can see. An Event can return to Draft only until its first sale.
_Avoid_: Unpublished, hidden

**Published Event**:
An Event that Buyers can see on the Organizer's shop.
_Avoid_: Live, public, active

**Past Event**:
A Published Event whose date and time have passed.
_Avoid_: Archived, finished, closed

**Cancelled Event**:
An Event called off by an Admin, with sales stopped immediately. Refunds and Buyer notices have their own progress and can remain pending or fail.
_Avoid_: Deleted event, void event

**Venue**:
The physical place where an Event happens. A Venue is a property of an Event, not a party in the system.
_Avoid_: Location (when meaning the Organizer)

**Ticket Type**:
A priced category of general-admission Tickets for one Event, with its own capacity. A Ticket Type can be free.
_Avoid_: Tier, product, SKU, price level

**Booking Terms**:
The Organizer's conditions for a booking, which a Buyer acknowledges when the Organizer requires it. Booking Terms are separate from privacy information and marketing consent.
_Avoid_: Privacy consent, marketing opt-in

**Buyer Message**:
An Organizer's operational email about one Event, sent once per unique Buyer email with a valid Ticket, including complimentary Tickets. It excludes fully refunded or void Orders; Buyers forward it to other Attendees.
_Avoid_: Attendee email, campaign, newsletter

**Sales Window**:
The period in which a Ticket Type can be bought. By default it ends when the Event starts.
_Avoid_: On-sale period, availability

**Event Capacity**:
An optional upper limit on Tickets sold across all Ticket Types of an Event.
_Avoid_: Venue capacity, limit

**Hold**:
A time-limited reservation of capacity that exists while a Buyer completes checkout. A Hold becomes Tickets when payment is confirmed, or releases its capacity when it expires.
_Avoid_: Reservation, lock, cart

**Order**:
One Buyer's purchase of one or more Tickets for one Event.
_Avoid_: Purchase, transaction, booking

**Ticket**:
One admission of one Attendee to one Event, identified at the door by a unique code.
_Avoid_: Pass, seat, admission

**Complimentary Ticket**:
A free Ticket that an Admin issues without checkout. It counts against capacity.
_Avoid_: Comp, guest-list ticket, free ticket

**Late Payment**:
A payment confirmed after its Hold expired. It becomes Tickets if capacity is still free, otherwise it is refunded automatically.
_Avoid_: Orphan payment, stale checkout

**Refund**:
The return of an entire Order's payment to the Buyer. A full Refund removes admission from all of the Order's Tickets, regardless of earlier Check-in.
_Avoid_: Cancellation, chargeback, partial refund

**Void Ticket**:
A Ticket that no longer admits anyone because its Order was refunded.
_Avoid_: Cancelled ticket, invalid ticket

### Door

**Door Link**:
A labelled, revocable link that lets anyone holding it check in Attendees for one Event until that Event ends. A Door Link is not a Member.
_Avoid_: Scanner link, guest login, PIN

**Check-in**:
The act of recording that a Ticket has been used to enter its Event. A checked-in Ticket cannot be checked in again unless an Admin or Door Staff undoes the Check-in; Undo never restores admission to a refunded Ticket.
_Avoid_: Scan, redemption, validation
