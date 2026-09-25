# Tessen

Cash-secured put vaults on [preStocks](https://prestocks.com) pre-IPO tokens, on Solana.

LPs deposit collateral into a pool; the pool writes cash-secured puts against a
real preStocks mark price; buyers pay a premium for downside protection or
directional exposure; every round settles on-chain against a tamper-resistant
oracle median.

## How it works

- **Pool** — one PDA per `(collateral_mint, kind)`, owning an SPL vault. LPs
  deposit/withdraw against it; deposits and withdrawals are epoch-gated so a
  pool's collateral is never pulled mid-round.
- **Oracle** — a 32-sample timestamped ring per underlying, `[b"oracle", underlying]`,
  shared by every pool on that preStock. A keeper polls the real preStocks
  `markPrice` and pushes it on-chain.
- **Epoch** — a fixed-length round. `roll_epoch` opens it, `close_epoch` latches
  a settlement price (the median of samples in the 30-minute window ending at
  `epoch_end` — never the moment someone calls `close_epoch`), `settle` pays out
  every position against that price.
- **buy_option** — quote-signer-gated: the backend prices the option and
  co-signs the transaction, with an on-chain intrinsic-value floor so no quote
  can undercut a payout that's already guaranteed.

Every price, strike and size is fixed-point at 1e6 scale.


## Layout

```
programs/tessen/   Anchor program (pools, oracle, epochs, options)
backend/              Read API + quote signer + indexer (Rust, axum, SQLite)
keeper/               Off-chain operator: pushes prices, closes/settles/rolls epochs
app/                  React frontend (dashboard, trade, liquidity, positions)
scripts/              Devnet setup, bootstrap, ix-shape checks
tests/                Anchor TS integration tests + exploit/invariant checks
```

## Building

Two `platform-tools` versions are installed and Anchor's default picks the one
whose rustc can't build `indexmap 2.14.2` / edition2024. Build in two steps:

```bash
anchor build --no-idl -- --tools-version v1.54
RUSTUP_TOOLCHAIN=1.89.0-sbpf-solana-v1.54 anchor idl build \
  -o target/idl/tessen.json -t target/types/tessen.ts
anchor test --skip-build          # TS integration + exploit suite
cargo test -p tessen           # pure payoff/collateral/settlement unit tests
```

## Running against devnet

```bash
bash scripts/devnet-setup.sh            # keys + SOL (idempotent)
anchor deploy --provider.cluster devnet
node scripts/devnet-bootstrap.mjs       # mints, oracle, both pools; prints keeper env
```

The bootstrap script pushes no price — the keeper polls preStocks and pushes on
its first tick. Until a keeper runs, the oracle ring is empty and the backend's
`/quote` answers `409 oracle has no samples`.

```bash
cargo run --manifest-path keeper/Cargo.toml    # off-chain operator
cargo run --manifest-path backend/Cargo.toml   # read API + quote signer
cd app && npm run dev                          # frontend
```

Offline checks (no validator, no SOL):

```bash
cargo test --manifest-path keeper/Cargo.toml
node scripts/bootstrap-ixcheck.mjs
```

## Two pools, one oracle

The pool PDA is `[b"pool", collateral_mint, kind, poolname]` — one PUT pool per
collateral mint, so a conservative (24h epoch) and an aggressive (1h epoch)
pool on the same underlying need two different collateral mints. Epoch length
isn't a pool field; it's the keeper's `EPOCH_LEN_SECS`, passed to `roll_epoch`.

## Status

Devnet. The real preStocks mint (`Pren1FvFX6J3E4kXhJuCiAD5aDmGEb7qJRncwA8Lkhw`)
is Token-2022 with 9 decimals and can't be used as an Anchor `Mint` account
today, so devnet uses a 6-decimal stand-in mint — the price fed to the oracle
is still the real preStocks price.
