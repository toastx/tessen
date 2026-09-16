use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("86kDc93MAkfLm3JNi43KYcFPiKZ765tYHPTekxmp1Ukb");

/// Every price, strike and size is fixed-point 1e6.
/// size 1_000_000 == 1 contract == 1 underlying token (underlying mint must have 6 decimals).
pub const SCALE: u128 = 1_000_000;
pub const SAMPLES: usize = 16;
pub const KIND_PUT: u8 = 0;
pub const KIND_CALL: u8 = 1;
pub const MAX_QUOTE_TTL: i64 = 300;
pub const MAX_ORACLE_STALENESS: i64 = 900;

#[macro_export]
macro_rules! pool_seeds {
    ($pool:expr) => {
        [
            b"pool".as_ref(),
            $pool.collateral_mint.as_ref(),
            std::slice::from_ref(&$pool.kind),
            std::slice::from_ref(&$pool.bump),
        ]
    };
}

#[program]
pub mod stocklana {
    use super::*;

    pub fn init_pool(ctx: Context<InitPool>, kind: u8, quote_signer: Pubkey) -> Result<()> {
        require!(kind <= KIND_CALL, StockError::BadKind);
        // ponytail: covered-call vaults are collateralised in the very underlying
        // they write calls on (payoff is paid in underlying, see `payoff` for
        // KIND_CALL); cash-secured put vaults are collateralised in a different
        // mint (USDC) from the underlying they price against. Splitting the two
        // relationships here covers both kinds; move to a per-pool collateral
        // whitelist if a third kind ever needs a looser pairing.
        if kind == KIND_CALL {
            require!(
                ctx.accounts.oracle.underlying == ctx.accounts.collateral_mint.key(),
                StockError::OracleMintMismatch
            );
        } else {
            require!(
                ctx.accounts.oracle.underlying != ctx.accounts.collateral_mint.key(),
                StockError::OracleMintMismatch
            );
        }
        let pool = &mut ctx.accounts.pool;
        pool.authority = ctx.accounts.admin.key();
        pool.kind = kind;
        pool.collateral_mint = ctx.accounts.collateral_mint.key();
        pool.vault = ctx.accounts.vault.key();
        pool.oracle = ctx.accounts.oracle.key();
        pool.quote_signer = quote_signer;
        pool.total_shares = 0;
        pool.locked = 0;
        pool.available = 0;
        pool.epoch = 0;
        pool.epoch_end = 0;
        pool.settle_price = 0;
        // ponytail: epoch 0 starts out already closed (`settled_epoch == epoch`), which
        // is what lets the genesis `roll_epoch` through without a `close_epoch` on an
        // epoch that never traded. It is sound because epoch 0 has `epoch_end == 0`, so
        // `buy_option` can never write into it and `settle_price == 0` is never read.
        // Give `Pool` an explicit `status` enum if a future epoch ever needs to be
        // distinguishable from this pre-genesis one.
        pool.settled_epoch = 0;
        pool.open_positions = 0;
        pool.bump = ctx.bumps.pool;
        Ok(())
    }

    /// Opens the next epoch. Every option written in it expires at `epoch_end`,
    /// so NAV can only move at a boundary. Keeper passes now+86400 for daily.
    pub fn roll_epoch(ctx: Context<RollEpoch>, epoch_end: i64) -> Result<()> {
        require!(
            epoch_end > Clock::get()?.unix_timestamp,
            StockError::BadExpiry
        );
        let pool = &mut ctx.accounts.pool;
        require!(pool.open_positions == 0, StockError::EpochActive);
        // the epoch just ended must have its settlement price latched; that also
        // proves `now >= epoch_end`, which `close_epoch` already required
        require!(pool.settled_epoch == pool.epoch, StockError::EpochNotClosed);
        pool.epoch += 1;
        pool.epoch_end = epoch_end;
        Ok(())
    }

    /// Latches the epoch settlement price. Every position in the epoch settles
    /// against this one print, so the payout no longer depends on who calls
    /// `settle` or when.
    ///
    /// ponytail: permissionless, and the price is the median at the moment of the
    /// first call after `epoch_end` rather than at `epoch_end` itself. The keeper
    /// calls this on the boundary, and anyone holding a position or a share can
    /// call it if the keeper stalls, so the worst case is a print a few blocks
    /// late off a 16-sample median. Move to a keeper-only close over a fixed
    /// post-expiry sampling window if that drift ever becomes worth gaming.
    pub fn close_epoch(ctx: Context<CloseEpoch>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let pool = &mut ctx.accounts.pool;
        require!(
            pool.settled_epoch < pool.epoch,
            StockError::EpochAlreadyClosed
        );
        require!(now >= pool.epoch_end, StockError::EpochNotEnded);
        pool.settle_price = median(&ctx.accounts.oracle, now)?;
        pool.settled_epoch = pool.epoch;
        Ok(())
    }

