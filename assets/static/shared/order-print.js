(() => {
  const button = document.getElementById('print-order');
  if (button) button.addEventListener('click', () => window.print());
})();
