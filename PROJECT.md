# Tessen — cash-secured puts on pre-IPO stock

## Problem

[preStocks](https://prestocks.com) lets people trade tokenized exposure to
pre-IPO companies (Anthropic, SpaceX, ...). That exposure is directional only —
there's no way to get paid for taking on downside risk, or to buy downside
protection, the way options markets do for public equities. The underlying
markets are also thin: one preStock's on-chain DEX price sat 29% away from its
real SPV mark, which makes any on-chain product that trusts a single spot price
trivially manipulable.

## What Tessen does

Tessen is a Solana protocol that turns LP collateral into cash-secured put
vaults on preStocks tokens, priced off a manipulation-resistant oracle:

- **LPs** deposit stablecoin/collateral into a pool and earn premium for
  underwriting puts, epoch over epoch.
- **Buyers** pay a premium to hold a put — downside protection or a
  directional short — against a real preStocks mark price, not a thin DEX
  quote.
- **Settlement is deterministic**: each epoch's price is the median of oracle
  samples timestamped in the 30 minutes ending at `epoch_end`, so nobody's
  timing of `close_epoch` — early, late, or exactly on time — can move the
  number. Below a minimum sample count the epoch simply refuses to close
  rather than settle on a handful of stragglers, and every settled price is
  recorded permanently on-chain.

A round works like a normal cash-secured put desk, just run entirely by a
program and a scheduled keeper instead of a market maker:

1. **Genesis / roll** — a pool starts with no epoch. `roll_epoch(epoch_end)`
   opens one with a fixed close time; epoch length isn't stored on the pool at
   all, it's the keeper's own clock, so the same program serves a 1-hour
   aggressive pool and a 24-hour conservative pool without any extra state.
2. **Trade** — while the epoch is open, LPs can deposit/withdraw and buyers can
   `buy_option`. Every quote is priced by the backend, signed by a dedicated
   quote signer keypair, and rejected on-chain if it undercuts intrinsic value
   or falls under a minimum premium floor — a malicious or buggy backend
   cannot sell a put for less than the payout it already owes.
3. **Close** — at `epoch_end` the keeper (or, as a backstop, the authority)
   calls `close_epoch`, which reads back the oracle ring, medians every sample
   inside the last 30 minutes, and latches that number into an `EpochRecord`
   PDA forever. If fewer than 8 samples fall in that window, it refuses — a
   thin window is a red flag, not a price.
4. **Settle** — `settle` walks positions and pays out against the latched
   price; `force_settle` is the authority-only escape hatch for a stuck round.
5. **Roll again** — a fresh `roll_epoch` opens the next round on the same
   pool, same vault, same LPs.

## How pricing works

The backend prices every quote as intrinsic value plus a spread (200 bps of
collateral by default), and stamps it with a short TTL — 60 seconds
server-side, always under the on-chain 300-second ceiling, so a quote can't be
held and replayed once the market's moved. The buyer's transaction is
co-signed by the quote signer and only then submitted, so the same signature
that authorized the price also authorizes the trade. On-chain, `buy_option`
independently re-derives the position's intrinsic value from the oracle and
rejects any quote priced below it, and enforces a 10 bps minimum premium floor
so a pool can never write a put for free.

Everything — price, strike, size — is fixed-point at `SCALE = 1_000_000`, so a
size of `1_000_000` is exactly one contract on one underlying token.

## Why this is hard to fake

- The oracle is a 32-sample ring, one per underlying, shared by every pool on
  that preStock. A single push can't move it more than 10% from the running
  median, and pushes are rate-limited to at least 30 seconds apart, so 32
  slots span at least 16 minutes and the ring can't be stuffed with correlated
  samples inside one 30-minute settlement window. A settlement window also has
  to *span* at least 15 minutes of real time, not just contain 8 timestamps —
  defense in depth behind the push rate limit.
- `buy_option` is gated by a quote signer *and* an on-chain intrinsic-value
  floor — the backend can't quote a price that undercuts a payout the chain
  already guarantees, even if the backend itself were compromised.
- The oracle also enforces a staleness ceiling (15 minutes): quoting and
  buying both refuse to proceed against a price that's stopped updating,
  instead of silently pricing off a stale number.
- The keeper polls preStocks' real SPV mark (`markPrice`, not the manipulable
  `tokenPrice`) on a dedicated thread so a slow HTTP call can never delay
  closing an epoch on time — a delay past `epoch_end` rotates samples out of
  the ring and bricks that epoch's close permanently. The poller tolerates its
  own mutex being poisoned and keeps serving the last good price rather than
  failing fatally, because a stale push still beats no push at all.
- `tests/exploit.ts` is a standing adversarial PoC suite, run separately from
  the protocol test suite, that measures on-chain — with authority, keeper,
  quote signer, LP and attacker held by separate keys — exactly what each
  compromised role can do rather than assuming it's safe. It's also honest
  about what's *not* yet solved: with today's guards, a compromised keeper key
  can still walk the settlement window down within the legal per-push
  deviation band and drain an epoch at "fair" premiums, and an epoch that
  never accumulates enough in-window samples (dust position, or a keeper that
  pastes over the window after expiry) can permanently brick LP withdrawals
  for that pool. Those are known, load-bearing tradeoffs of a keeper-driven
  design, not oversights — the PoC exists to keep them visible instead of
  buried.

## Architecture

| Piece | Role |
|---|---|
| `programs/tessen` (Anchor/Rust) | Pools, oracle ring, epoch lifecycle, option payoff/collateral math |
| `keeper` (Rust) | Off-chain operator: polls the real preStocks price, pushes it, closes/settles/rolls each pool's epoch on schedule |
| `backend` (Rust/axum) | Indexes chain state into SQLite, prices quotes, co-signs `buy_option` as the quote signer, serves the frontend over REST + websocket |
| `app` (React) | Dashboard, trade, liquidity, positions and history UI |

Two pools can share one oracle: the pool PDA is
`[b"pool", collateral_mint, kind]`, one PUT pool per collateral mint, while the
oracle PDA is `[b"oracle", underlying]` — so a fast, aggressive pool and a
slow, conservative pool on the same preStock just need two different
collateral mints, and both keepers push into the same price ring.

## Status / what's deployed

Live on devnet, running on devnet USDC as collateral with the keeper polling
preStocks' real mainnet mark price for Anthropic — the settlement number is
real even though the collateral and underlying mints aren't. That substitution
is deliberate rather than a shortcut still owed: preStocks' real Anthropic mint
is Token-2022 with 9 decimals, which Anchor's `Account<'info, Mint>` can't
deserialize, and a cash-secured put never transfers the underlying anyway — it
only names what the strike refers to, so the mint standing in for it doesn't
change what's being priced or settled.

