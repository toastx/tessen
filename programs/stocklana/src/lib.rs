use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("86kDc93MAkfLm3JNi43KYcFPiKZ765tYHPTekxmp1Ukb");

/// Every price, strike and size is fixed-point 1e6.
/// size 1_000_000 == 1 contract == 1 underlying token (underlying mint must have 6 decimals).
pub const SCALE: u128 = 1_000_000;
pub const SAMPLES: usize = 16;
pub const KIND_PUT: u8 = 0;
pub const KIND_CALL: u8 = 1;

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
        pool.bump = ctx.bumps.pool;
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

    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, StockError::ZeroAmount);
        let pool = &mut ctx.accounts.pool;
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
        expiry: i64,
        premium: u64,
        quote_expiry: i64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(now <= quote_expiry, StockError::QuoteExpired);
        require!(expiry > now, StockError::BadExpiry);
        require!(strike > 0 && size > 0, StockError::ZeroAmount);

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

        ctx.accounts.position.set_inner(OptionPosition {
            owner: ctx.accounts.buyer.key(),
            pool: pool.key(),
            id,
            kind: pool.kind,
            strike,
            size,
            expiry,
            collateral,
            premium,
            payout: 0,
            settled: false,
            bump: ctx.bumps.position,
        });
        Ok(())
    }

    pub fn settle(ctx: Context<Settle>) -> Result<()> {
        let pos = &mut ctx.accounts.position;
        require!(!pos.settled, StockError::AlreadySettled);
        require!(
            Clock::get()?.unix_timestamp >= pos.expiry,
            StockError::NotExpired
        );

        let spot = median(&ctx.accounts.oracle)?;
        let payout = payoff(pos.kind, pos.strike, pos.size, spot)?.min(pos.collateral);

        let pool = &mut ctx.accounts.pool;
        pool.locked -= pos.collateral;
        pool.available = add(pool.available, pos.collateral - payout)?;

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

fn median(o: &Oracle) -> Result<u64> {
    require!(o.count > 0, StockError::NoSamples);
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
    #[account(mut, has_one = vault)]
    pub pool: Account<'info, Pool>,
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
    #[account(mut, has_one = oracle)]
    pub pool: Account<'info, Pool>,
    pub oracle: Account<'info, Oracle>,
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
}
