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
const settle = (rows, deductions, deal, guarantee, percentage, acts) => {
  if (!Number.isInteger(acts) || acts < 1) throw new Error("Enter at least one act.");
  const gross = rows.reduce((sum, row) => sum + row.quantity * row.price, 0);
  const costs = deductions.reduce((sum, cost) => sum + cost, 0);
  const net = gross - costs;
  const split = round(BigInt(Math.max(0, net)) * BigInt(percentage), 10000);
  const artist = deal === "guarantee" ? guarantee : deal === "versus" ? Math.max(guarantee, split) : deal === "plus" ? guarantee + round(BigInt(Math.max(0, net - guarantee)) * BigInt(percentage), 10000) : split;
  return { gross, deductions: costs, net, promoter: net - artist, artist, act: Math.floor(artist / acts), extra: artist % acts };
};

if (typeof module !== "undefined") module.exports = { minor, round, fee, settle };
if (typeof document === "undefined") return;
  const form = document.getElementById("tool");
  if (!form) {
    window.CrabGateToolMath ||= {};
    window.CrabGateToolMath["door-split"] = { minor, round, fee, settle };
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
  const update = () => {
    const result = settle(Array.from({ length: 3 }, (_, index) => ({ quantity: count(`quantity-${index}`), price: amount(`price-${index}`) })), ["fees", "venue", "sound", "other"].map(amount), value("deal"), amount("guarantee"), minor(value("percentage")), count("acts"));
    write(result);
    const terms = value("deal") === "split" ? `${value("percentage")}% of net` : value("deal") === "guarantee" ? `${money(amount("guarantee"))} guarantee` : `${money(amount("guarantee"))} guarantee; ${value("percentage")}% artist share`;
    document.getElementById("settlement-note").textContent = `${form.elements.namedItem("deal").selectedOptions[0].textContent}: ${terms}. ${count("acts")} acts. ` + (result.extra ? `Give one extra penny to ${result.extra} act${result.extra === 1 ? "" : "s"} to settle exactly. ` : "Shares divide evenly. ") + (result.promoter < 0 ? "The promoter must cover the shortfall shown above." : "Check the agreed costs with both sides before paying.");
  };
  document.getElementById("print").addEventListener("click", () => { if (form.reportValidity()) window.print(); });

  run();
})();
