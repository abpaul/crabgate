"use strict";
(() => {
const minor = value => {
  const match = /^(\d+)(?:\.(\d{0,2}))?$/.exec(String(value || "0"));
  if (!match) throw new Error("Use a positive amount with at most two decimal places.");
  const amount = Number(match[1]) * 100 + Number((match[2] || "").padEnd(2, "0"));
  if (!Number.isSafeInteger(amount)) throw new Error("That amount is too large.");
  return amount;
};
const round = (numerator, denominator) => Number((BigInt(numerator) * 2n + BigInt(denominator)) / (2n * BigInt(denominator)));
const fee = (amount, rate) => amount === 0 ? 0 : Math.min(rate.cap ?? Number.MAX_SAFE_INTEGER, Math.max(rate.minimum || 0, round(BigInt(amount) * BigInt(rate.bps) + BigInt(rate.fixed) * 10000n, 10000)));
const reverse = (target, rate) => {
  if (target === 0) return 0;
  let charge = Number((BigInt(target + rate.fixed) * 10000n) / BigInt(10000 - rate.bps));
  while (charge - fee(charge, rate) < target) charge += 1;
  while (charge > 0 && charge - 1 - fee(charge - 1, rate) >= target) charge -= 1;
  return charge;
};
const payment = (amount, target, rate) => ({ fee: fee(amount, rate), receive: amount - fee(amount, rate), charge: reverse(target, rate) });

if (typeof module !== "undefined") module.exports = { minor, round, fee, reverse, payment };
if (typeof document === "undefined") return;
  const form = document.getElementById("tool");
  if (!form) {
    window.CrabGateToolMath ||= {};
    window.CrabGateToolMath["stripe"] = { minor, round, fee, reverse, payment };
    return;
  }
  const money = (amount, currency = "GBP") => new Intl.NumberFormat("en-GB", { style: "currency", currency }).format(amount / 100);
  const value = id => form.elements.namedItem(id).value;
  const amount = id => minor(value(id));
  const count = id => Number(value(id));
  const write = (values, currency = "GBP") => {
    for (const [key, result] of Object.entries(values)) {
      const node = document.querySelector(`[data-result="${key}"]`);
      if (node) node.textContent = typeof result === "number" ? money(result, currency) : result;
    }
  };
  const run = () => {
    const error = document.getElementById("tool-error");
    try {
      if (!form.checkValidity()) throw new Error("Check the fields. Use whole ticket counts and amounts within the shown limits.");
      update();
      error.hidden = true;
    } catch (problem) {
      error.textContent = problem.message;
      error.hidden = false;
      document.querySelectorAll("[data-result]").forEach(node => { node.textContent = "—"; });
    }
  };
  form.addEventListener("submit", event => event.preventDefault());
  form.addEventListener("input", run);
  form.addEventListener("change", run);
  const rates = JSON.parse(form.dataset.rates);
  let currentCountry;
  const update = () => {
    const country = rates.stripe[count("country")];
    if (currentCountry !== country) {
      form.elements.namedItem("card").replaceChildren(...country.types.map((rate, index) => { const option = document.createElement("option"); option.value = index; option.textContent = rate.name; return option; }));
      currentCountry = country;
    }
    const rate = country.types[count("card")];
    write(payment(amount("amount"), amount("target"), rate), country.currency);
    document.getElementById("rate-note").textContent = `${rate.bps / 100}% + ${money(rate.fixed, country.currency)} per payment. No published cap. Conversion and extra products excluded.`;
  };

  run();
})();
