# stocklana

Anchor program `stocklana`: cash-secured put vaults on preStocks tokens. A Pool PDA per `(collateral_mint, kind)` owns an SPL vault; pricing comes from a 32-sample timestamped median oracle; LP deposits/withdrawals are epoch-gated; `buy_option` is quote-signer-gated with an intrinsic-value floor.

## Settlement

Settlement is the median of the oracle samples timestamped in the 30 minutes
ENDING at `epoch_end` (`SETTLEMENT_WINDOW`), never a median taken at the moment
someone calls `close_epoch`. The price is therefore a pure function of
`epoch_end` and the sample history: closing early and closing late produce the
identical number, and the operator's timing is worth nothing. Below
`MIN_SETTLEMENT_SAMPLES` in-window samples the epoch refuses to latch at all
rather than print off a handful of stragglers, and each round's number is kept
permanently in an `EpochRecord` PDA at `[b"epoch", pool, epoch]`.

Keeper-or-authority gates `close_epoch` and `settle`; `force_settle` is
authority-only. Nothing in the program accepts a settlement price as an
argument — see the ponytails on `close_epoch` for why, and for what the recovery
path is when a keeper closes too late.

## Fixed-point convention

Every price, strike and size is 1e6 scale — see `SCALE` at programs/stocklana/src/lib.rs:8. Size `1_000_000` == 1 contract == 1 underlying token (underlying mint must have 6 decimals).

## The ponytail convention

A `ponytail:` comment marks a DELIBERATE simplification, sited at the code it explains, stating what was chosen, why it is sufficient under current constraints, and the concrete upgrade path if those constraints change. It is not a TODO and not an apology — it is a tied-off loose end.

Format: `// ponytail: <what was chosen>. <why it's enough here>. <what to swap in if X changes>.`

Rules: one per non-obvious decision, none for obvious ones; never use it to excuse a bug; if there is no plausible future where the simplification breaks, delete the comment instead of writing it. Any non-obvious choice you make gets a ponytail comment.

## Complexity

Follow `.claude/skills/cyclomatic-complexity/SKILL.md` for all code changes.
## Building on this machine

Two platform-tools versions are installed (v1.48 and v1.54) and Anchor's default
selection picks the broken one — its rustc is too old for `indexmap 2.14.2` and
for edition2024. A plain `anchor build` fails, and `anchor build -- --tools-version
v1.54` also fails, because Anchor 0.32.1 forwards `--` args into its internal
`idl build` step, which runs `cargo test` and rejects the flag.

If you hit an indexmap/rustc/edition2024 error, this is it. Do NOT edit
`Cargo.lock`, `Cargo.toml` or `rust-toolchain.toml` — the dependency graph is fine.
Build in two steps instead:

```bash
anchor build --no-idl -- --tools-version v1.54
RUSTUP_TOOLCHAIN=1.89.0-sbpf-solana-v1.54 anchor idl build \
  -o target/idl/stocklana.json -t target/types/stocklana.ts
anchor test --skip-build          # TS suite; regenerate types above first
cargo test -p stocklana           # pure payoff/collateral unit tests
```

`target/types/stocklana.ts` is what the TS suite imports. It goes stale whenever an
accounts struct changes, and a stale copy fails the tests with a confusing account
error — regenerate it before blaming the test.
