// Display helpers. Everything on chain is u64 at 1e6 (see SCALE in
// programs/stocklana/src/lib.rs) — `ui()` is the only place that divides.

export const SCALE = 1_000_000;

/** raw u64 (as number | bigint | string) -> UI float */
export const ui = raw => Number(raw ?? 0) / SCALE;
/** UI float -> raw u64 integer, rounded. Throws rather than silently truncating. */
export const raw = n => {
  const v = Math.round(Number(n) * SCALE);
  if (!Number.isFinite(v) || v < 0) throw new Error("not a positive amount: " + n);
  if (v > Number.MAX_SAFE_INTEGER) throw new Error("amount too large: " + n);
  return v;
};

export const usd = (n, d = 2) =>
  "$" + Number(n || 0).toLocaleString("en-US", { minimumFractionDigits: d, maximumFractionDigits: d });
export const num = (n, d = 0) =>
  Number(n || 0).toLocaleString("en-US", { minimumFractionDigits: d, maximumFractionDigits: d });

export const dur = s => {
  if (s <= 0) return "00h 00m 00s";
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), x = Math.floor(s % 60);
  return `${String(h).padStart(2, "0")}h ${String(m).padStart(2, "0")}m ${String(x).padStart(2, "0")}s`;
};
export const clock = ts =>
  new Date(ts * 1000).toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit", hour12: false }) + " UTC";
export const short = k => (k ? k.slice(0, 4) + "…" + k.slice(-4) : "—");

/** NAV per share, 1e6 scale — mirrors Pool::share_price (par when empty). */
export const sharePrice = pool =>
  !pool || !Number(pool.total_shares) ? 1 : Number(pool.assets) / Number(pool.total_shares);

/**
 * Put payoff in RAW 1e6 units, mirroring `payoff()` in the program:
 * max(0, strike - spot) * size / SCALE. Raw in, raw out — pass it through
 * `ui()` to display.
 */
export const payoff = (strikeRaw, sizeRaw, spotRaw) =>
  (Math.max(0, Number(strikeRaw) - Number(spotRaw)) * Number(sizeRaw)) / SCALE;

/**
 * Upper median of the filled oracle ring — mirrors `spot_from` in
 * backend/src/chain.rs and `median_of` in the program (upper median on even
 * counts). Getting the parity wrong here would show a spot the chain disagrees
 * with, so it is covered by selfcheck.js.
 */
export function medianOf(samples, count) {
  if (!count) return null;
  const buf = samples.slice(0, count).map(Number).sort((a, b) => a - b);
  return buf[Math.floor(count / 2)];
}

/**
 * A ladder of strike prices around spot, on a tick the price justifies.
 * Rounding to a tick can collapse two factors onto the same strike (at spot
 * 200, 0.94 and 0.96 both land on 190), so the result is deduped — callers
 * key React lists by the strike itself.
 */
export function strikeLadder(spotUi, factors = [0.9, 0.95, 1.0, 1.05]) {
  if (!spotUi) return [];
  const tick = spotUi >= 100 ? 5 : spotUi >= 20 ? 1 : 0.5;
  const ticks = factors.map(f => Math.round((spotUi * f) / tick) * tick);
  return [...new Set(ticks)].filter(v => v > 0).sort((a, b) => a - b);
}
