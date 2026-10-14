# One SQLite database per Organizer, one process for all Organizers

Each Organizer's data lives in its own SQLite file. A small control database maps the request's host (the Organizer's subdomain) to the Organizer's file. One application process serves all Organizers. We chose this because it gives per-Organizer backup, restore, export, and deletion as single-file operations, keeps idle memory to one process, and keeps Organizer operations isolated.

## Considered Options

- One shared database with an `organizer_id` column: cheaper queries across Organizers, but restore and export of one Organizer become row-level surgery, and a missing filter leaks data.
- One container per Organizer: strong isolation, but idle memory grows with every Organizer.
- Postgres: better concurrent writes, but another service to operate.

## Consequences

- Every request must resolve its Organizer before it touches data. There is no default database.
- Schema migrations run against every Organizer database at boot. An Organizer whose migration fails is taken offline, and the others continue.
- Reports across Organizers need a separate path. They are not SQL joins.
- Members, their roles per Organizer, Subscriptions, and Access Grants live in the control database. An Organizer's export does not include logins.
- Background jobs carry the Organizer id and open that Organizer's database themselves.
- Continuous backup uses one Litestream process that watches the Organizer database directory.
