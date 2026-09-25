//! Tessen keeper — the off-chain operator loop.
//!
//! A supervisor process starts one worker process per deployed pool. Each worker
//! pushes oracle prices on a cadence, closes its epoch at the boundary, settles
//! every position, then rolls the next epoch.
//!
//! Config is all env vars (pubkeys base58, private keys are JSON byte arrays):
//!   RPC_URL             default http://127.0.0.1:8899
//!   PROGRAM_ID          default 548P3sxkEEeE1L935jh4y7Tcosp5zUjCeR7Nn7NL1MHr
//!   POOL_1              required — first pool PDA
//!   EPOCH_LEN_SECS_1    required — first pool's epoch length
//!   POOL_2              required — second pool PDA
//!   EPOCH_LEN_SECS_2    required — second pool's epoch length
//!   KEEPER_PRIVATE_KEY  required — fee payer, signs push/close/settle
//!   AUTHORITY_PRIVATE_KEY optional — signs roll_epoch (default: keeper key)
//!   PRICE_URL           preStocks API, default https://prestocks.com/api/prestocks
//!   PRICE_SYMBOL        which preStock to track, default ANTHROPIC
//!   PRICE_FIELD         mark|token (or the full markPrice/tokenPrice), default mark
//!   PRICE_POLL_SECS     how often the poller refetches, default 30
//!   PRICE_FILE          override: a file of dollars ("212.34"), re-read every tick
//!   PRICE_1E6           override: a static 1e6-scaled price, for offline runs
//!   PUSH_INTERVAL_SECS  default 60
//!
//! LOAD-BEARING INVARIANT: once `now >= epoch_end` this loop STOPS pushing and
//! closes. A push landing after expiry rotates the in-window samples out of the
//! 32-slot ring and bricks `close_epoch` forever — that is the whole recovery
//! story in the program's `close_epoch` ponytail, and `phase()` encodes it.

use std::process::{Child, Command};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anchor_client::solana_sdk::commitment_config::CommitmentConfig;
use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::solana_sdk::signature::Keypair;
use anchor_client::solana_sdk::signer::Signer;
use anchor_client::{Client, Cluster, Program};

use tessen::{EpochState, OptionPosition, Oracle, Pool};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const BPS: u128 = 10_000;
const MAX_DEV_BPS: u128 = 1_000; // mirrors the program's push deviation band
const PRESTOCKS_API: &str = "https://prestocks.com/api/prestocks";
const WORKER_PROCESS: &str = "TESSEN_KEEPER_WORKER";
/// How old a polled price may get before every push says so out loud.
const STALE_WARN_SECS: i64 = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PoolConfig {
    name: String,
    pool: Pubkey,
    epoch_len: i64,
}

struct Config {
    rpc_url: String,
    program_id: Pubkey,
    pool: Pubkey,
    keeper: Rc<Keypair>,
    authority: Keypair,
    price: PriceSource,
    push_interval: Duration,
    epoch_len: i64,
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

fn load_dotenv() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    let _ = dotenvy::from_path(path);
}

fn parse_pool_config(name: String, pool: String, epoch_len: String) -> Res<PoolConfig> {
    let epoch_len = epoch_len.parse::<i64>()?;
    if epoch_len <= 0 {
        return Err(format!("{name} epoch length must be positive").into());
    }
    Ok(PoolConfig {
        name,
        pool: pool.parse()?,
        epoch_len,
    })
}

fn load_pool_config(slot: usize) -> Res<PoolConfig> {
    let pool_key = format!("POOL_{slot}");
    let epoch_key = format!("EPOCH_LEN_SECS_{slot}");
    parse_pool_config(
        format!("pool-{slot}"),
        env(&pool_key).ok_or_else(|| format!("{pool_key} is required"))?,
        env(&epoch_key).ok_or_else(|| format!("{epoch_key} is required"))?,
    )
}

fn load_pool_configs() -> Res<[PoolConfig; 2]> {
    let configs = [load_pool_config(1)?, load_pool_config(2)?];
    if configs[0].pool == configs[1].pool {
        return Err("POOL_1 and POOL_2 must be different".into());
    }
    Ok(configs)
}