    pub fn init_oracle(ctx: Context<InitOracle>, keeper: Pubkey) -> Result<()> {
        let o = &mut ctx.accounts.oracle;
        o.keeper = keeper;
        o.underlying = ctx.accounts.underlying.key();
        o.samples = [0u64; SAMPLES];
        o.count = 0;
        o.idx = 0;
        o.last_update = 0;
        Ok(())
    }

    pub fn push_price(ctx: Context<PushPrice>, price: u64) -> Result<()> {
        require!(price > 0, StockError::BadPrice);
        let o = &mut ctx.accounts.oracle;
        let i = o.idx as usize;
        o.samples[i] = price;
        o.idx = (o.idx + 1) % SAMPLES as u8;
        if (o.count as usize) < SAMPLES {
            o.count += 1;
        }
        o.last_update = Clock::get()?.unix_timestamp;
        Ok(())
    }

    /// ponytail: LPs may only enter or exit while the pool carries no open
    /// position, gated on `open_positions` rather than on `locked`. `locked` is a
    /// proxy that lies — `required_collateral` floors to 0 for a dust-sized put,
    /// so an open position can sit at `locked == 0` and let a depositor in ahead
    /// of a payout. Holding the gate shut for the whole life of an epoch's
    /// positions is what keeps the mint/redeem price fixed between boundaries:
    /// premiums land in `available` at write time, so NAV does drift mid-epoch,
    /// but no share can be minted or burned against the drifted number. Add a
    /// pending-deposit queue credited at the next `close_epoch` if LPs ever need
    /// to subscribe mid-epoch.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, StockError::ZeroAmount);
        let pool = &mut ctx.accounts.pool;
        require!(pool.open_positions == 0, StockError::EpochActive);
        let assets = pool.assets()?;
        let shares = if pool.total_shares == 0 {
            amount
        } else {
            mul_div(amount, pool.total_shares, assets)?
        };
        require!(shares > 0, StockError::ZeroAmount);

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_token.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
        )?;

        pool.available = add(pool.available, amount)?;
        pool.total_shares = add(pool.total_shares, shares)?;
        let lp = &mut ctx.accounts.lp;
        lp.owner = ctx.accounts.user.key();
        lp.pool = pool.key();
        lp.shares = add(lp.shares, shares)?;
        Ok(())
    }

    pub fn withdraw(ctx: Context<Withdraw>, shares: u64) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let lp = &mut ctx.accounts.lp;
        // same gate as `deposit`, same reason — see the ponytail there
        require!(pool.open_positions == 0, StockError::EpochActive);
        require!(shares > 0 && shares <= lp.shares, StockError::ZeroAmount);
        let amount = mul_div(shares, pool.assets()?, pool.total_shares)?;
        require!(amount <= pool.available, StockError::InsufficientAvailable);

        pool.available -= amount;
        pool.total_shares -= shares;
        lp.shares -= shares;

        let seeds = pool_seeds!(pool);
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.user_token.to_account_info(),
                    authority: pool.to_account_info(),
                },
                &[&seeds[..]],
            ),
            amount,
        )?;
        Ok(())
    }

    /// Quote validity comes from the backend co-signing the transaction.
    /// ponytail: admin-signer quotes; move to ed25519 sysvar verification if quotes get relayed.
    pub fn buy_option(
        ctx: Context<BuyOption>,
        id: u64,
        strike: u64,
        size: u64,
        premium: u64,
        quote_expiry: i64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let expiry = ctx.accounts.pool.epoch_end;
        require!(now <= quote_expiry, StockError::QuoteExpired);
        require!(
            quote_expiry <= now + MAX_QUOTE_TTL,
            StockError::QuoteTtlTooLong
        );
        require!(now < expiry, StockError::EpochClosed);
        require!(strike > 0 && size > 0 && premium > 0, StockError::ZeroAmount);

        // an option is never worth less than its intrinsic value, whatever the backend signs
        let spot = median(&ctx.accounts.oracle, now)?;
        require!(
            premium >= payoff(ctx.accounts.pool.kind, strike, size, spot)?,
            StockError::PremiumBelowIntrinsic
        );

        let pool = &mut ctx.accounts.pool;
        let collateral = required_collateral(pool.kind, strike, size)?;
        require!(collateral <= pool.available, StockError::InsufficientAvailable);

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.buyer_token.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.buyer.to_account_info(),
                },
            ),
            premium,
        )?;

        pool.available = add(pool.available, premium)? - collateral;
        pool.locked = add(pool.locked, collateral)?;
        pool.open_positions = pool
            .open_positions
            .checked_add(1)
            .ok_or(StockError::MathOverflow)?;

        ctx.accounts.position.set_inner(OptionPosition {
            owner: ctx.accounts.buyer.key(),
            pool: pool.key(),
            id,
            kind: pool.kind,
            strike,
            size,
            expiry,
            epoch: pool.epoch,
            collateral,
            premium,
            payout: 0,
            settled: false,
            bump: ctx.bumps.position,
        });
        Ok(())
    }

    /// Pays a position out against the price its own epoch latched, not against
    /// a live median, so every position written in an epoch settles at the same
    /// print whoever calls this and whenever they call it.
    ///
    /// ponytail: permissionless settle, deliberately with no admin force-settle
    /// beside it. Anyone can settle anyone's expired position and
    /// `pool.open_positions` tells a keeper how many are outstanding, so the
    /// epoch always drains and the counter is the only thing holding the next
    /// `roll_epoch`. Add an admin sweep that writes a position off and
    /// decrements the counter if one can ever become unsettleable.
    pub fn settle(ctx: Context<Settle>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let pos = &mut ctx.accounts.position;
        // two checks, not one: above the latched epoch means the position's own
        // epoch has no settlement price yet, below it means the position is a
        // straggler from an earlier epoch and must never take this price
        require!(pos.epoch <= pool.settled_epoch, StockError::EpochNotClosed);
        require!(pos.epoch == pool.settled_epoch, StockError::EpochMismatch);
        require!(!pos.settled, StockError::AlreadySettled);
        require!(
            Clock::get()?.unix_timestamp >= pos.expiry,
            StockError::NotExpired
        );

        let payout =
            payoff(pos.kind, pos.strike, pos.size, pool.settle_price)?.min(pos.collateral);

        pool.locked -= pos.collateral;
        pool.available = add(pool.available, pos.collateral - payout)?;
        pool.open_positions = pool
            .open_positions
            .checked_sub(1)
            .ok_or(StockError::MathOverflow)?;

        pos.payout = payout;
        pos.settled = true;
        Ok(())
    }

    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        let payout = ctx.accounts.position.payout;
        require!(ctx.accounts.position.settled, StockError::NotSettled);
        if payout > 0 {
            let pool = &ctx.accounts.pool;
            let seeds = pool_seeds!(pool);
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault.to_account_info(),
                        to: ctx.accounts.owner_token.to_account_info(),
                        authority: pool.to_account_info(),
                    },
                    &[&seeds[..]],
                ),
                payout,
            )?;
        }
        Ok(())
    }
}

