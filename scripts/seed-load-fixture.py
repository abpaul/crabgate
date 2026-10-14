import argparse
import datetime
import sqlite3
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description="Add future Events to an empty test Organizer database")
    parser.add_argument("database", type=Path)
    parser.add_argument("--events", type=int, default=100)
    args = parser.parse_args()
    if args.events < 1:
        parser.error("--events must be positive")
    if not args.database.is_file():
        parser.error("database must already exist")

    uri = f"file:{args.database.resolve()}?mode=rw"
    with sqlite3.connect(uri, uri=True) as db:
        existing = db.execute("SELECT COUNT(*) FROM events").fetchone()[0]
        if existing:
            parser.error("database already contains Events")
        first = datetime.datetime.now(datetime.timezone.utc).replace(
            hour=19, minute=0, second=0, microsecond=0
        ) + datetime.timedelta(days=365)
        created_at = int(datetime.datetime.now(datetime.timezone.utc).timestamp())
        for index in range(args.events):
            starts = first + datetime.timedelta(days=index)
            event_id = f"load-event-{index:04d}"
            db.execute(
                "INSERT INTO events (id, title, description, date, start_time, end_time, timezone, starts_at, ends_at, venue, address, status, event_capacity, max_per_order, attendee_name_required, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    event_id,
                    f"Load Event {index:04d}",
                    "",
                    starts.strftime("%Y-%m-%d"),
                    "19:00",
                    "21:00",
                    "UTC",
                    int(starts.timestamp()),
                    int(starts.timestamp()) + 7200,
                    "Hall",
                    "1 Main Street",
                    "Published",
                    None,
                    10,
                    0,
                    created_at,
                    created_at,
                ),
            )
            for kind in range(2):
                db.execute(
                    "INSERT INTO ticket_types (id, event_id, name, price_cents, capacity, sales_open, sales_close, sort_order) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    (
                        f"load-type-{index:04d}-{kind}",
                        event_id,
                        f"Type {kind}",
                        1500 + kind * 500,
                        100,
                        None,
                        None,
                        kind,
                    ),
                )
    print(f"seeded {args.events} Events and {args.events * 2} Ticket Types")


if __name__ == "__main__":
    main()
