//! stocklana keeper — the off-chain operator loop.
//!
//! One process that runs the routine epoch machinery against a deployed pool:
//! pushes oracle prices on a cadence, closes the epoch at its boundary, settles
//! every position, then rolls the next epoch.
//!
//! Config is all env vars (pubkeys base58, keypairs are solana-cli json files):
//!   RPC_URL             default http://127.0.0.1:8899
//!   PROGRAM_ID          default 86kDc93MAkfLm3JNi43KYcFPiKZ765tYHPTekxmp1Ukb
//!   POOL                required — the pool PDA; oracle/vault/mint are read off it
//!   KEEPER_KEYPAIR      required — fee payer, signs push/close/settle
//!   AUTHORITY_KEYPAIR   optional — signs roll_epoch (default: KEEPER_KEYPAIR)
//!   PRICE_FILE          file holding the spot price in dollars ("212.34"); re-read every tick
//!   PRICE_1E6           fallback fixed 1e6-scaled price if PRICE_FILE is unset
//!   PUSH_INTERVAL_SECS  default 60
//!   EPOCH_LEN_SECS      default 86400
//!
//! LOAD-BEARING INVARIANT: once `now >= epoch_end` this loop STOPS pushing and
//! closes. A push landing after expiry rotates the in-window samples out of the
//! 32-slot ring and bricks `close_epoch` forever — that is the whole recovery
//! story in the program's `close_epoch` ponytail, and `phase()` encodes it.

use std::rc::Rc;
use std::time::Duration;

use anchor_client::solana_sdk::commitment_config::CommitmentConfig;
use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::solana_sdk::signature::{read_keypair_file, Keypair};
use anchor_client::solana_sdk::signer::Signer;
use anchor_client::{Client, Cluster, Program};

use stocklana::{EpochState, OptionPosition, Oracle, Pool};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const BPS: u128 = 10_000;
const MAX_DEV_BPS: u128 = 1_000; // mirrors the program's push deviation band

struct Config {
    rpc_url: String,
    program_id: Pubkey,
    pool: Pubkey,
    keeper: Rc<Keypair>,
    authority: Keypair,
    price_file: Option<String>,
    price_1e6: Option<u64>,
    push_interval: Duration,
    epoch_len: i64,
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

fn load_config() -> Res<Config> {
    let keeper_path = env("KEEPER_KEYPAIR").ok_or("KEEPER_KEYPAIR is required")?;
    let keeper = read_keypair_file(&keeper_path).map_err(|e| format!("keeper key: {e}"))?;
    let authority = match env("AUTHORITY_KEYPAIR") {
        Some(p) => read_keypair_file(&p).map_err(|e| format!("authority key: {e}"))?,
        None => Keypair::try_from(&keeper.to_bytes()[..]).unwrap(), // default: same desk key
    };
    Ok(Config {
        rpc_url: env("RPC_URL").unwrap_or_else(|| "http://127.0.0.1:8899".into()),
        program_id: env("PROGRAM_ID")
            .unwrap_or_else(|| "86kDc93MAkfLm3JNi43KYcFPiKZ765tYHPTekxmp1Ukb".into())
            .parse()?,
        pool: env("POOL").ok_or("POOL is required")?.parse()?,
        keeper: Rc::new(keeper),
        authority,
        price_file: env("PRICE_FILE"),
        price_1e6: env("PRICE_1E6").map(|s| s.parse()).transpose()?,
        push_interval: Duration::from_secs(env("PUSH_INTERVAL_SECS").and_then(|s| s.parse().ok()).unwrap_or(60)),
        epoch_len: env("EPOCH_LEN_SECS").and_then(|s| s.parse().ok()).unwrap_or(86_400),
    })
}

/// The spot the keeper wants to publish this tick, 1e6-scaled. `PRICE_FILE`
/// (dollars) is the calibration knob a real feed writes to; `PRICE_1E6` is the
/// static fallback. Re-read every tick so the operator can steer live.
/// ponytail: file/env price source, no exchange client. Wire a real feed into
/// PRICE_FILE (a separate writer process) or replace this fn if you want the
/// keeper to pull the quote itself.
fn read_target(cfg: &Config) -> Res<u64> {
    let price = if let Some(path) = &cfg.price_file {
        let dollars: f64 = std::fs::read_to_string(path)?.trim().parse()?;
        (dollars * 1_000_000.0).round() as u64
    } else {
        cfg.price_1e6.ok_or("no price source: set PRICE_FILE or PRICE_1E6")?
    };
    if price == 0 {
        return Err("price resolved to zero".into());
    }
    Ok(price)
}

/// Upper median of the filled ring slots — the exact number `push_price`'s
/// deviation guard measures a new push against on chain (`median_of`, upper
/// median on even counts). `None` on an empty ring.
fn ring_median(samples: &[u64]) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut buf = samples.to_vec();
    buf.sort_unstable();
    Some(buf[buf.len() / 2])
}

