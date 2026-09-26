//! Chain reads, option pricing, and the one thing only the backend can do:
//! co-sign a `buy_option` transaction as the pool's `quote_signer`.

use std::str::FromStr;

use anchor_lang::{AccountDeserialize, Discriminator, InstructionData, ToAccountMetas};
use base64::{engine::general_purpose::STANDARD, Engine};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

use tessen::{payoff, required_collateral, Oracle, Pool, BPS, KIND_PUT, MIN_PREMIUM_BPS, SCALE};

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 365d. The epoch is hours long; leap years are far below the oracle's noise.
const SECS_PER_YEAR: f64 = 31_536_000.0;

pub struct Chain {
    pub rpc: RpcClient,
    pub program_id: Pubkey,
}

fn token_program() -> Pubkey {
    Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
}
fn ata_program() -> Pubkey {
    Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap()
}

/// Associated token account — the standard derivation, so we don't take a dep
/// on spl-associated-token-account for one function.
fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program().as_ref(), mint.as_ref()],
        &ata_program(),
    )
    .0
}

/// Upper median of the filled ring slots — matches the program's settlement /
/// quote median (`median_of`, upper median on even counts).
pub fn spot_from(o: &Oracle) -> Option<u64> {
    let n = o.count as usize;
    if n == 0 {
        return None;
    }
    let mut buf = o.samples[..n].to_vec();
    buf.sort_unstable();
    Some(buf[n / 2])
}

#[derive(serde::Serialize)]
pub struct Quote {
    pub spot: u64,
    pub intrinsic: u64,
    pub collateral: u64,
    pub premium: u64,
    /// Settlement price at which the buyer nets zero.
    pub breakeven: u64,
    pub quote_expiry: i64,
}

