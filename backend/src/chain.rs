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

use tessen::{payoff, required_collateral, Oracle, Pool};

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

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
    pub quote_expiry: i64,
}

/// Price an option at/above intrinsic. The premium reuses the program's own
/// `payoff`/`required_collateral`, so the on-chain `premium >= intrinsic` floor
/// can never reject a quote we produced (barring the oracle moving under us,
/// which the spread cushions).
pub fn price(
    kind: u8,
    strike: u64,
    size: u64,
    spot: u64,
    spread_bps: u64,
    ttl: i64,
    now: i64,
) -> Res<Quote> {
    let intrinsic = payoff(kind, strike, size, spot).map_err(|e| e.to_string())?;
    let collateral = required_collateral(kind, strike, size).map_err(|e| e.to_string())?;
    let spread = ((collateral as u128 * spread_bps as u128) / 10_000) as u64;
    let premium = intrinsic
        .checked_add(spread.max(1)) // never quote a zero premium; the chain rejects it
        .ok_or("premium overflow")?;
    Ok(Quote {
        spot,
        intrinsic,
        collateral,
        premium,
        quote_expiry: now + ttl,
    })
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

    #[test]
    fn quote_never_below_intrinsic_and_never_zero() {
        // ITM put: strike 220, spot 200 -> intrinsic 20
        let q = price(0, 220 * S, S, 200 * S, 200, 60, 1000).unwrap();
        assert_eq!(q.intrinsic, 20 * S);
        assert!(q.premium > q.intrinsic);
        assert_eq!(q.collateral, 220 * S);
        assert_eq!(q.quote_expiry, 1060);

        // OTM put: intrinsic 0, premium must still be > 0
        let q = price(0, 200 * S, S, 220 * S, 200, 60, 0).unwrap();
        assert_eq!(q.intrinsic, 0);
        assert!(q.premium > 0);
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
