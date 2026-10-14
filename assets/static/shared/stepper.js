document.documentElement.classList.add('js');
for (const form of document.querySelectorAll('[data-tickets]')) {
  const rows = Array.from(form.querySelectorAll('.tt[data-price]')).filter(row => row.querySelector('[data-quantity], [data-seat]'));
  const refresh = () => {
    let count = 0;
    let cents = 0;
    for (const row of rows) {
      const input = row.querySelector('[data-quantity]');
      const value = input ? Number(input.value) || 0 : row.querySelectorAll('[data-seat]:checked').length;
      if (input) row.querySelector('output').textContent = String(value);
      count += value;
      cents += value * Number(row.dataset.price);
    }
    form.querySelector('[data-count]').textContent = `${count} ${count === 1 ? 'ticket' : 'tickets'}`;
    form.querySelector('[data-total]').textContent = new Intl.NumberFormat('en-IE', { style: 'currency', currency: form.dataset.currency || 'EUR', maximumFractionDigits: cents % 100 === 0 ? 0 : 2 }).format(cents / 100);
  };
  for (const row of rows) {
    const input = row.querySelector('[data-quantity]');
    if (!input) {
      row.querySelectorAll('[data-seat]').forEach(seat => seat.addEventListener('change', refresh));
      continue;
    }
    input.addEventListener('change', refresh);
    for (const button of row.querySelectorAll('[data-step]')) {
      button.addEventListener('click', () => {
        const next = Number(input.value) + Number(button.dataset.step);
        input.value = String(Math.max(0, Math.min(Number(input.max), next)));
        refresh();
      });
    }
  }
  refresh();
}
