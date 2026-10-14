# CrabGate design

CrabGate has one look everywhere, inspired by Basecamp and HEY: a notice board. Pages are a dotted paper board; content sits on white paper sheets with tape, pins, stamps, and highlighter marks. Organizers cannot restyle their Shop (see `docs/adr/0005-no-white-label.md`); the Organizer appears only by name.

## Source of truth

- Stylesheet: `prototypes/fast/site/shared/crabgate.css`. Its tokens and classes are normative; the app ships this file (split or renamed as needed) instead of re-creating the look.
- Reference screens: `prototypes/fast/site/` (open `index.html`). Every app screen has a matching prototype page; build the app screen to match it.
- Screenshots of every screen at 390 px and 1440 px: `prototypes/fast/site/screenshots/`.

## Tokens

| Token | Value | Use |
|---|---|---|
| `--board` | `#F4EFE4` + dot pattern | Page background (`.board`) |
| `--paper` | `#FFFFFF` | Sheets, cards, tables |
| `--ink` | `#29353C` | Body text, buttons |
| `--deep` | `#0B151B` | Headlines, text on yellow |
| `--muted` | `#5F686D` | Secondary text |
| `--yellow` | `#FFC933` | Current navigation item, key price, tape and pins, the logo's Ticket |
| `--mark` | `#FFDC74` at 50% | `<mark>` highlighter behind key words |
| `--error` | `#B42318` | Errors and destructive actions only |
| `--ok` | `#14532D` | Door "checked in" result |

Measured contrast (WCAG 2.x): ink on board 10.97, ink on paper 12.58, muted on board 4.97, muted on paper 5.69, white on ink button 12.58, deep on yellow 12.01, error on paper 6.57, white on ok 9.11.

## Type

- Figtree only, locally served variable WOFF2 (OFL). Never load fonts from Google or another third party.
- Body 20 px / 1.5. Headlines weight 900 with tracking −0.02 to −0.03 em (`.h-xl`, `.h-l`, `.h-m`); smaller headings 800 (`.h-s`).
- Kickers: 16 px, weight 800, uppercase, +0.08 em, muted.
- Prices and counts use tabular figures (`.num`).

## Rules

- Primary buttons are slate (`.btn`) with a pressed bottom edge. Yellow buttons are rare. Red (`.btn-danger`) only for destructive actions such as Refund.
- Links are bold and underlined, in the text color.
- Status: stamps (`.stamp`) on Shop pages; labels (`.status-*`) in Back Office tables. Paid = yellow label; Late payment = outlined; Refunded = struck; Refunded automatically = dashed; Draft = dashed with an empty dot.
- Door: white background, 3 px deep rules, bottom tabs (Scan, Find by name), and full-screen result colors: ok green (checked in), yellow (already checked in), error red (Void Ticket, wrong Event). Door shows the Event and Organizer names, never the CrabGate mark.
- Minimum touch target 44 px everywhere, 64 px on the Door. Visible focus ring: 3 px deep outline; fields use a yellow outline.
- Respect `prefers-reduced-motion`: no transitions or animations.
- Copy follows the Voice section below and uses only the terms in `CONTEXT.md`.

## Voice

Plain, warm, and direct, in the manner of 37signals. One voice at three levels.

| Surface | Level |
|---|---|
| Marketing pages, Back Office, Operator, emails to Members | Full voice. "We" is CrabGate, "you" is the reader. |
| Shop and emails to Buyers | Plain and kind. No jokes and no "we": the Organizer's name speaks. |
| Door | Short words only. Staff read it at a glance in a queue. |

Rules:

1. Talk like a person. Use "we", "you", and contractions. British spelling.
2. Sell first. A marketing page opens with the reader's problem, then the outcome, then the price. Agitate with a sum the reader can check, built from published rates, labelled as an estimate, with the source and date.
3. State a limit once, plainly, where the reader asks: in "What's coming next" or the questions. No apology, and no limit in a hero. The hero names no payment provider.
4. Explain, don't command. "You'll need a subscription before you can publish" beats "A Subscription is required".
5. An error says what happened and what to do next.
6. Opinions are about ourselves: what we build and what we won't. No jabs at named competitors. Their fees appear only as dated, sourced numbers.
7. No invented proof: no customers, counts, quotes, or ratings that do not exist.
8. Sentence case everywhere, including headings, buttons, and labels. Capitals are for the first word and for things with a name on screen: Shop, Door, Back Office, Setup, Door Link, Access Grant, the roles Admin, Door Staff, and Operator, and status labels such as Draft and Paid. Write ticket, event, order, buyer, attendee, refund, organizer, and subscription in lower case.
9. "On our list" is only for work we intend to do: the Organizer's own domain, logo, and colours (ADR 0005), a seating map, and a one-click payment connection. Never give a date.
10. Paul speaks as "I" in the founder block and the setup offer. Everywhere else CrabGate is "we".

## Logo

| File | Use |
|---|---|
| `docs/design/assets/crabgate-mark.svg` | Slate claw holding a yellow Ticket; 48 px and larger |
| `docs/design/assets/crabgate-mark-small.svg` | Simplified; below 48 px ("Powered by CrabGate") |
| `assets/static/shared/favicon.svg` | The small mark on a white rounded tile, so the claw shows on dark browser tabs. `favicon.ico` (16, 32, 48 px) and `apple-touch-icon.png` (180 px) are rendered from it and served at the site root |
| `docs/design/assets/crabgate-mark-mono.svg` | One-color slate; documents and invoices |

The claw disappears on dark backgrounds; never place the mark on slate or other dark surfaces. The mascot PNGs still use the retired coral and teal colors and are not used.