fn parse_keypair(value: &str, name: &str) -> Res<Keypair> {
    let bytes: Vec<u8> = serde_json::from_str(value)
        .map_err(|e| format!("{name} must be a JSON byte array: {e}"))?;
    Keypair::try_from(bytes.as_slice()).map_err(|e| format!("invalid {name}: {e}").into())
}

fn load_config() -> Res<Config> {
    let keeper_value = env("KEEPER_PRIVATE_KEY").ok_or("KEEPER_PRIVATE_KEY is required")?;
    let keeper = parse_keypair(&keeper_value, "KEEPER_PRIVATE_KEY")?;
    let authority = match env("AUTHORITY_PRIVATE_KEY") {
        Some(value) => parse_keypair(&value, "AUTHORITY_PRIVATE_KEY")?,
        None => Keypair::try_from(&keeper.to_bytes()[..]).unwrap(), // default: same desk key
    };
    Ok(Config {
        rpc_url: env("RPC_URL").unwrap_or_else(|| "http://127.0.0.1:8899".into()),
        program_id: env("PROGRAM_ID")
            .unwrap_or_else(|| "548P3sxkEEeE1L935jh4y7Tcosp5zUjCeR7Nn7NL1MHr".into())
            .parse()?,
        pool: env("POOL").ok_or("POOL is required")?.parse()?,
        keeper: Rc::new(keeper),
        authority,
        price: PriceSource::resolve()?,
        push_interval: Duration::from_secs(
            env("PUSH_INTERVAL_SECS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
        ),
        epoch_len: env("EPOCH_LEN_SECS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(86_400),
    })
}

/// Dollars -> 1e6 u64, rejecting anything that would put a nonsense price on
/// chain. `push_price` only checks `price > 0`, so everything else is caught here.
fn to_scaled(dollars: f64) -> Res<u64> {
    if !dollars.is_finite() || dollars <= 0.0 {
        return Err(format!("price {dollars} is not a positive finite number").into());
    }
    let scaled = (dollars * 1_000_000.0).round();
    if scaled >= u64::MAX as f64 {
        return Err(format!("price {dollars} overflows u64 at 1e6 scale").into());
    }
    Ok(scaled as u64)
}

/// Pull `symbol`'s `field` out of a preStocks payload and scale it.
///
/// The API carries two prices per token and they are NOT interchangeable:
/// `markPrice` is the SPV mark, `tokenPrice` is the DEX price, and for thin
/// tokens they diverge badly (SPACEX's sit ~29% apart). The caller names which.
fn price_from_json(body: &str, symbol: &str, field: &str) -> Res<u64> {
    let v: serde_json::Value = serde_json::from_str(body)?;
    let arr = v.as_array().ok_or("preStocks payload is not an array")?;
    let hit = arr
        .iter()
        .find(|t| {
            t["symbol"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(symbol))
        })
        .ok_or_else(|| format!("no token {symbol} in payload"))?;
    let dollars = hit[field]
        .as_f64()
        .ok_or_else(|| format!("{symbol}.{field} is missing or not a number"))?;
    to_scaled(dollars)
}

/// One successful observation from the poller.
#[derive(Clone, Copy, Debug)]
struct Observed {
    price: u64,
    at: i64,
}

/// Live price poller. The HTTP request runs on its own thread and publishes into
/// this cell; `latest()` only takes a mutex.
///
/// LOAD-BEARING: the tick loop must never block on HTTP. `phase()` has to reach
/// `Close` the moment `now >= epoch_end`; a request hanging inside `do_push`
/// delays that, the in-window samples rotate out of the 32-slot ring, and then
/// the epoch can never close at all — the brick in the module docstring. A
/// background thread plus a non-blocking read is what keeps that unreachable.
#[derive(Clone)]
struct PriceFeed {
    cell: Arc<Mutex<Option<Observed>>>,
    label: String,
}

impl PriceFeed {
    fn spawn(url: String, symbol: String, field: String, every: Duration) -> Self {
        let cell = Arc::new(Mutex::new(None));
        let label = format!("{symbol}.{field}");
        let feed = PriceFeed {
            cell: Arc::clone(&cell),
            label: label.clone(),
        };
        std::thread::Builder::new()
            .name("price-poll".into())
            .spawn(move || {
                let client = match reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("price: cannot build http client: {e}");
                        return;
                    }
                };
                loop {
                    match fetch_price(&client, &url, &symbol, &field) {
                        Ok(price) => set_cell(
                            &cell,
                            Observed {
                                price,
                                at: now_ts(),
                            },
                        ),
                        // Never fatal: a stale price still lets the epoch close,
                        // whereas a dead poller thread would leave it unable to.
                        Err(e) => eprintln!("price: {label} fetch failed: {e} (serving last good)"),
                    }
                    std::thread::sleep(every);
                }
            })
            .expect("spawn price poller");
        feed
    }

    /// Lock-poison tolerant: if the poller ever panicked holding the lock, the
    /// keeper still reads the last value rather than panicking in sympathy.
    fn latest(&self) -> Option<Observed> {
        match self.cell.lock() {
            Ok(g) => *g,
            Err(p) => **p.get_ref(),
        }
    }

    /// Block at startup only, so the first push doesn't have to fail and wait a
    /// whole interval. Bounded, and a miss is a warning: Close and Settle need
    /// no price at all, so the keeper must still come up without one.
    fn wait_first(&self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while self.latest().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        match self.latest() {
            Some(o) => println!("price: {} first sample {}", self.label, o.price),
            None => eprintln!(
                "price: {} no sample after {:?} — pushes will wait, close/settle will not",
                self.label, timeout
            ),
        }
    }
}

