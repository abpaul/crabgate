"use strict";
function runChecks(rates, math, check, expectThrow) {
  const fees = math.fees;
  const door = math["door-split"];
  const prices = math["ticket-price"];
  const stripe = math.stripe;
  const maker = math["ticket-maker"];
const uk = rates.stripe[0].types[0];
check(fees.minor('20.05'), 2005);
check(fees.minor('0.01'), 1);
check(fees.round(1, 2), 1);
check(fees.fee(100, { bps: 50, fixed: 0 }), 1);
check(fees.fee(0, uk), 0);
check(fees.fee(2000, uk), 50);
check(fees.fee(1, rates.providers[0]), 25);
check(fees.fee(100000, rates.providers[2]), 2500);
check(fees.fee(2000, rates.providers[1]), 139);
check(fees.compare(2000, 1000, 'organiser', rates.providers[0], uk, 7425), { fee: 140, year: 140000, buyer: 2000, keep: 1860, processor: 0, cost: 140000, crabgate: 57425, crabkeep: 1942575, difference: 82575 });
check(fees.compare(2000, 0, 'organiser', rates.providers[0], uk, 7425), { fee: 140, year: 0, buyer: 2000, keep: 1860, processor: 0, cost: 0, crabgate: 7425, crabkeep: -7425, difference: -7425 });
check(fees.compare(2000, 1000, 'buyer', rates.providers[0], uk, 7425).cost, 0);
check(fees.compare(2000, 1000, 'organiser', rates.providers[4], uk, 7425).difference, 140575);
check(fees.compare(2000, 1000, 'organiser', rates.providers[3], uk, 7425).cost, 110000);
check(fees.compare(2000, 1000, 'organiser', rates.providers[2], rates.stripe[1].types[0], 8910).difference, 51090);
const us = rates.stripe[2].types[0];
const eventbriteUS = { ...rates.providers[4], ...rates.providers[4].currency_rates.USD };
check(fees.compare(2000, 1000, 'buyer', eventbriteUS, us, 29900), { fee: 253, year: 253000, buyer: 2318, keep: 2000, processor: 65, cost: 0, crabgate: 117900, crabkeep: 1882100, difference: -117900 });
check(fees.compare(2000, 1000, 'organiser', eventbriteUS, us, 29900), { fee: 253, year: 253000, buyer: 2000, keep: 1689, processor: 58, cost: 311000, crabgate: 117900, crabkeep: 1882100, difference: 193100 });
check(fees.compare(2000, 1000, 'organiser', eventbriteUS, us, 9900).crabgate, 97900);
check(fees.compare(2000, 1000, 'organiser', eventbriteUS, us, 9900).difference, 213100);
check(fees.compare(0, 1000, 'buyer', eventbriteUS, us, 29900), { fee: 0, year: 0, buyer: 0, keep: 0, processor: 0, cost: 0, crabgate: 29900, crabkeep: -29900, difference: -29900 });
check(fees.compare(2000, 0, 'organiser', eventbriteUS, us, 29900).cost, 0);
const rows = [{ quantity: 100, price: 2000 }];
check(door.settle(rows, [10000, 30000, 10000], 'split', 50000, 7000, 2), { gross: 200000, deductions: 50000, net: 150000, promoter: 45000, artist: 105000, act: 52500, extra: 0 });
check(door.settle(rows, [50000], 'guarantee', 50000, 7000, 2).artist, 50000);
check(door.settle(rows, [50000], 'versus', 50000, 7000, 2).artist, 105000);
check(door.settle(rows, [50000], 'versus', 180000, 7000, 2).promoter, -30000);
check(door.settle(rows, [50000], 'plus', 50000, 7000, 2).artist, 120000);
check(door.settle([], [10000], 'split', 0, 7000, 1), { gross: 0, deductions: 10000, net: -10000, promoter: -10000, artist: 0, act: 0, extra: 0 });
check(door.settle([{ quantity: 1, price: 1 }], [], 'split', 0, 5000, 2), { gross: 1, deductions: 0, net: 1, promoter: 0, artist: 1, act: 0, extra: 1 });
check(prices.budget([100000], 200, 200, 7500, 0, 50000, 0, 1500), { break: 867, target: 1200, needed: 77, scenarios: [{ percent: 50, sold: 100, profit: 30000 }, { percent: 75, sold: 150, profit: 95000 }, { percent: 100, sold: 200, profit: 160000 }] });
check(prices.budget([1], 0, 2, 10000, 0, 0, 0, 1).break, 1);
check(prices.budget([10000], 0, 0, 0, 0, 0, 0, 0), { break: null, target: null, needed: null, scenarios: [{ percent: 50, sold: 0, profit: -10000 }, { percent: 75, sold: 0, profit: -10000 }, { percent: 100, sold: 0, profit: -10000 }] });
check(prices.budget([100], 200, 10, 10000, 200, 0, 0, 100).needed, 0);
check(prices.budget([100], 200, 10, 10000, 0, 0, 0, 100).needed, null);
check(stripe.payment(2000, 2000, uk), { fee: 50, receive: 1950, charge: 2051 });
check(stripe.payment(0, 0, uk), { fee: 0, receive: 0, charge: 0 });
check(stripe.payment(1, 1, { bps: 5000, fixed: 0 }), { fee: 1, receive: 0, charge: 2 });
check(stripe.payment(2000, 2000, rates.stripe[2].types[0]), { fee: 88, receive: 1912, charge: 2091 });
check(maker.ticketNumbers(8, 101), [[101, 102, 103, 104, 105, 106, 107, 108]]);
check(maker.ticketNumbers(9, 101), [[101, 102, 103, 104, 105, 106, 107, 108], [109]]);
check(maker.ticketNumbers(200, 1).at(-1), [193, 194, 195, 196, 197, 198, 199, 200]);
expectThrow(() => maker.ticketNumbers(0, 1));
expectThrow(() => maker.ticketNumbers(201, 1));
expectThrow(() => fees.minor('1.005'));
expectThrow(() => door.settle([], [], 'split', 0, 0, 0));
}
if (typeof module !== "undefined") {
  module.exports = runChecks;
} else {
  fetch("../assets/static/marketing/tools/rates.json").then(response => response.json()).then(rates => {
    let checks = 0;
    const check = (actual, expected) => {
      if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`Expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
      checks += 1;
    };
    const expectThrow = callback => {
      let threw = false;
      try { callback(); } catch (_) { threw = true; }
      if (!threw) throw new Error("Expected an invalid input to be rejected.");
      checks += 1;
    };
    runChecks(rates, window.CrabGateToolMath, check, expectThrow);
    document.getElementById("checks").textContent = `${checks} literal maths checks passed`;
  }).catch(error => { document.getElementById("checks").textContent = `FAIL: ${error.message}`; });
}