/// Clamp `target` into the program's ±10% deviation band around `med` so a
/// single push is never rejected. A real market gap is crossed by pushing
/// repeatedly: each tick steps up to 10% toward `target` and the next tick
/// re-centres on the new median.
fn clamp_band(target: u64, med: u64) -> u64 {
    let d = ((med as u128) * MAX_DEV_BPS / BPS) as u64;
    target.clamp(med.saturating_sub(d), med + d).max(1)
}

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    Push,
    Close,
    Settle,
    Roll,
}

/// The one place the epoch state machine lives. Note `Open && now >= epoch_end`
/// resolves to `Close`, never `Push`: pushing past expiry is the documented
/// brick, so it must be unreachable from here.
fn phase(state: EpochState, now: i64, epoch_end: i64, open_positions: u32) -> Phase {
    match state {
        EpochState::Genesis => Phase::Roll,
        EpochState::Open if now < epoch_end => Phase::Push,
        EpochState::Open => Phase::Close,
        EpochState::Closed if open_positions > 0 => Phase::Settle,
        EpochState::Closed => Phase::Roll,
    }
}

fn epoch_record_pda(program_id: &Pubkey, pool: &Pubkey, epoch: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[b"epoch", pool.as_ref(), &epoch.to_le_bytes()],
        program_id,
    )
    .0
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

struct Keeper<'a> {
    cfg: &'a Config,
    program: Program<Rc<Keypair>>,
}

impl<'a> Keeper<'a> {
    fn pool(&self) -> Res<Pool> {
        Ok(self.program.account::<Pool>(self.cfg.pool)?)
    }

    fn do_push(&self, pool: &Pool) -> Res<()> {
        let oracle = self.program.account::<Oracle>(pool.oracle)?;
        let target = read_target(self.cfg)?;
        let price = match ring_median(&oracle.samples[..oracle.count as usize]) {
            Some(med) => clamp_band(target, med),
            None => target, // empty ring: first sample has nothing to deviate from
        };
        self.program
            .request()
            .accounts(stocklana::accounts::PushPrice {
                oracle: pool.oracle,
                keeper: self.cfg.keeper.pubkey(),
            })
            .args(stocklana::instruction::PushPrice { price })
            .send()?;
        println!("push  price={price} (target={target})");
        Ok(())
    }

    fn do_close(&self, pool: &Pool) -> Res<()> {
        let record = epoch_record_pda(&self.cfg.program_id, &self.cfg.pool, pool.epoch);
        self.program
            .request()
            .accounts(stocklana::accounts::CloseEpoch {
                operator: self.cfg.keeper.pubkey(),
                pool: self.cfg.pool,
                oracle: pool.oracle,
                epoch_record: record,
                system_program: anchor_client::anchor_lang::solana_program::system_program::ID,
            })
            .args(stocklana::instruction::CloseEpoch {})
            .send()?;
        println!("close epoch={}", pool.epoch);
        Ok(())
    }

    fn do_settle(&self) -> Res<()> {
        // disc filter is added by `accounts()`; the rest we filter locally, so
        // no rpc-filter dependency. open positions only ever belong to the live
        // epoch (you cannot roll with open_positions > 0), but we settle each
        // against its own record for correctness.
        let positions: Vec<(Pubkey, OptionPosition)> = self.program.accounts(vec![])?;
        for (key, pos) in positions {
            if pos.pool != self.cfg.pool || pos.settled {
                continue;
            }
            let record = epoch_record_pda(&self.cfg.program_id, &self.cfg.pool, pos.epoch);
            self.program
                .request()
                .accounts(stocklana::accounts::Settle {
                    operator: self.cfg.keeper.pubkey(),
                    pool: self.cfg.pool,
                    position: key,
                    epoch_record: record,
                })
                .args(stocklana::instruction::Settle {})
                .send()?;
            println!("settle position={key}");
        }
        Ok(())
    }

