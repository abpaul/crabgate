const deviceZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
for (const suffix of document.querySelectorAll('[data-event-zone]')) {
  if (deviceZone !== suffix.dataset.eventZone) {
    suffix.textContent = `(${suffix.dataset.zoneLabel})`;
  }
}
