# stocklana

Anchor program `stocklana`: cash-secured put vaults on preStocks tokens. A Pool PDA per `(collateral_mint, kind)` owns an SPL vault; pricing comes from a 16-sample median oracle; LP deposits/withdrawals are epoch-gated; `buy_option` is quote-signer-gated with an intrinsic-value floor.

## Fixed-point convention

Every price, strike and size is 1e6 scale — see `SCALE` at programs/stocklana/src/lib.rs:8. Size `1_000_000` == 1 contract == 1 underlying token (underlying mint must have 6 decimals).

## The ponytail convention

A `ponytail:` comment marks a DELIBERATE simplification, sited at the code it explains, stating what was chosen, why it is sufficient under current constraints, and the concrete upgrade path if those constraints change. It is not a TODO and not an apology — it is a tied-off loose end.

Format: `// ponytail: <what was chosen>. <why it's enough here>. <what to swap in if X changes>.`

Rules: one per non-obvious decision, none for obvious ones; never use it to excuse a bug; if there is no plausible future where the simplification breaks, delete the comment instead of writing it. Any non-obvious choice you make gets a ponytail comment.

## Complexity

Follow `.claude/skills/cyclomatic-complexity/SKILL.md` for all code changes.