    fn do_roll(&self) -> Res<()> {
        let epoch_end = now_ts() + self.cfg.epoch_len;
        self.program
            .request()
            .accounts(stocklana::accounts::RollEpoch {
                pool: self.cfg.pool,
                authority: self.cfg.authority.pubkey(),
            })
            .args(stocklana::instruction::RollEpoch { epoch_end })
            .signer(&self.cfg.authority)
            .send()?;
        println!("roll  epoch_end={epoch_end}");
        Ok(())
    }

    /// One iteration. Returns true when it changed on-chain state and the loop
    /// should re-check promptly instead of sleeping a full push interval.
    fn tick(&self) -> Res<bool> {
        let pool = self.pool()?;
        match phase(pool.state, now_ts(), pool.epoch_end, pool.open_positions) {
            Phase::Push => {
                self.do_push(&pool)?;
                Ok(false)
            }
            Phase::Close => {
                self.do_close(&pool)?;
                Ok(true)
            }
            Phase::Settle => {
                self.do_settle()?;
                Ok(true)
            }
            Phase::Roll => {
                self.do_roll()?;
                Ok(true)
            }
        }
    }
}

fn main() -> Res<()> {
    let cfg = load_config()?;
    let cluster = Cluster::Custom(cfg.rpc_url.clone(), cfg.rpc_url.clone());
    let client = Client::new_with_options(cluster, cfg.keeper.clone(), CommitmentConfig::confirmed());
    let program = client.program(cfg.program_id)?;
    let keeper = Keeper { cfg: &cfg, program };

    println!(
        "keeper up: pool={} keeper={} authority={} interval={}s",
        cfg.pool,
        cfg.keeper.pubkey(),
        cfg.authority.pubkey(),
        cfg.push_interval.as_secs()
    );

    loop {
        let fast = match keeper.tick() {
            Ok(fast) => fast,
            Err(e) => {
                eprintln!("tick error: {e}");
                false
            }
        };
        std::thread::sleep(if fast {
            Duration::from_secs(2)
        } else {
            cfg.push_interval
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // replica of the program's `within_deviation`, so we can assert clamp output
    // is always a push the chain would accept.
    fn within(p: u64, med: u64) -> bool {
        (p.abs_diff(med) as u128) * BPS <= (med as u128) * MAX_DEV_BPS
    }

    #[test]
    fn ring_median_is_upper_median_like_on_chain() {
        assert_eq!(ring_median(&[]), None);
        assert_eq!(ring_median(&[3, 1, 2]), Some(2));
        assert_eq!(ring_median(&[10, 20]), Some(20)); // upper median on even count
    }

    #[test]
    fn clamp_stays_inside_the_band_the_chain_enforces() {
        for &med in &[1u64, 1_000_000, 200_000_000] {
            for &target in &[0u64, 1, med / 2, med, med * 3, u64::MAX / 2] {
                let p = clamp_band(target, med);
                assert!(p > 0);
                assert!(within(p, med), "clamp({target},{med})={p} escaped band");
            }
        }
        assert_eq!(clamp_band(500_000_000, 200_000_000), 220_000_000);
        assert_eq!(clamp_band(1, 200_000_000), 180_000_000);
    }

    #[test]
    fn never_pushes_past_expiry() {
        // the brick guard: an Open epoch past its end closes, it does not push
        assert_eq!(phase(EpochState::Open, 100, 200, 0), Phase::Push);
        assert_eq!(phase(EpochState::Open, 200, 200, 3), Phase::Close);
        assert_eq!(phase(EpochState::Open, 999, 200, 3), Phase::Close);
    }

    #[test]
    fn closed_settles_then_rolls_genesis_rolls() {
        assert_eq!(phase(EpochState::Closed, 300, 200, 2), Phase::Settle);
        assert_eq!(phase(EpochState::Closed, 300, 200, 0), Phase::Roll);
        assert_eq!(phase(EpochState::Genesis, 0, 0, 0), Phase::Roll);
    }
}
