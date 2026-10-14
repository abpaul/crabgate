const assert = require('node:assert/strict');
const runChecks = require('./free_tool_math.js');
const rates = require('../assets/static/marketing/tools/rates.json');
const math = Object.fromEntries(['fees', 'door-split', 'ticket-price', 'stripe', 'ticket-maker'].map(name => [name, require(`../assets/static/marketing/tools/${name}.js`)]));
let checks = 0;
runChecks(rates, math, (actual, expected) => { assert.deepEqual(actual, expected); checks++; }, callback => { assert.throws(callback); checks++; });
console.log(`${checks} literal maths checks passed`);
