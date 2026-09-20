use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("86kDc93MAkfLm3JNi43KYcFPiKZ765tYHPTekxmp1Ukb");

/// Every price, strike and size is fixed-point 1e6.
/// size 1_000_000 == 1 contract == 1 underlying token (underlying mint must have 6 decimals).
pub const SCALE: u128 = 1_000_000;
pub const SAMPLES: usize = 32;
pub const KIND_PUT: u8 = 0;
pub const KIND_CALL: u8 = 1;
pub const MAX_QUOTE_TTL: i64 = 300;
pub const MAX_ORACLE_STALENESS: i64 = 900;
pub const BPS: u64 = 10_000;
/// A single push may not land more than 10% away from the ring median.
pub const MAX_DEV_BPS: u64 = 1_000;
/// Settlement reads the samples in the 30 minutes ENDING at `epoch_end`.
pub const SETTLEMENT_WINDOW: i64 = 1_800;
/// Below this many in-window samples an epoch refuses to latch a price.
pub const MIN_SETTLEMENT_SAMPLES: usize = 8;
/// Floor on the deposit that sets the share scale at genesis.
pub const MIN_FIRST_DEPOSIT: u64 = 1_000_000;
/// The shortest gap between two accepted pushes. 32 slots at this cadence span
/// at least 16 minutes, so the ring cannot be stuffed with samples all taken
/// inside one settlement window.
pub const MIN_PUSH_INTERVAL: i64 = 30;
/// The shortest span a settlement window's samples must cover. Defense in depth
/// behind `MIN_PUSH_INTERVAL`.
pub const MIN_SETTLEMENT_SPAN: i64 = 900;
/// Floor on a quote's premium, in bps of the position's collateral.
pub const MIN_PREMIUM_BPS: u64 = 10;