pub fn required_collateral(kind: u8, strike: u64, size: u64) -> Result<u64> {
    match kind {
        KIND_PUT => u64::try_from((strike as u128 * size as u128) / SCALE)
            .map_err(|_| StockError::MathOverflow.into()),
        _ => Ok(size),
    }
}

/// Put: max(0, strike-spot) * size, paid in collateral (USDC).
/// Call: max(0, spot-strike) * size / spot, paid in the underlying.
pub fn payoff(kind: u8, strike: u64, size: u64, spot: u64) -> Result<u64> {
    let (hi, lo, denom) = if kind == KIND_PUT {
        (strike, spot, SCALE)
    } else {
        (spot, strike, spot as u128)
    };
    if lo >= hi {
        return Ok(0);
    }
    u64::try_from(((hi - lo) as u128 * size as u128) / denom)
        .map_err(|_| StockError::MathOverflow.into())
}

fn median(o: &Oracle, now: i64) -> Result<u64> {
    require!(o.count > 0, StockError::NoSamples);
    require!(
        now - o.last_update <= MAX_ORACLE_STALENESS,
        StockError::StaleOracle
    );
    let n = o.count as usize;
    let mut s = o.samples[..n].to_vec();
    s.sort_unstable();
    // ponytail: upper median on even counts, plenty for an outlier-resistant settlement price
    Ok(s[n / 2])
}

fn mul_div(a: u64, b: u64, d: u64) -> Result<u64> {
    require!(d > 0, StockError::MathOverflow);
    u64::try_from(a as u128 * b as u128 / d as u128).map_err(|_| StockError::MathOverflow.into())
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or(StockError::MathOverflow.into())
}

