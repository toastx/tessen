// The money math, checked against the program's own conventions.
// Run: npm test  (from app/)
import assert from "node:assert/strict";
import { SCALE, breakeven, depositRaw, medianOf, payoff, pnl, raw, sharePrice, strikeLadder, ui, usd, num, dur, short } from "./format.js";

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

// strike ladders are deduped — they key React lists, and rounding to a tick
// collapses nearby factors (spot 200: 0.94 and 0.96 both land on 190)
for (const spot of [0.4, 3, 21, 99.99, 200, 214.37, 1850]) {
  for (const factors of [[0.9, 0.95, 1, 1.05], [0.94, 0.96, 0.98, 1], [1, 1, 1]]) {
    const l = strikeLadder(spot, factors);
    assert.equal(new Set(l).size, l.length, `duplicate strike at spot ${spot}: ${l}`);
    assert.ok(l.every(v => v > 0), `non-positive strike at spot ${spot}: ${l}`);
    assert.deepEqual(l, l.slice().sort((a, b) => a - b), "ladder not ascending");
  }
}
assert.deepEqual(strikeLadder(200), [180, 190, 200, 210]);
assert.deepEqual(strikeLadder(200, [0.94, 0.96, 0.98, 1]), [190, 195, 200]); // 0.94/0.96 collapse
assert.deepEqual(strikeLadder(0), []);
assert.deepEqual(strikeLadder(null), []);

// MAX deposits the exact balance and never a unit more — the round trip
// raw -> ui -> raw is what a MAX click does, and it must not overshoot
for (const bal of [1, 1_000_000, 500_004_200_000, 999_999_999_999, 123_456_789]) {
  const typed = ui(bal);                       // what MAX puts in the box
  assert.ok(depositRaw(typed, bal) <= bal, `MAX overshot at balance ${bal}`);
  assert.equal(depositRaw(typed, bal), bal, `MAX undershot at balance ${bal}`);
}
// typing more than you hold is clamped, typing less is untouched
assert.equal(depositRaw(999, 1_000_000), 1_000_000);
assert.equal(depositRaw(0.5, 1_000_000), 500_000);
// unknown balance (no wallet) falls through to the typed amount
assert.equal(depositRaw(25, null), 25_000_000);
assert.throws(() => depositRaw(-5, null), /positive/);

// breakeven mirrors breakeven() in backend/src/chain.rs: strike less the
// per-contract premium, so a $4 premium on a $1060 strike breaks even at $1056
assert.equal(ui(breakeven(raw(1060), raw(1), raw(4))), 1056);
// per CONTRACT, not per position — 2 contracts for $8 is the same breakeven
assert.equal(ui(breakeven(raw(1060), raw(2), raw(8))), 1056);
// never wraps past zero, never divides by a zero size
assert.equal(breakeven(raw(10), raw(1), raw(99)), 0);
assert.equal(breakeven(raw(10), 0, raw(1)), 0);

// PnL is payoff-so-far less premium, and is signed: OTM is a full loss
assert.equal(ui(pnl(raw(1060), raw(1), raw(4), raw(1070))), -4);
// at breakeven it is exactly zero
assert.equal(ui(pnl(raw(1060), raw(1), raw(4), raw(1056))), 0);
// ITM past breakeven it is positive, and scales with size
assert.equal(ui(pnl(raw(1060), raw(1), raw(4), raw(1000))), 56);
assert.equal(ui(pnl(raw(1060), raw(2), raw(8), raw(1000))), 112);

console.log("ok — scale, share price, payoff, breakeven, PnL, median, strike ladder, max deposit and formatting");
