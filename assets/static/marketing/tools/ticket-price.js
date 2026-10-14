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
const budget = (fixed, person, capacity, sell, income, profit, ticketFee, chosen) => {
  const tickets = Number(BigInt(capacity) * BigInt(sell) / 10000n);
  const uncovered = fixed.reduce((sum, cost) => sum + cost, 0) - income;
  const variable = person + ticketFee;
  const price = target => tickets === 0 ? null : round(BigInt(Math.max(0, uncovered + target + variable * tickets)), tickets);
  const contribution = chosen - variable;
  const needed = uncovered <= 0 ? 0 : contribution <= 0 ? null : Math.ceil(uncovered / contribution);
  return { break: price(0), target: price(profit), needed, scenarios: [50, 75, 100].map(percent => { const sold = Math.floor(capacity * percent / 100); return { percent, sold, profit: sold * contribution - uncovered }; }) };
};

if (typeof module !== "undefined") module.exports = { minor, round, fee, budget };
if (typeof document === "undefined") return;
  const form = document.getElementById("tool");
  if (!form) {
    window.CrabGateToolMath ||= {};
    window.CrabGateToolMath["ticket-price"] = { minor, round, fee, budget };
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
    const capacity = count("capacity");
    const result = budget(Array.from({ length: 4 }, (_, index) => amount(`cost-${index}`)), amount("person"), capacity, minor(value("sell")), amount("income"), amount("profit"), amount("fee"), amount("chosen"));
    write({ break: result.break ?? "No expected tickets", target: result.target ?? "No expected tickets", needed: result.needed === null ? "Not possible at this price" : `${result.needed.toLocaleString("en-GB")} tickets${result.needed > capacity ? " — above capacity" : ""}` });
    const table = document.getElementById("scenarios");
    table.replaceChildren(...result.scenarios.map(row => {
      const tr = document.createElement("tr");
      for (const text of [`${row.percent}%`, String(row.sold), money(row.profit)]) { const td = document.createElement("td"); td.textContent = text; tr.append(td); }
      return tr;
    }));
  };

  run();
})();