fn set_cell(cell: &Arc<Mutex<Option<Observed>>>, o: Observed) {
    match cell.lock() {
        Ok(mut g) => *g = Some(o),
        Err(p) => {
            let mut g = p.into_inner();
            *g = Some(o);
        }
    }
}

fn fetch_price(
    client: &reqwest::blocking::Client,
    url: &str,
    symbol: &str,
    field: &str,
) -> Res<u64> {
    let body = client
        .get(url)
        .header("accept", "application/json")
        .send()?
        .error_for_status()?
        .text()?;
    price_from_json(&body, symbol, field)
}

/// Where spot comes from. Resolved once at startup and printed, so nobody has to
/// guess which source a running keeper is actually pushing from.
enum PriceSource {
    /// A file of dollars an operator writes. The manual override, and the hatch
    /// to pull when the API is wrong.
    File(String),
    /// Static price, for offline runs and tests.
    Fixed(u64),
    /// Live poll of the preStocks API.
    Poll(PriceFeed),
}

impl PriceSource {
    fn resolve() -> Res<Self> {
        // overrides first: an operator who set one meant it
        if let Some(path) = env("PRICE_FILE") {
            return Ok(PriceSource::File(path));
        }
        if let Some(p) = env("PRICE_1E6") {
            return Ok(PriceSource::Fixed(p.parse()?));
        }
        let field = match env("PRICE_FIELD").unwrap_or_else(|| "mark".into()).as_str() {
            "mark" | "markPrice" => "markPrice".to_string(),
            "token" | "tokenPrice" => "tokenPrice".to_string(),
            other => return Err(format!("PRICE_FIELD must be mark or token, got {other}").into()),
        };
        Ok(PriceSource::Poll(PriceFeed::spawn(
            env("PRICE_URL").unwrap_or_else(|| PRESTOCKS_API.into()),
            env("PRICE_SYMBOL").unwrap_or_else(|| "ANTHROPIC".into()),
            field,
            Duration::from_secs(
                env("PRICE_POLL_SECS")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(30),
            ),
        )))
    }

    fn describe(&self) -> String {
        match self {
            PriceSource::File(p) => format!("file {p}"),
            PriceSource::Fixed(v) => format!("fixed {v}"),
            PriceSource::Poll(f) => format!("live poll {}", f.label),
        }
    }