/// Where the pool is in the epoch cycle. `Genesis` is a pool that has never
/// opened an epoch, and it is a state in its own right rather than epoch 0
/// dressed up as a round that settled at a price of zero.
///
/// ponytail: three states, with no `Paused` and no `Voided`. The transitions are
/// a closed cycle — `Genesis | Closed -> Open -> Closed` — and every instruction
/// is reachable from exactly one of them, so a fourth variant would today buy
/// nothing but unreachable code. `Voided` is the variant to add if the
/// round-cannot-close tail case described in `close_epoch` ever needs the remedy
/// that ponytail names: it wants a round that is finished without ever having
/// had a price, which is precisely what none of these three can express.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum EpochState {
    Genesis,
    Open,
    Closed,
}

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

    pub fn init_pool(
        ctx: Context<InitPool>,
        kind: u8,
        quote_signer: Pubkey,
        keeper: Pubkey,
    ) -> Result<()> {
        require!(kind <= KIND_CALL, StockError::BadKind);
        require!(
            ctx.accounts.oracle.underlying == ctx.accounts.underlying.key(),
            StockError::OracleMintMismatch
        );
        
        if kind == KIND_CALL {
            require!(
                ctx.accounts.collateral_mint.key() == ctx.accounts.underlying.key(),
                StockError::OracleMintMismatch
            );
        } else {
            require!(
                ctx.accounts.collateral_mint.key() != ctx.accounts.underlying.key(),
                StockError::OracleMintMismatch
            );
        }
        let pool = &mut ctx.accounts.pool;
        pool.authority = ctx.accounts.admin.key();
        pool.keeper = keeper;
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
        pool.state = EpochState::Genesis;
        pool.open_positions = 0;
        pool.epoch_premium = 0;
        pool.epoch_collateral = 0;
        pool.epoch_positions = 0;
        pool.share_price_open = 0;
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
        // a `Genesis` pool has no epoch to close; anything else must have had its
        // settlement price latched, which also proves `now >= epoch_end` because
        // `close_epoch` already required it
        require!(pool.state != EpochState::Open, StockError::EpochNotClosed);
        pool.epoch += 1;
        pool.epoch_end = epoch_end;
        pool.state = EpochState::Open;
        pool.epoch_premium = 0;
        pool.epoch_collateral = 0;
        pool.epoch_positions = 0;

        pool.share_price_open = pool.share_price()?;
        Ok(())
    }

    pub fn close_epoch(ctx: Context<CloseEpoch>) -> Result<()> {
        let clock = Clock::get()?;
        let pool = &mut ctx.accounts.pool;
        require!(
            clock.unix_timestamp >= pool.epoch_end,
            StockError::EpochNotEnded
        );

        let window_start = pool.epoch_end - SETTLEMENT_WINDOW;
        let (price, sample_count) =
            window_median(&ctx.accounts.oracle, window_start, pool.epoch_end)?;
        pool.settle_price = price;
        pool.state = EpochState::Closed;

        ctx.accounts.epoch_record.set_inner(EpochRecord {
            epoch: pool.epoch,
            epoch_end: pool.epoch_end,
            settle_price: price,
            settle_slot: clock.slot,
            settle_ts: clock.unix_timestamp,
            window_start,
            window_end: pool.epoch_end,
            sample_count,
            total_premium: pool.epoch_premium,
            total_collateral: pool.epoch_collateral,
            total_payout: 0,
            positions_written: pool.epoch_positions,
            positions_settled: 0,
            share_price_open: pool.share_price_open,
            share_price_close: pool.share_price()?,
            bump: ctx.bumps.epoch_record,
        });

        emit!(CloseEpochEvent {
            pool: pool.key(),
            epoch: pool.epoch,
            settle_price: price,
            slot: clock.slot,
            sample_count,
        });
        Ok(())
    }

    pub fn init_oracle(ctx: Context<InitOracle>, keeper: Pubkey) -> Result<()> {
        let o = &mut ctx.accounts.oracle;
        o.keeper = keeper;
        o.underlying = ctx.accounts.underlying.key();
        o.samples = [0u64; SAMPLES];
        o.sample_ts = [0i64; SAMPLES];
        o.count = 0;
        o.idx = 0;
        o.last_update = 0;
        Ok(())
    }

    pub fn push_price(ctx: Context<PushPrice>, price: u64) -> Result<()> {
        require!(price > 0, StockError::BadPrice);
        let o = &mut ctx.accounts.oracle;
        // skipped while the ring is empty: the first sample has nothing to deviate from
        if let Some(med) = ring_median(o) {
            require!(within_deviation(price, med), StockError::OracleDeviation);
        }
        let ts = Clock::get()?.unix_timestamp;
        require!(
            ts - o.last_update >= MIN_PUSH_INTERVAL,
            StockError::PushTooSoon
        );
        let i = o.idx as usize;
        o.samples[i] = price;
        o.sample_ts[i] = ts;
        o.idx = (o.idx + 1) % SAMPLES as u8;
        if (o.count as usize) < SAMPLES {
            o.count += 1;
        }
        o.last_update = ts;
        emit!(PushPriceEvent {
            oracle: o.key(),
            price,
            ts,
            count: o.count,
        });
        Ok(())
    }

    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, StockError::ZeroAmount);
        let pool = &mut ctx.accounts.pool;
        require!(pool.open_positions == 0, StockError::EpochActive);
        let assets = pool.assets()?;
        let shares = if pool.total_shares == 0 {
            require!(amount >= MIN_FIRST_DEPOSIT, StockError::FirstDepositTooSmall);
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
        emit!(DepositEvent {
            pool: pool.key(),
            epoch: pool.epoch,
            user: lp.owner,
            amount,
            shares,
        });
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
        emit!(WithdrawEvent {
            pool: pool.key(),
            epoch: pool.epoch,
            user: lp.owner,
            amount,
            shares,
        });
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
        require_writable(&ctx.accounts.pool, now, quote_expiry)?;
        require!(strike > 0 && size > 0 && premium > 0, StockError::ZeroAmount);

        // an option is never worth less than its intrinsic value, whatever the backend signs
        let spot = median(&ctx.accounts.oracle, now)?;
        require!(
            premium >= payoff(ctx.accounts.pool.kind, strike, size, spot)?,
            StockError::PremiumBelowIntrinsic
        );

        let pool = &mut ctx.accounts.pool;
        let collateral = required_collateral(pool.kind, strike, size)?;
        require!(collateral > 0, StockError::ZeroCollateral);
        require!(
            premium as u128 * BPS as u128 >= collateral as u128 * MIN_PREMIUM_BPS as u128,
            StockError::PremiumTooSmall
        );
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
        pool.epoch_premium = add(pool.epoch_premium, premium)?;
        pool.epoch_collateral = add(pool.epoch_collateral, collateral)?;
        pool.epoch_positions = pool
            .epoch_positions
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
        emit!(BuyOptionEvent {
            pool: pool.key(),
            epoch: pool.epoch,
            id,
            strike,
            size,
            premium,
            collateral,
        });
        Ok(())
    }

    pub fn settle(ctx: Context<Settle>) -> Result<()> {
        let pool_key = ctx.accounts.pool.key();
        let position_key = ctx.accounts.position.key();
        let pool = &mut ctx.accounts.pool;
        let pos = &mut ctx.accounts.position;
        // two checks, not one, answering two different questions: the state says
        // whether `pool.settle_price` is a latched price at all, and the epoch
        // equality says whether it is THIS position's price rather than one
        // belonging to a round the pool has already moved past
        require!(pool.state == EpochState::Closed, StockError::EpochNotClosed);
        require!(pos.epoch == pool.epoch, StockError::EpochMismatch);
        require!(
            Clock::get()?.unix_timestamp >= pos.expiry,
            StockError::NotExpired
        );

        let price = pool.settle_price;
        let payout = write_off(pool, pos, &mut ctx.accounts.epoch_record, price)?;
        emit!(SettleEvent {
            pool: pool_key,
            epoch: pos.epoch,
            position: position_key,
            payout,
        });
        Ok(())
    }


    pub fn force_settle(ctx: Context<ForceSettle>) -> Result<()> {
        let pool_key = ctx.accounts.pool.key();
        let position_key = ctx.accounts.position.key();
        let price = ctx.accounts.epoch_record.settle_price;
        let epoch = ctx.accounts.position.epoch;
        let payout = write_off(
            &mut ctx.accounts.pool,
            &mut ctx.accounts.position,
            &mut ctx.accounts.epoch_record,
            price,
        )?;
        emit!(ForceSettleEvent {
            pool: pool_key,
            epoch,
            position: position_key,
            payout,
        });
        Ok(())
    }

    /// Deliberately epoch-agnostic: `payout` is computed and frozen at settle
    /// time, `settled` is single-shot, and `has_one = pool` already binds the
    /// position to the vault that holds its money. An epoch check here (e.g.
    /// `position.epoch == pool.epoch`) would permanently strand a
    /// settled-but-unclaimed position the moment the pool rolled on: by the time
    /// the buyer came back, `pool.epoch` would have moved on and the claim could
    /// never succeed. Payouts must survive a roll, so this stays as
    /// is.
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
        emit!(ClaimEvent {
            pool: ctx.accounts.pool.key(),
            epoch: ctx.accounts.pool.epoch,
            owner: ctx.accounts.owner.key(),
            payout,
        });
        Ok(())
    }
}