#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub authority: Pubkey,
    pub collateral_mint: Pubkey,
    pub vault: Pubkey,
    pub oracle: Pubkey,
    pub quote_signer: Pubkey,
    pub total_shares: u64,
    pub locked: u64,
    pub available: u64,
    pub epoch: u64,
    pub epoch_end: i64,
    /// The price every position in `settled_epoch` pays out against. Only
    /// meaningful for that epoch; stale between `roll_epoch` and `close_epoch`.
    pub settle_price: u64,
    pub settled_epoch: u64,
    /// Written and not yet settled, across every epoch. Gates `roll_epoch` and
    /// the LP flows, and is a keeper's outstanding-settle count.
    pub open_positions: u32,
    pub kind: u8,
    pub bump: u8,
}

impl Pool {
    fn assets(&self) -> Result<u64> {
        add(self.available, self.locked)
    }
}

#[account]
#[derive(InitSpace)]
pub struct Oracle {
    pub keeper: Pubkey,
    pub underlying: Pubkey,
    pub samples: [u64; SAMPLES],
    pub last_update: i64,
    pub count: u8,
    pub idx: u8,
}

#[account]
#[derive(InitSpace)]
pub struct LpPosition {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub shares: u64,
}

#[account]
#[derive(InitSpace)]
pub struct OptionPosition {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub id: u64,
    pub strike: u64,
    pub size: u64,
    pub expiry: i64,
    /// The epoch this was written in; `settle` only accepts the price latched
    /// for exactly this epoch.
    pub epoch: u64,
    pub collateral: u64,
    pub premium: u64,
    pub payout: u64,
    pub kind: u8,
    pub settled: bool,
    pub bump: u8,
}

#[derive(Accounts)]
#[instruction(kind: u8)]
pub struct InitPool<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    pub collateral_mint: Account<'info, Mint>,
    pub oracle: Account<'info, Oracle>,
    #[account(
        init,
        payer = admin,
        space = 8 + Pool::INIT_SPACE,
        seeds = [b"pool", collateral_mint.key().as_ref(), &[kind]],
        bump
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        init,
        payer = admin,
        token::mint = collateral_mint,
        token::authority = pool,
        seeds = [b"vault", pool.key().as_ref()],
        bump
    )]
    pub vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct InitOracle<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    pub underlying: Account<'info, Mint>,
    #[account(
        init,
        payer = admin,
        space = 8 + Oracle::INIT_SPACE,
        seeds = [b"oracle", underlying.key().as_ref()],
        bump
    )]
    pub oracle: Account<'info, Oracle>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RollEpoch<'info> {
    #[account(mut, has_one = authority)]
    pub pool: Account<'info, Pool>,
    pub authority: Signer<'info>,
}

/// No signer of its own: `close_epoch` is permissionless, the transaction fee
/// payer is the only signature it needs.
#[derive(Accounts)]
pub struct CloseEpoch<'info> {
    #[account(mut, has_one = oracle)]
    pub pool: Account<'info, Pool>,
    pub oracle: Account<'info, Oracle>,
}

#[derive(Accounts)]
pub struct PushPrice<'info> {
    #[account(mut, has_one = keeper @ StockError::NotKeeper)]
    pub oracle: Account<'info, Oracle>,
    pub keeper: Signer<'info>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, has_one = vault)]
    pub pool: Account<'info, Pool>,
    #[account(mut)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint = pool.collateral_mint, token::authority = user)]
    pub user_token: Account<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = user,
        space = 8 + LpPosition::INIT_SPACE,
        seeds = [b"lp", pool.key().as_ref(), user.key().as_ref()],
        bump
    )]
    pub lp: Account<'info, LpPosition>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    pub user: Signer<'info>,
    #[account(mut, has_one = vault)]
    pub pool: Account<'info, Pool>,
    #[account(mut)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint = pool.collateral_mint, token::authority = user)]
    pub user_token: Account<'info, TokenAccount>,
    #[account(
        mut,
        seeds = [b"lp", pool.key().as_ref(), user.key().as_ref()],
        bump,
        constraint = lp.owner == user.key() @ StockError::NotOwner
    )]
    pub lp: Account<'info, LpPosition>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(id: u64)]
