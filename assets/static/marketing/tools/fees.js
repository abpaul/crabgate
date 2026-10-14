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
const compare = (price, tickets, payer, provider, stripe, annual) => {
  const booking = fee(price, provider);
  const payment = provider.payment_bps ? fee(price + (payer === "buyer" ? booking : 0), { bps: provider.payment_bps, fixed: 0 }) : 0;
  const buyer = price + (payer === "buyer" ? booking + payment : 0);
  const processor = provider.payment_bps ? payment : provider.processor ? fee(buyer, stripe) : 0;
  const organiserProcessor = provider.payment_bps && payer === "buyer" ? 0 : processor;
  const cost = ((payer === "organiser" ? booking : 0) + organiserProcessor) * tickets;
  const crabgate = annual + fee(price, stripe) * tickets;
  return { fee: booking, year: booking * tickets, buyer, keep: price - (payer === "organiser" ? booking : 0) - organiserProcessor, processor, cost, crabgate, crabkeep: price * tickets - crabgate, difference: cost - crabgate };
};

if (typeof module !== "undefined") module.exports = { minor, round, fee, compare };
if (typeof document === "undefined") return;
  const form = document.getElementById("tool");
  if (!form) {
    window.CrabGateToolMath ||= {};
    window.CrabGateToolMath["fees"] = { minor, round, fee, compare };
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
      document.getElementById("comparison").textContent = "";
      document.querySelectorAll("[data-result]").forEach(node => { node.textContent = "—"; });
    }
  };
  form.addEventListener("submit", event => event.preventDefault());
  form.addEventListener("input", run);
  form.addEventListener("change", run);
  const rates = JSON.parse(form.dataset.rates);
  const provider = rates.providers.find(rate => rate.id === form.dataset.provider);
  const update = () => {
    const currency = value("currency");
    const selectedProvider = currency === provider.currency ? provider : provider.currency_rates?.[currency] ? { ...provider, ...provider.currency_rates[currency] } : null;
    if (!selectedProvider) throw new Error(`No confirmed ${currency} rate. Choose ${provider.currency} to calculate.`);
    const stripe = rates.stripe.find(rate => rate.currency === currency).types[0];
    const annual = round(minor(form.dataset.annualPrice.replace(/^\$/, "")) * rates.exchange[currency], 100);
    const result = compare(amount("price"), count("tickets"), value("payer"), selectedProvider, stripe, annual);
    write({ ...result, totalfee: result.fee + result.processor, totalyear: (result.fee + result.processor) * count("tickets"), crabfixed: annual, crabstripe: result.crabgate - annual, difference: money(Math.abs(result.difference), currency) }, currency);
    const comparison = result.difference > 0 ? `CrabGate costs ${money(result.difference, currency)} less for the organiser this year.` : result.difference < 0 ? `${provider.name} costs ${money(-result.difference, currency)} less for the organiser this year.` : "Both cost the same for the organiser this year.";
    document.getElementById("comparison").textContent = comparison + (value("payer") === "buyer" ? ` The ${form.dataset.annualPrice} yearly payment would be a new cost for the organiser. Buyers would stop paying a booking fee. Stripe’s card fee still applies.` : " Tax is excluded. One ticket per card payment assumed.");
  };

  run();
})();