/// Everything that has to hold about the quote and about the epoch before a
/// position can be written into the pool at all.
///
/// The three epoch checks are ordered and carry distinct errors deliberately,
/// for the same reason `CloseEpoch` orders its constraints: a pool that has
/// never opened an epoch, a pool whose epoch has already settled, and an epoch
/// that has merely run out of time are three different things to tell a buyer,
/// and one shared message sends whoever is debugging a quote to the wrong place.
/// `state` answers whether this epoch is writable at all; `now < epoch_end`
/// answers whether there is any time left in it. The state checks come first
/// because a `Genesis` pool has `epoch_end == 0` and so fails the time check
/// too — ordered the other way it would report having run out of time for an
/// epoch it never started.
fn require_writable(pool: &Pool, now: i64, quote_expiry: i64) -> Result<()> {
    require!(now <= quote_expiry, StockError::QuoteExpired);
    require!(
        quote_expiry <= now + MAX_QUOTE_TTL,
        StockError::QuoteTtlTooLong
    );
    require!(
        pool.state != EpochState::Genesis,
        StockError::EpochNotStarted
    );
    require!(pool.state == EpochState::Open, StockError::EpochClosed);
    require!(now < pool.epoch_end, StockError::EpochExpired);
    Ok(())
}

/// Shared body of `settle` and `force_settle`: writes `pos` off against an
/// already-latched `price`, returns its collateral to `available` and books the
/// result on the round's record. Neither caller supplies `price` — one reads the
/// pool's latch, the other the epoch record's, and there is no third way in.
fn write_off(
    pool: &mut Pool,
    pos: &mut OptionPosition,
    record: &mut EpochRecord,
    price: u64,
) -> Result<u64> {
    require!(!pos.settled, StockError::AlreadySettled);
    let payout = payoff(pos.kind, pos.strike, pos.size, price)?.min(pos.collateral);

    pool.locked -= pos.collateral;
    pool.available = add(pool.available, pos.collateral - payout)?;
    pool.open_positions = pool
        .open_positions
        .checked_sub(1)
        .ok_or(StockError::MathOverflow)?;

    pos.payout = payout;
    pos.settled = true;

    record.total_payout = add(record.total_payout, payout)?;
    record.positions_settled = record
        .positions_settled
        .checked_add(1)
        .ok_or(StockError::MathOverflow)?;
    record.share_price_close = pool.share_price()?;
    Ok(payout)
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

/// True when `price` sits inside the +/-MAX_DEV_BPS band around `med`.
fn within_deviation(price: u64, med: u64) -> bool {
    (price.abs_diff(med) as u128) * (BPS as u128) <= (med as u128) * (MAX_DEV_BPS as u128)
}


fn median_of(buf: &mut [u64; SAMPLES], n: usize) -> Option<u64> {
    if n == 0 {
        return None;
    }
    buf[..n].sort_unstable();
    // ponytail: upper median on even counts, plenty for an outlier-resistant settlement price
    Some(buf[n / 2])
}

/// Median of every filled slot, ignoring timestamps. Deliberately has no
/// staleness check: it is what a new push is measured against, and a stale
/// oracle has to stay pushable or the deviation guard would brick it.
fn ring_median(o: &Oracle) -> Option<u64> {
    let n = o.count as usize;
    let mut buf = [0u64; SAMPLES];
    buf[..n].copy_from_slice(&o.samples[..n]);
    median_of(&mut buf, n)
}

/// Median of the samples timestamped in `start..=end`, with the count that
/// produced it. Rejects a window too thin to be a settlement price rather than
/// latching the median of a few stragglers.
fn window_median(o: &Oracle, start: i64, end: i64) -> Result<(u64, u8)> {
    let mut buf = [0u64; SAMPLES];
    let mut n = 0usize;
    let mut oldest = i64::MAX;
    let mut newest = i64::MIN;
    for (&price, &ts) in o
        .samples
        .iter()
        .zip(o.sample_ts.iter())
        .take(o.count as usize)
    {
        if (start..=end).contains(&ts) {
            buf[n] = price;
            n += 1;
            oldest = oldest.min(ts);
            newest = newest.max(ts);
        }
    }
    require!(
        n >= MIN_SETTLEMENT_SAMPLES,
        StockError::InsufficientSamples
    );
    require!(
        newest - oldest >= MIN_SETTLEMENT_SPAN,
        StockError::SettlementWindowTooShort
    );
    let price = median_of(&mut buf, n).ok_or(StockError::NoSamples)?;
    Ok((price, n as u8))
}

fn median(o: &Oracle, now: i64) -> Result<u64> {
    let m = ring_median(o).ok_or(StockError::NoSamples)?;
    require!(
        now - o.last_update <= MAX_ORACLE_STALENESS,
        StockError::StaleOracle
    );
    Ok(m)
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
    /// Runs the routine epoch machinery: `close_epoch` and `settle`. The
    /// authority can do both as well, and only the authority can `force_settle`.
    pub keeper: Pubkey,
    pub collateral_mint: Pubkey,
    pub vault: Pubkey,
    pub oracle: Pubkey,
    pub quote_signer: Pubkey,
    pub total_shares: u64,
    pub locked: u64,
    pub available: u64,
    pub epoch: u64,
    pub epoch_end: i64,
    /// The price every position in `epoch` pays out against. Only meaningful
    /// while `state == Closed`; stale between `roll_epoch` and `close_epoch`.
    pub settle_price: u64,
    pub state: EpochState,
    /// Written and not yet settled, across every epoch. Gates `roll_epoch` and
    /// the LP flows, and is a keeper's outstanding-settle count.
    pub open_positions: u32,
    /// Running totals for the CURRENT epoch, zeroed at every `roll_epoch` and
    /// copied onto the `EpochRecord` at `close_epoch`.
    pub epoch_premium: u64,
    pub epoch_collateral: u64,
    pub epoch_positions: u32,
    /// Share price stamped at the roll that opened the current epoch.
    pub share_price_open: u64,
    pub kind: u8,
    pub bump: u8,
}

impl Pool {
    /// LOAD-BEARING INVARIANT — share price derives ONLY from `available + locked`,
    /// and both fields move exclusively inside program instructions
    /// (`deposit`/`withdraw`/`buy_option`/`settle`). The vault's on-chain token
    /// `amount` is never read back, so a raw SPL transfer into the vault bumps the
    /// token balance without touching NAV: it is a donation to the LPs, not a
    /// share-price move. That asymmetry is exactly what makes the ERC4626
    /// donation-inflation attack impossible here — a first depositor cannot
    /// inflate the share price by transferring tokens in before someone else
    /// mints. Do NOT add a "sync"/"skim" instruction that folds the vault's
    /// `amount` into `available`: that reintroduces the attacker-controlled
    /// denominator and enables the attack immediately.
    fn assets(&self) -> Result<u64> {
        add(self.available, self.locked)
    }

    /// NAV per share, 1e6 scale. An empty pool reports par: there is no NAV to
    /// divide, and par is the price the next genesis deposit will mint at.
    fn share_price(&self) -> Result<u64> {
        if self.total_shares == 0 {
            return Ok(SCALE as u64);
        }
        mul_div(self.assets()?, SCALE as u64, self.total_shares)
    }

    pub fn is_operator(&self, signer: Pubkey) -> bool {
        signer == self.keeper || signer == self.authority
    }
}

#[account]
#[derive(InitSpace)]
pub struct Oracle {
    pub keeper: Pubkey,
    pub underlying: Pubkey,
    pub samples: [u64; SAMPLES],
    /// On-chain clock at the moment each sample landed, parallel to `samples`.
    /// This is what makes a settlement window provable rather than assumed.
    pub sample_ts: [i64; SAMPLES],
    pub last_update: i64,
    pub count: u8,
    pub idx: u8,
}

/// Permanent, per-round proof of what settlement actually did. `Pool.settle_price`
/// is overwritten by the next close and transaction logs get pruned by RPCs, so
/// account state is the only durable place a user can be shown the number their
/// option paid out against.
#[account]
#[derive(InitSpace)]
pub struct EpochRecord {
    pub epoch: u64,
    pub epoch_end: i64,
    pub settle_price: u64,
    pub settle_slot: u64,
    pub settle_ts: i64,
    pub window_start: i64,
    pub window_end: i64,
    /// How many in-window samples produced `settle_price`. A thin or degraded
    /// settlement is visible here after the fact instead of being silent.
    pub sample_count: u8,
    pub total_premium: u64,
    pub total_collateral: u64,
    /// Final only once `positions_settled == positions_written`.
    pub total_payout: u64,
    pub positions_written: u32,
    pub positions_settled: u32,
    pub share_price_open: u64,
    /// Rewritten by every settle; the round's true close is the value it holds
    /// once `positions_settled == positions_written`.
    pub share_price_close: u64,
    pub bump: u8,
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

#[event]
pub struct DepositEvent {
    pub pool: Pubkey,
    pub epoch: u64,
    pub user: Pubkey,
    pub amount: u64,
    pub shares: u64,
}

#[event]
pub struct WithdrawEvent {
    pub pool: Pubkey,
    pub epoch: u64,
    pub user: Pubkey,
    pub amount: u64,
    pub shares: u64,
}

#[event]
pub struct BuyOptionEvent {
    pub pool: Pubkey,
    pub epoch: u64,
    pub id: u64,
    pub strike: u64,
    pub size: u64,
    pub premium: u64,
    pub collateral: u64,
}

#[event]
pub struct ClaimEvent {
    pub pool: Pubkey,
    pub epoch: u64,
    pub owner: Pubkey,
    pub payout: u64,
}

#[event]
pub struct PushPriceEvent {
    pub oracle: Pubkey,
    pub price: u64,
    pub ts: i64,
    pub count: u8,
}

#[event]
pub struct CloseEpochEvent {
    pub pool: Pubkey,
    pub epoch: u64,
    pub settle_price: u64,
    pub slot: u64,
    pub sample_count: u8,
}

#[event]
pub struct SettleEvent {
    pub pool: Pubkey,
    pub epoch: u64,
    pub position: Pubkey,
    pub payout: u64,
}

#[event]
pub struct ForceSettleEvent {
    pub pool: Pubkey,
    pub epoch: u64,
    pub position: Pubkey,
    pub payout: u64,
}

#[derive(Accounts)]
#[instruction(kind: u8)]
pub struct InitPool<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    pub collateral_mint: Account<'info, Mint>,
    /// The asset the strikes refer to. Passed explicitly rather than inferred
    /// from the collateral mint so the admin has to name the preStock this pool
    /// prices, instead of a wrong oracle silently agreeing with a wrong mint.
    pub underlying: Account<'info, Mint>,
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

/// `operator` is declared first on purpose: Anchor validates fields in
/// declaration order, so the pool's own constraints can name it, and the
/// already-closed check therefore reports `EpochAlreadyClosed` rather than the
/// account-already-in-use failure the `epoch_record` init would otherwise raise
/// first on a duplicate close.
#[derive(Accounts)]
pub struct CloseEpoch<'info> {
    #[account(mut)]
    pub operator: Signer<'info>,
    #[account(
        mut,
        has_one = oracle,
        // ordered: raw constraints are checked in declaration order, so a pool
        // that has never opened an epoch says so instead of claiming to have
        // already closed one
        constraint = pool.state != EpochState::Genesis @ StockError::EpochNotStarted,
        constraint = pool.state == EpochState::Open @ StockError::EpochAlreadyClosed,
        constraint = pool.is_operator(operator.key()) @ StockError::NotKeeper
    )]
    pub pool: Account<'info, Pool>,
    pub oracle: Account<'info, Oracle>,
    // ponytail: `init_if_needed`, with the record's write-once-ness enforced by
    // the `EpochAlreadyClosed` constraint above rather than by the runtime's
    // create-once. Anchor generates every `init` field ahead of every other
    // access check (anchor-syn `generate_constraints`), so a plain `init` would
    // make a duplicate close fail inside the system program with "account
    // already in use" and bury the reason — on a settlement path the operator
    // can genuinely race into, the specific error is worth more than the
    // structural guarantee. The guarantee still holds: `close_epoch` is the only
    // writer, it requires `state == Open`, and the only way back to `Open` is
    // `roll_epoch`, which increments `epoch` — so no epoch number is ever
    // presented twice. Go back to `init` if `Pool.epoch` ever stops being
    // monotonic.
    #[account(
        init_if_needed,
        payer = operator,
        space = 8 + EpochRecord::INIT_SPACE,
        seeds = [b"epoch", pool.key().as_ref(), &pool.epoch.to_le_bytes()],
        bump
    )]
    pub epoch_record: Account<'info, EpochRecord>,
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
    pub operator: Signer<'info>,
    #[account(mut, constraint = pool.is_operator(operator.key()) @ StockError::NotKeeper)]
    pub pool: Account<'info, Pool>,
    #[account(mut, has_one = pool)]
    pub position: Account<'info, OptionPosition>,
    #[account(
        mut,
        seeds = [b"epoch", pool.key().as_ref(), &position.epoch.to_le_bytes()],
        bump = epoch_record.bump
    )]
    pub epoch_record: Account<'info, EpochRecord>,
}