pub struct BuyOption<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,
    #[account(address = pool.quote_signer @ StockError::BadQuoteSigner)]
    pub quote_signer: Signer<'info>,
    #[account(mut, has_one = vault, has_one = oracle)]
    pub pool: Account<'info, Pool>,
    pub oracle: Account<'info, Oracle>,
    #[account(mut)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint = pool.collateral_mint, token::authority = buyer)]
    pub buyer_token: Account<'info, TokenAccount>,
    #[account(
        init,
        payer = buyer,
        space = 8 + OptionPosition::INIT_SPACE,
        seeds = [b"opt", pool.key().as_ref(), buyer.key().as_ref(), &id.to_le_bytes()],
        bump
    )]
    pub position: Account<'info, OptionPosition>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(mut)]
    pub pool: Account<'info, Pool>,
    #[account(mut, has_one = pool)]
    pub position: Account<'info, OptionPosition>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(has_one = vault)]
    pub pool: Account<'info, Pool>,
    #[account(mut)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint = pool.collateral_mint, token::authority = owner)]
    pub owner_token: Account<'info, TokenAccount>,
    #[account(mut, has_one = pool, has_one = owner, close = owner)]
    pub position: Account<'info, OptionPosition>,
    pub token_program: Program<'info, Token>,
}

#[error_code]
pub enum StockError {
    #[msg("math overflow")]
    MathOverflow,
    #[msg("amount must be non-zero")]
    ZeroAmount,
    #[msg("not enough available collateral")]
    InsufficientAvailable,
    #[msg("signer is not the oracle keeper")]
    NotKeeper,
    #[msg("quote expired")]
    QuoteExpired,
    #[msg("bad quote signer")]
    BadQuoteSigner,
    #[msg("premium is below the option intrinsic value")]
    PremiumBelowIntrinsic,
    #[msg("epoch has open positions")]
    EpochActive,
    #[msg("epoch is closed for new options")]
    EpochClosed,
    #[msg("expiry must be in the future")]
    BadExpiry,
    #[msg("option has not expired")]
    NotExpired,
    #[msg("already settled")]
    AlreadySettled,
    #[msg("not settled")]
    NotSettled,
    #[msg("oracle has no samples")]
    NoSamples,
    #[msg("bad option kind")]
    BadKind,
    #[msg("bad price")]
    BadPrice,
    #[msg("not the owner")]
    NotOwner,
    #[msg("oracle samples are stale")]
    StaleOracle,
    #[msg("quote expiry is too far in the future")]
    QuoteTtlTooLong,
    #[msg("oracle underlying does not match the pool collateral mint")]
    OracleMintMismatch,
    #[msg("position belongs to a different epoch")]
    EpochMismatch,
    #[msg("epoch has not been closed")]
    EpochNotClosed,
    #[msg("epoch settlement price is already latched")]
    EpochAlreadyClosed,
    #[msg("epoch has not reached its end")]
    EpochNotEnded,
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    #[test]
    fn put_payoff() {
        assert_eq!(payoff(KIND_PUT, 220 * S, S, 200 * S).unwrap(), 20 * S);
        assert_eq!(payoff(KIND_PUT, 220 * S, S, 220 * S).unwrap(), 0);
        assert_eq!(payoff(KIND_PUT, 220 * S, S, 250 * S).unwrap(), 0);
        assert_eq!(required_collateral(KIND_PUT, 220 * S, S).unwrap(), 220 * S);
    }

    #[test]
    fn call_payoff() {
        assert_eq!(payoff(KIND_CALL, 200 * S, S, 250 * S).unwrap(), 200_000);
        assert_eq!(payoff(KIND_CALL, 200 * S, S, 200 * S).unwrap(), 0);
        assert_eq!(payoff(KIND_CALL, 200 * S, S, 150 * S).unwrap(), 0);
        assert_eq!(required_collateral(KIND_CALL, 200 * S, S).unwrap(), S);
    }

    #[test]
    fn call_payoff_zero_at_or_below_strike() {
        assert_eq!(payoff(KIND_CALL, 100 * S, S, 50 * S).unwrap(), 0);
        assert_eq!(payoff(KIND_CALL, 100 * S, S, 100 * S).unwrap(), 0);
    }

    #[test]
    fn call_payoff_bounded_by_size_when_deep_itm() {
        let p = payoff(KIND_CALL, 100 * S, S, 1000 * S).unwrap();
        assert!(p < S, "deep ITM call payoff {p} should be below size {S}");
    }

    #[test]
    fn collateral_requirements() {
        assert_eq!(required_collateral(KIND_PUT, 100 * S, S).unwrap(), 100 * S);
        assert_eq!(required_collateral(KIND_CALL, 100 * S, S).unwrap(), S);
    }
}
