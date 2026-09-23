// The money math, checked against the program's own conventions.
// Run: npm test  (from app/)
import assert from "node:assert/strict";
import { SCALE, medianOf, payoff, raw, sharePrice, ui, usd, num, dur, short } from "./format.js";

// 1e6 scale round-trips, and `raw` refuses what would silently corrupt an amount
assert.equal(raw(200), 200_000_000);
assert.equal(raw(0.000001), 1);
assert.equal(ui(200_000_000), 200);
assert.equal(raw(ui(123_456_789)), 123_456_789);
assert.throws(() => raw(-1), /positive/);
assert.throws(() => raw("abc"), /positive/);
assert.throws(() => raw(1e12), /too large/);

// share price mirrors Pool::share_price — par when the pool is empty
assert.equal(sharePrice({ assets: 0, total_shares: 0 }), 1);
assert.equal(sharePrice(null), 1);
assert.equal(sharePrice({ assets: 2_080_000, total_shares: 2_000_000 }), 1.04);

// put payoff mirrors payoff(): raw in, raw out, 1e6 throughout
// 1 contract, strike 200, settle 180 -> $20
assert.equal(ui(payoff(raw(200), raw(1), raw(180))), 20);
// 3 contracts -> $60
assert.equal(ui(payoff(raw(200), raw(3), raw(180))), 60);
// out of the money -> nothing, never negative
assert.equal(payoff(raw(200), raw(3), raw(214.37)), 0);
// payoff can never exceed the collateral the pool locked (strike * size / SCALE)
const collateral = (raw(200) * raw(3)) / SCALE;
for (const spot of [0, 1, 99.5, 200]) {
  assert.ok(payoff(raw(200), raw(3), raw(spot)) <= collateral);
}

// upper median on even counts, matching spot_from / median_of
assert.equal(medianOf([3, 1, 2], 3), 2);
assert.equal(medianOf([4, 1, 3, 2], 4), 3); // upper, not 2.5 and not 2
assert.equal(medianOf([9, 9, 9, 1, 1], 5), 9);
// only the filled prefix of the ring counts
assert.equal(medianOf([5, 7, 0, 0, 0], 2), 7);
assert.equal(medianOf([5, 7, 0, 0, 0], 0), null);

// formatting doesn't lie about zero/undefined
assert.equal(usd(0), "$0.00");
assert.equal(usd(undefined), "$0.00");
assert.equal(num(1234.5678, 4), "1,234.5678");
assert.equal(dur(0), "00h 00m 00s");
assert.equal(dur(3661), "01h 01m 01s");
assert.equal(short("9Fq2abcdefgh8mTa"), "9Fq2…8mTa");
assert.equal(short(null), "—");

console.log("ok — scale, share price, payoff, median and formatting");