/// The record's seeds carry `position.epoch`, so the price `force_settle` reads
/// is the one that epoch latched and no other. The record existing at all is
/// what proves the epoch closed.
#[derive(Accounts)]
pub struct ForceSettle<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ StockError::NotAuthority)]
    pub pool: Account<'info, Pool>,
    #[account(mut, has_one = pool)]
    pub position: Account<'info, OptionPosition>,
    #[account(
        mut,
        seeds = [b"epoch", pool.key().as_ref(), &position.epoch.to_le_bytes()],
        bump = epoch_record.bump
    )]
    pub epoch_record: Account<'info, EpochRecord>,
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
    #[msg("first deposit must be at least one whole unit")]
    FirstDepositTooSmall,
    #[msg("price deviates too far from the oracle median")]
    OracleDeviation,
    #[msg("too few oracle samples in the settlement window")]
    InsufficientSamples,
    #[msg("signer is not the pool authority")]
    NotAuthority,
    #[msg("pool has not opened an epoch yet")]
    EpochNotStarted,
    #[msg("epoch has run out of time for new options")]
    EpochExpired,
    #[msg("oracle pushed again too soon after the last sample")]
    PushTooSoon,
    #[msg("settlement window samples do not span enough time")]
    SettlementWindowTooShort,
    #[msg("premium is too small relative to the position collateral")]
    PremiumTooSmall,
    #[msg("position requires zero collateral")]
    ZeroCollateral,
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    fn oracle_with(samples: &[(u64, i64)]) -> Oracle {
        let mut o = Oracle {
            keeper: Pubkey::default(),
            underlying: Pubkey::default(),
            samples: [0u64; SAMPLES],
            sample_ts: [0i64; SAMPLES],
            last_update: 0,
            count: 0,
            idx: 0,
        };
        for (i, &(price, ts)) in samples.iter().enumerate() {
            o.samples[i] = price;
            o.sample_ts[i] = ts;
            o.count += 1;
            o.idx = (o.idx + 1) % SAMPLES as u8;
            o.last_update = ts;
        }
        o
    }

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

    #[test]
    fn deviation_band_is_inclusive_at_ten_percent() {
        assert!(within_deviation(220 * S, 200 * S));
        assert!(within_deviation(180 * S, 200 * S));
        assert!(!within_deviation(220 * S + 1, 200 * S));
        assert!(!within_deviation(180 * S - 1, 200 * S));
    }

    #[test]
    fn settlement_window_ignores_samples_outside_it() {
        let mut s: Vec<(u64, i64)> = (0..12).map(|i| (200 * S, 1_000 + i * 100)).collect();
        s.extend((0..12).map(|i| (300 * S, 9_000 + i)));
        let (price, n) = window_median(&oracle_with(&s), 900, 2_100).unwrap();
        assert_eq!(price, 200 * S, "post-window prints must not settle the epoch");
        assert_eq!(n, 12);
    }

    #[test]
    fn settlement_window_is_boundary_anchored_not_call_anchored() {
        let end = 1_900;
        let on_time: Vec<(u64, i64)> = (0..12).map(|i| (200 * S, 1_000 + i * 100)).collect();
        // the operator stalls instead, and the feed keeps running past the
        // boundary while it does: the extra history must be worth nothing
        let mut stalled = on_time.clone();
        stalled.extend((0..12).map(|i| (300 * S, end + 1 + i)));
        assert_eq!(
            window_median(&oracle_with(&on_time), end - SETTLEMENT_WINDOW, end).unwrap(),
            window_median(&oracle_with(&stalled), end - SETTLEMENT_WINDOW, end).unwrap()
        );
    }

    #[test]
    fn settlement_refuses_a_thin_window() {
        let s: Vec<(u64, i64)> = (0..MIN_SETTLEMENT_SAMPLES as i64 - 1)
            .map(|i| (200 * S, 1_000 + i))
            .collect();
        assert!(window_median(&oracle_with(&s), 900, 1_100).is_err());
    }

    #[test]
    fn settlement_refuses_a_window_that_does_not_span_time() {
        let mut s: Vec<(u64, i64)> = (0..12).map(|i| (200 * S, 1_000 + i * 100)).collect();
        s.extend((0..12).map(|i| (300 * S, 1_016 + i)));
        // the 200s span 220s but the 300s land in a 12-second burst; the window
        // must see through the stuffing and span the full in-window range
        assert!(window_median(&oracle_with(&s), 1_000, 1_100).is_err());
    }

    #[test]
    fn a_single_wick_does_not_move_the_median() {
        let mut s: Vec<(u64, i64)> = (0..15).map(|i| (200 * S, 1_000 + i * 70)).collect();
        s.push((500 * S, 1_016));
        let (price, _) = window_median(&oracle_with(&s), 900, 2_100).unwrap();
        assert_eq!(price, 200 * S);
    }
}