/// Standard normal CDF — Zelen & Severo 26.2.17, |error| < 7.5e-8. No erf in
/// std and no new dep for six lines.
fn norm_cdf(x: f64) -> f64 {
    let (sign, x) = if x < 0.0 { (-1.0, -x) } else { (1.0, x) };
    let t = 1.0 / (1.0 + 0.2316419 * x);
    let poly = t
        * (0.319381530
            + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    let tail = (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt() * poly;
    0.5 * (1.0 + sign * (1.0 - 2.0 * tail))
}

/// Black-Scholes put, r = 0, all inputs in UI units (dollars, years). r = 0
/// because the epoch is hours long: a rate term would move the price less than
/// the oracle's own tick.
fn bs_put(spot: f64, strike: f64, t_years: f64, sigma: f64) -> f64 {
    if t_years <= 0.0 || sigma <= 0.0 || spot <= 0.0 {
        return (strike - spot).max(0.0);
    }
    let sq = sigma * t_years.sqrt();
    let d1 = ((spot / strike).ln() + 0.5 * sigma * sigma * t_years) / sq;
    strike * norm_cdf(sq - d1) - spot * norm_cdf(-d1)
}

/// Price an option at/above intrinsic. Puts are priced off Black-Scholes at
/// `vol_bps` annualised, so both time to expiry and moneyness are in the number
/// — a flat percentage of collateral charged the same 2% for a 1h option as for
/// a 24h one, and the same for a 10% OTM strike as for an ATM one.
///
/// `spread_bps` is the markup over fair value (the pool's edge), and the result
/// is floored at the program's own `MIN_PREMIUM_BPS` of collateral so a
/// far-OTM quote can never be dust that locks real collateral for nothing.
///
/// ponytail: KIND_CALL keeps the old flat-spread formula. The program's call
/// payoff is inverse (`(spot-strike)*size/spot`, collateralised in the
/// underlying), which is not what `bs_put`'s mirror prices; no call pool exists
/// yet. Price that shape when the first one does.
#[allow(clippy::too_many_arguments)]
pub fn price(
    kind: u8,
    strike: u64,
    size: u64,
    spot: u64,
    spread_bps: u64,
    vol_bps: u64,
    expiry: i64,
    ttl: i64,
    now: i64,
) -> Res<Quote> {
    let intrinsic = payoff(kind, strike, size, spot).map_err(|e| e.to_string())?;
    let collateral = required_collateral(kind, strike, size).map_err(|e| e.to_string())?;
    let markup = 1.0 + spread_bps as f64 / BPS as f64;
    let fair = if kind == KIND_PUT {
        let contracts = size as f64 / SCALE as f64;
        let t = (expiry - now).max(0) as f64 / SECS_PER_YEAR;
        bs_put(
            spot as f64 / SCALE as f64,
            strike as f64 / SCALE as f64,
            t,
            vol_bps as f64 / BPS as f64,
        ) * contracts
            * SCALE as f64
    } else {
        // old behaviour: intrinsic plus a flat slice of collateral
        intrinsic as f64 + (collateral as u128 * spread_bps as u128 / BPS as u128) as f64
    };
    let floor = (collateral as u128 * MIN_PREMIUM_BPS as u128 / BPS as u128) as u64;
    let premium = (fair * markup)
        .max(0.0)
        .min(u64::MAX as f64)
        .round() as u64;
    // three floors, three different reasons: the chain rejects premium < intrinsic,
    // rejects premium below MIN_PREMIUM_BPS of collateral, and rejects zero
    let premium = premium
        .max(intrinsic.checked_add(1).ok_or("premium overflow")?)
        .max(floor)
        .max(1);
    Ok(Quote {
        spot,
        intrinsic,
        collateral,
        premium,
        breakeven: breakeven(kind, strike, size, premium),
        quote_expiry: now + ttl,
    })
}

/// Settlement price at which the buyer nets zero: they paid `premium`, so the
/// payoff has to cover it. Puts break even below the strike, calls above it.
/// Zero when the option can never recover the premium (premium > collateral).
pub fn breakeven(kind: u8, strike: u64, size: u64, premium: u64) -> u64 {
    if size == 0 {
        return 0;
    }
    let per_contract = (premium as u128 * SCALE / size as u128) as u64;
    if kind == KIND_PUT {
        strike.saturating_sub(per_contract)
    } else {
        strike.saturating_add(per_contract)
    }
}

impl Chain {
    pub fn new(rpc_url: String, program_id: Pubkey) -> Self {
        Chain {
            rpc: RpcClient::new(rpc_url),
            program_id,
        }
    }

    pub async fn account<T: AccountDeserialize>(&self, key: &Pubkey) -> Res<T> {
        let data = self.rpc.get_account_data(key).await?;
        Ok(T::try_deserialize(&mut data.as_slice())?)
    }

    /// Every account of type `T` owned by the program. Other account types are
    /// skipped by discriminator; malformed matching accounts are reported.
    pub async fn all<T: AccountDeserialize + Discriminator>(&self) -> Res<Vec<(Pubkey, T)>> {
        let raw = self.rpc.get_program_accounts(&self.program_id).await?;
        let mut accounts = Vec::new();
        for (key, account) in raw {
            if !account.data.starts_with(T::DISCRIMINATOR) {
                continue;
            }
            match T::try_deserialize(&mut account.data.as_slice()) {
                Ok(decoded) => accounts.push((key, decoded)),
                Err(error) => eprintln!("skipping incompatible account {key}: {error}"),
            }
        }
        Ok(accounts)
    }

    /// Build a `buy_option` transaction, fee-payable by `buyer`, already signed
    /// by the `quote_signer`. The caller (frontend) deserialises it, adds the
    /// buyer signature, and submits. We only ever sign terms we computed.
    #[allow(clippy::too_many_arguments)]
    pub async fn build_buy(
        &self,
        pool_key: &Pubkey,
        pool: &Pool,
        buyer: &Pubkey,
        id: u64,
        strike: u64,
        size: u64,
        premium: u64,
        quote_expiry: i64,
        quote_signer: &Keypair,
    ) -> Res<String> {
        let position = Pubkey::find_program_address(
            &[b"opt", pool_key.as_ref(), buyer.as_ref(), &id.to_le_bytes()],
            &self.program_id,
        )
        .0;
        let metas = tessen::accounts::BuyOption {
            buyer: *buyer,
            quote_signer: quote_signer.pubkey(),
            pool: *pool_key,
            oracle: pool.oracle,
            vault: pool.vault,
            buyer_token: ata(buyer, &pool.collateral_mint),
            position,
            token_program: token_program(),
            system_program: solana_sdk::system_program::ID,
        }
        .to_account_metas(None);
        let data = tessen::instruction::BuyOption {
            id,
            strike,
            size,
            premium,
            quote_expiry,
        }
        .data();
        let ix = Instruction {
            program_id: self.program_id,
            accounts: metas,
            data,
        };
        let bh = self.rpc.get_latest_blockhash().await?;
        let mut tx = Transaction::new_with_payer(&[ix], Some(buyer));
        tx.partial_sign(&[quote_signer], bh);
        Ok(STANDARD.encode(bincode::serialize(&tx)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u64 = 1_000_000;

    const HOUR: i64 = 3600;
    const VOL: u64 = 8000; // 80% annualised

    /// One contract, `hours` to expiry, priced now.
    fn q1(strike: u64, spot: u64, hours: i64) -> Quote {
        price(0, strike, S, spot, 200, VOL, hours * HOUR, 60, 0).unwrap()
    }

    #[test]
    fn quote_never_below_intrinsic_and_never_zero() {
        // ITM put: strike 220, spot 200 -> intrinsic 20
        let q = price(0, 220 * S, S, 200 * S, 200, VOL, HOUR, 60, 1000).unwrap();
        assert_eq!(q.intrinsic, 20 * S);
        assert!(q.premium > q.intrinsic);
        assert_eq!(q.collateral, 220 * S);
        assert_eq!(q.quote_expiry, 1060);

        // OTM put: intrinsic 0, premium must still be > 0
        let q = q1(200 * S, 220 * S, 1);
        assert_eq!(q.intrinsic, 0);
        assert!(q.premium > 0);
    }

    /// The whole point of the Black-Scholes swap: a 1h option is far cheaper
    /// than a 24h one, where the old flat spread charged both the same.
    #[test]
    fn premium_shrinks_with_time_to_expiry() {
        let hour = q1(1060 * S, 1059 * S, 1);
        let day = q1(1060 * S, 1059 * S, 24);
        assert!(day.premium > 4 * hour.premium, "{} vs {}", day.premium, hour.premium);
        // ~80% IV ATM for 1h lands single-digit dollars, not the old $21
        assert!((3 * S..8 * S).contains(&hour.premium), "{}", hour.premium);
    }

    /// ...and the other half: a far-OTM strike is cheap, where the old formula
    /// charged nearly the ATM price for it.
    #[test]
    fn premium_shrinks_as_strike_goes_out_of_the_money() {
        let atm = q1(1060 * S, 1059 * S, 1);
        let otm = q1(1000 * S, 1059 * S, 1);
        assert!(otm.premium * 4 < atm.premium, "{} vs {}", otm.premium, atm.premium);
        // but never below the program's own MIN_PREMIUM_BPS of collateral
        assert_eq!(otm.premium, 1000 * S * MIN_PREMIUM_BPS / BPS);
    }

    #[test]
    fn breakeven_is_strike_less_premium_per_contract() {
        let q = q1(1060 * S, 1059 * S, 1);
        assert_eq!(q.breakeven, 1060 * S - q.premium);
        // two contracts halve the per-contract premium, so breakeven moves up
        let two = price(0, 1060 * S, 2 * S, 1059 * S, 200, VOL, HOUR, 60, 0).unwrap();
        assert_eq!(two.breakeven, 1060 * S - two.premium / 2);
        // a put that cannot recover its premium floors at zero, never wraps
        assert_eq!(breakeven(0, 10, S, 99 * S), 0);
    }

    #[test]
    fn norm_cdf_matches_known_values() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-9);
        assert!((norm_cdf(1.96) - 0.975).abs() < 1e-4);
        assert!((norm_cdf(-1.96) - 0.025).abs() < 1e-4);
        assert!(norm_cdf(-40.0) >= 0.0 && norm_cdf(40.0) <= 1.0);
    }

    #[test]
    fn spot_is_upper_median() {
        let mut o = Oracle {
            keeper: Pubkey::default(),
            underlying: Pubkey::default(),
            samples: [0; 32],
            sample_ts: [0; 32],
            last_update: 0,
            count: 0,
            idx: 0,
        };
        assert_eq!(spot_from(&o), None);
        for (i, v) in [30u64, 10, 20].iter().enumerate() {
            o.samples[i] = *v;
            o.count += 1;
        }
        assert_eq!(spot_from(&o), Some(20));
    }
}

