# Personal data is encrypted per Organizer, and Operators need an Access Grant

Buyer emails, Attendee names, and payment and email secrets are encrypted in the application with a data key per Organizer. That data key is itself encrypted by a master key that the application loads from its environment. Operators see an Organizer's data only through the application, under an Admin's revocable, time-limited Access Grant. Every Operator request is logged, and the log is visible to that Organizer's Admins. We chose this, following Basecamp's consent model, so that database files, backups, and shell access do not expose personal data by accident, and so that deleting the live Organizer key blocks normal access. Historical control backups retain wrapped data keys, so live-key deletion alone does not make retained backups unrecoverable.

## Consequences

- Encryption does not protect data from someone who controls the running application. It protects against casual access and leaked files.
- Search on encrypted fields decrypts one Event's Attendees in memory. There is no SQL `LIKE` on names or emails.
- The full database export is decrypted.
- Losing the service master key makes encrypted personal data in live databases and backups unreadable.
