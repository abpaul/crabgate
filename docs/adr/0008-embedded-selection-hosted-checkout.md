# Embed ticket selection; keep checkout hosted

Organizers need ticket selection on their websites at launch, so CrabGate will provide a dedicated selector iframe supporting an Event or a Series date picker, with a normal hosted link as fallback. Buyer details, questions, Booking Terms, and payment continue through the hosted checkout outside the frame. This reuses the existing payment flow and avoids making checkout depend on third-party cookies; framing permission applies only to the public selector, and the server revalidates all selections.