    /// The spot this tick wants to publish, 1e6-scaled. Never blocks on the
    /// network: the poll variant reads whatever the background thread last put
    /// there.
    fn target(&self) -> Res<u64> {
        match self {
            PriceSource::File(path) => to_scaled(std::fs::read_to_string(path)?.trim().parse()?),
            PriceSource::Fixed(p) => Ok(*p),
            PriceSource::Poll(feed) => {
                let o = feed.latest().ok_or("price feed has no sample yet")?;
                let age = now_ts() - o.at;
                // Pushing a stale price beats not pushing: below
                // MIN_SETTLEMENT_SAMPLES in-window samples the epoch refuses to
                // close at all, so a quiet keeper is the worse failure.
                if age > STALE_WARN_SECS {
                    eprintln!("price: {} is {age}s stale — pushing it anyway", feed.label);
                }
                Ok(o.price)
            }
        }
    }
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
    Pubkey::find_program_address(&[b"epoch", pool.as_ref(), &epoch.to_le_bytes()], program_id).0
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
        let target = self.cfg.price.target()?;
        let price = match ring_median(&oracle.samples[..oracle.count as usize]) {
            Some(med) => clamp_band(target, med),
            None => target, // empty ring: first sample has nothing to deviate from
        };
        self.program
            .request()
            .accounts(tessen::accounts::PushPrice {
                oracle: pool.oracle,
                keeper: self.cfg.keeper.pubkey(),
            })
            .args(tessen::instruction::PushPrice { price })
            .send()?;
        println!("push  price={price} (target={target})");
        Ok(())
    }

    fn do_close(&self, pool: &Pool) -> Res<()> {
        let record = epoch_record_pda(&self.cfg.program_id, &self.cfg.pool, pool.epoch);
        self.program
            .request()
            .accounts(tessen::accounts::CloseEpoch {
                operator: self.cfg.keeper.pubkey(),
                pool: self.cfg.pool,
                oracle: pool.oracle,
                epoch_record: record,
                system_program: anchor_client::anchor_lang::solana_program::system_program::ID,
            })
            .args(tessen::instruction::CloseEpoch {})
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
                .accounts(tessen::accounts::Settle {
                    operator: self.cfg.keeper.pubkey(),
                    pool: self.cfg.pool,
                    position: key,
                    epoch_record: record,
                })
                .args(tessen::instruction::Settle {})
                .send()?;
            println!("settle position={key}");
        }
        Ok(())
    }

    fn do_roll(&self) -> Res<()> {
        let epoch_end = now_ts() + self.cfg.epoch_len;
        self.program
            .request()
            .accounts(tessen::accounts::RollEpoch {
                pool: self.cfg.pool,
                authority: self.cfg.authority.pubkey(),
            })
            .args(tessen::instruction::RollEpoch { epoch_end })
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

fn run_worker() -> Res<()> {
    let cfg = load_config()?;
    let cluster = Cluster::Custom(cfg.rpc_url.clone(), cfg.rpc_url.clone());
    let client =
        Client::new_with_options(cluster, cfg.keeper.clone(), CommitmentConfig::confirmed());
    let program = client.program(cfg.program_id)?;
    let keeper = Keeper { cfg: &cfg, program };

    println!(
        "keeper up: pool={} keeper={} authority={} interval={}s price={}",
        cfg.pool,
        cfg.keeper.pubkey(),
        cfg.authority.pubkey(),
        cfg.push_interval.as_secs(),
        cfg.price.describe()
    );
    if let PriceSource::Poll(feed) = &cfg.price {
        feed.wait_first(Duration::from_secs(15));
    }

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

fn spawn_worker(exe: &std::path::Path, cfg: &PoolConfig) -> Res<Child> {
    let child = Command::new(exe)
        .env(WORKER_PROCESS, "1")
        .env("POOL", cfg.pool.to_string())
        .env("EPOCH_LEN_SECS", cfg.epoch_len.to_string())
        .spawn()?;
    println!(
        "spawned {}: pid={} pool={} epoch={}s",
        cfg.name,
        child.id(),
        cfg.pool,
        cfg.epoch_len
    );
    Ok(child)
}

fn stop_worker(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn stop_all_workers(workers: &mut [(PoolConfig, Child)]) {
    for (_, child) in workers {
        stop_worker(child);
    }
}

fn stop_workers(workers: &mut [(PoolConfig, Child)], exited: usize) {
    for (index, (_, child)) in workers.iter_mut().enumerate() {
        if index != exited {
            stop_worker(child);
        }
    }
}

fn supervise_workers(mut workers: Vec<(PoolConfig, Child)>) -> Res<()> {
    loop {
        for index in 0..workers.len() {
            match workers[index].1.try_wait() {
                Ok(Some(status)) => {
                    let name = workers[index].0.name.clone();
                    stop_workers(&mut workers, index);
                    return Err(format!("{name} worker exited with {status}").into());
                }
                Ok(None) => {}
                Err(error) => {
                    stop_all_workers(&mut workers);
                    return Err(error.into());
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn spawn_workers(exe: &std::path::Path, configs: [PoolConfig; 2]) -> Res<Vec<(PoolConfig, Child)>> {
    let mut workers = Vec::with_capacity(configs.len());
    for cfg in configs {
        match spawn_worker(exe, &cfg) {
            Ok(child) => workers.push((cfg, child)),
            Err(error) => {
                stop_all_workers(&mut workers);
                return Err(error);
            }
        }
    }
    Ok(workers)
}

fn run_supervisor() -> Res<()> {
    let exe = std::env::current_exe()?;
    supervise_workers(spawn_workers(&exe, load_pool_configs()?)?)
}

fn main() -> Res<()> {
    load_dotenv();
    if env(WORKER_PROCESS).is_some() {
        run_worker()
    } else {
        run_supervisor()
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

    // A trimmed copy of a real /api/prestocks response.
    const PAYLOAD: &str = r#"[
      {"symbol":"ANDURIL","contract_address":"PresTj4","markPrice":152.85,"tokenPrice":150.33},
      {"symbol":"ANTHROPIC","contract_address":"Pren1Fv","markPrice":1048.10584,"tokenPrice":1032.3512},
      {"symbol":"SPACEX","contract_address":"PreANxu","markPrice":148.645,"tokenPrice":115.4951}
    ]"#;

    #[test]
    fn picks_the_named_token_and_the_named_field() {
        assert_eq!(
            price_from_json(PAYLOAD, "ANTHROPIC", "markPrice").unwrap(),
            1_048_105_840
        );
        assert_eq!(
            price_from_json(PAYLOAD, "ANTHROPIC", "tokenPrice").unwrap(),
            1_032_351_200
        );
        // not the first entry, and case-insensitive on the symbol
        assert_eq!(
            price_from_json(PAYLOAD, "anthropic", "markPrice").unwrap(),
            1_048_105_840
        );
        // the two fields are far apart for a thin token — the choice is load-bearing
        let mark = price_from_json(PAYLOAD, "SPACEX", "markPrice").unwrap();
        let token = price_from_json(PAYLOAD, "SPACEX", "tokenPrice").unwrap();
        assert!(
            mark.abs_diff(token) * 100 / mark > 20,
            "mark {mark} token {token}"
        );
    }

    #[test]
    fn rejects_payloads_that_would_push_a_nonsense_price() {
        for (body, sym) in [
            (PAYLOAD, "NOPE"),                            // token absent
            (r#"{"symbol":"ANTHROPIC"}"#, "ANTHROPIC"),   // not an array
            (r#"[{"symbol":"ANTHROPIC"}]"#, "ANTHROPIC"), // field missing
            (r#"[{"symbol":"ANTHROPIC","markPrice":0}]"#, "ANTHROPIC"),
            (r#"[{"symbol":"ANTHROPIC","markPrice":-5}]"#, "ANTHROPIC"),
            (
                r#"[{"symbol":"ANTHROPIC","markPrice":"1048"}]"#,
                "ANTHROPIC",
            ), // string, not number
            (r#"not json"#, "ANTHROPIC"),
        ] {
            assert!(
                price_from_json(body, sym, "markPrice").is_err(),
                "accepted {body}"
            );
        }
    }

    #[test]
    fn scaling_is_the_program_convention_and_rejects_the_rest() {
        assert_eq!(to_scaled(1.0).unwrap(), 1_000_000);
        assert_eq!(to_scaled(1048.10584).unwrap(), 1_048_105_840);
        assert_eq!(to_scaled(0.000001).unwrap(), 1);
        assert_eq!(to_scaled(0.0000004).unwrap_or(0), 0); // rounds to zero -> rejected
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e15] {
            assert!(to_scaled(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn a_polled_price_is_usable_and_survives_a_poisoned_lock() {
        let feed = PriceFeed {
            cell: Arc::new(Mutex::new(None)),
            label: "TEST.markPrice".into(),
        };
        // nothing fetched yet: pushes must error rather than invent a price
        assert!(PriceSource::Poll(feed.clone()).target().is_err());

        set_cell(
            &feed.cell,
            Observed {
                price: 1_048_105_840,
                at: now_ts(),
            },
        );
        assert_eq!(
            PriceSource::Poll(feed.clone()).target().unwrap(),
            1_048_105_840
        );

        // poison the mutex the way a panicking poller would, then read again
        let c = Arc::clone(&feed.cell);
        let _ = std::thread::spawn(move || {
            let _g = c.lock().unwrap();
            panic!("poller died holding the lock");
        })
        .join();
        assert!(feed.cell.is_poisoned());
        assert_eq!(feed.latest().unwrap().price, 1_048_105_840);
        set_cell(
            &feed.cell,
            Observed {
                price: 999_000_000,
                at: now_ts(),
            },
        );
        assert_eq!(feed.latest().unwrap().price, 999_000_000);
    }

    #[test]
    fn a_stale_polled_price_is_still_pushed() {
        // a quiet keeper is the worse failure: without in-window samples the
        // epoch cannot close at all
        let feed = PriceFeed {
            cell: Arc::new(Mutex::new(None)),
            label: "T".into(),
        };
        set_cell(
            &feed.cell,
            Observed {
                price: 500_000,
                at: now_ts() - 10 * STALE_WARN_SECS,
            },
        );
        assert_eq!(PriceSource::Poll(feed).target().unwrap(), 500_000);
    }

    #[test]
    fn closed_settles_then_rolls_genesis_rolls() {
        assert_eq!(phase(EpochState::Closed, 300, 200, 2), Phase::Settle);
        assert_eq!(phase(EpochState::Closed, 300, 200, 0), Phase::Roll);
        assert_eq!(phase(EpochState::Genesis, 0, 0, 0), Phase::Roll);
    }

    #[test]
    fn parses_pool_worker_config() {
        let cfg = parse_pool_config(
            "conservative".into(),
            "5bdNVCZnVzUqirPNBqWY2Ehuxe6aFDsKnAUPD8aKY4yY".into(),
            "86400".into(),
        )
        .unwrap();
        assert_eq!(cfg.name, "conservative");
        assert_eq!(cfg.epoch_len, 86_400);
        assert_eq!(
            cfg.pool.to_string(),
            "5bdNVCZnVzUqirPNBqWY2Ehuxe6aFDsKnAUPD8aKY4yY"
        );
    }

    #[test]
    fn rejects_invalid_pool_worker_config() {
        assert!(parse_pool_config("bad-pool".into(), "nope".into(), "3600".into()).is_err());
        assert!(parse_pool_config(
            "bad-epoch".into(),
            "7zdb5RBTjnJsCReqj9ppV3BNJKBErekYAiSceevByrBQ".into(),
            "0".into(),
        )
        .is_err());
    }

    #[test]
    fn parses_private_key_byte_array() {
        let signer = Keypair::new();
        let value = serde_json::to_string(&signer.to_bytes().to_vec()).unwrap();
        let parsed = parse_keypair(&value, "TEST_PRIVATE_KEY").unwrap();
        assert_eq!(parsed.to_bytes(), signer.to_bytes());
    }

    #[test]
    fn rejects_invalid_private_key_byte_array() {
        assert!(parse_keypair("[1,2,3]", "TEST_PRIVATE_KEY").is_err());
        assert!(parse_keypair("not-json", "TEST_PRIVATE_KEY").is_err());
    }
}
