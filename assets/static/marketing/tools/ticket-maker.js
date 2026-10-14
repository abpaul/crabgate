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
const ticketNumbers = (count, start) => {
  if (!Number.isInteger(count) || count < 1 || count > 200 || !Number.isInteger(start) || start < 0 || start > 999999) throw new Error("Choose 1 to 200 tickets and a whole starting number from 0 to 999999.");
  return Array.from({ length: Math.ceil(count / 8) }, (_, page) => Array.from({ length: Math.min(8, count - page * 8) }, (_, index) => start + page * 8 + index));
};

if (typeof module !== "undefined") module.exports = { minor, round, fee, ticketNumbers };
if (typeof document === "undefined") return;
  const form = document.getElementById("tool");
  if (!form) {
    window.CrabGateToolMath ||= {};
    window.CrabGateToolMath["ticket-maker"] = { minor, round, fee, ticketNumbers };
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
  form.elements.namedItem("count").min = "1";
  const update = () => {
    const qr = value("qr");
    if (Array.from(qr).length > 200) throw new Error("Use at most 200 characters for the QR text.");
    const pages = ticketNumbers(count("count"), count("start"));
    const container = document.getElementById("tickets");
    container.replaceChildren(...pages.map(numbers => {
      const page = document.createElement("div"); page.className = "ticket-page";
      for (const number of numbers) {
        const ticket = document.createElement("div"); ticket.className = "made-ticket";
        const body = document.createElement("div"); body.className = "ticket-body";
        for (const [tag, text] of [["b", value("event")], ["p", value("when").replace("T", " at ")], ["p", value("venue")], ["p", `${value("kind")} · ${value("price")}`], ["b", `No. ${number}`]]) { const node = document.createElement(tag); node.textContent = text; body.append(node); }
        if (qr) { const image = document.createElement("img"); image.src = `/tools/ticket-qr?text=${encodeURIComponent(qr)}`; image.alt = "QR code for your text"; image.width = 52; image.height = 52; body.append(image); }
        const stub = document.createElement("div"); stub.className = "ticket-stub"; stub.textContent = `No. ${number}`;
        ticket.append(body, stub); page.append(ticket);
      }
      return page;
    }));
    const summary = document.getElementById("ticket-summary");
    const mark = document.createElement("span"); mark.className = "hl-under"; mark.textContent = `${count("count")} tickets`;
    summary.replaceChildren(mark, document.createTextNode(`, numbered ${count("start")} to ${count("start") + count("count") - 1}.`));
  };
  document.getElementById("print").addEventListener("click", async () => {
    if (!form.reportValidity()) return;
    run();
    if (!document.getElementById("tool-error").hidden) return;
    try {
      await Promise.all(Array.from(document.querySelectorAll("#tickets img"), image => image.decode()));
      window.print();
    } catch (_) {
      const error = document.getElementById("tool-error"); error.textContent = "The QR code did not load. Try again or leave the QR text blank before printing."; error.hidden = false;
    }
  });

  run();
})();
