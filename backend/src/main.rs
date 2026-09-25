//! Tessen backend — read API + quote signer + live cache for every pool.
//!
//! Pieces:
//!   * indexer  — polls the chain, reconciles pool/positions/epochs into SQLite,
//!                broadcasts every change to websocket clients.
//!   * cache    — SQLite (db.rs); read endpoints hit it, never RPC.
//!   * quotes   — /quote prices an option, /buy builds a buy_option tx already
//!                signed by the pool's quote_signer for the frontend to co-sign.
//!   * /ws      — pushes indexer updates to connected frontends.
//!
//! Config (env):
//!   RPC_URL              default http://127.0.0.1:8899
//!   PROGRAM_ID           default 548P3sxkEEeE1L935jh4y7Tcosp5zUjCeR7Nn7NL1MHr
//!   QUOTE_SIGNER_PRIVATE_KEY JSON byte array for the shared quote signer
//!   BIND                 default 0.0.0.0:8080
//!   PORT                 hosting fallback when BIND is unset
//!   DB_PATH              default tessen.db
//!   POLL_SECS            default 5
//!   SPREAD_BPS           default 200  (2% of collateral, added over intrinsic)
//!   QUOTE_TTL_SECS       default 60   (< on-chain MAX_QUOTE_TTL of 300)
//!
//! ponytail: polling indexer (not accountSubscribe), server-side re-pricing on
//! /buy. Websocket chain-subscribe can replace the poll loop when needed.

mod chain;
mod db;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use tokio::sync::broadcast;

use chain::{price, spot_from, Chain};
use db::Db;
use tessen::{EpochRecord, EpochState, OptionPosition, Oracle, Pool};

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type ApiErr = (StatusCode, String);

struct Cfg {
    program_id: Pubkey,
    quote_signer: Keypair,
    spread_bps: u64,
    quote_ttl: i64,
    poll: Duration,
}

#[derive(Clone)]
struct AppState {
    db: Arc<Db>,
    chain: Arc<Chain>,
    cfg: Arc<Cfg>,
    tx: broadcast::Sender<String>,
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

fn load_dotenv() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    let _ = dotenvy::from_path(path);
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn oops<E: std::fmt::Display>(e: E) -> ApiErr {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn load_cfg() -> Res<Cfg> {
    Ok(Cfg {
        program_id: env("PROGRAM_ID")
            .unwrap_or_else(|| "548P3sxkEEeE1L935jh4y7Tcosp5zUjCeR7Nn7NL1MHr".into())
            .parse()?,
        quote_signer: load_quote_signer()?,
        spread_bps: env("SPREAD_BPS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(200),
        quote_ttl: env("QUOTE_TTL_SECS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(60),
        poll: Duration::from_secs(env("POLL_SECS").and_then(|s| s.parse().ok()).unwrap_or(5)),
    })
}

fn load_quote_signer() -> Res<Keypair> {
    let value = env("QUOTE_SIGNER_PRIVATE_KEY").ok_or("QUOTE_SIGNER_PRIVATE_KEY is required")?;
    parse_quote_signer(&value)
}

fn parse_quote_signer(value: &str) -> Res<Keypair> {
    let bytes: Vec<u8> = serde_json::from_str(value)
        .map_err(|e| format!("QUOTE_SIGNER_PRIVATE_KEY must be a JSON byte array: {e}"))?;
    Keypair::try_from(bytes.as_slice())
        .map_err(|e| format!("invalid QUOTE_SIGNER_PRIVATE_KEY: {e}").into())
}

fn state_str(s: EpochState) -> &'static str {
    match s {
        EpochState::Genesis => "genesis",
        EpochState::Open => "open",
        EpochState::Closed => "closed",
    }
}

fn pool_json(key: &Pubkey, p: &Pool) -> Value {
    json!({
        "pubkey": key.to_string(),
        "authority": p.authority.to_string(),
        "keeper": p.keeper.to_string(),
        "collateral_mint": p.collateral_mint.to_string(),
        "vault": p.vault.to_string(),
        "oracle": p.oracle.to_string(),
        "quote_signer": p.quote_signer.to_string(),
        "pool_name": p.pool_name,
        "total_shares": p.total_shares,
        "locked": p.locked,
        "available": p.available,
        "assets": p.available as u128 + p.locked as u128,
        "epoch": p.epoch,
        "epoch_end": p.epoch_end,
        "settle_price": p.settle_price,
        "state": state_str(p.state),
        "open_positions": p.open_positions,
        "epoch_premium": p.epoch_premium,
        "epoch_collateral": p.epoch_collateral,
        "kind": p.kind,
    })
}

fn position_json(p: &OptionPosition) -> Value {
    json!({
        "owner": p.owner.to_string(),
        "pool": p.pool.to_string(),
        "id": p.id,
        "kind": p.kind,
        "strike": p.strike,
        "size": p.size,
        "expiry": p.expiry,
        "epoch": p.epoch,
        "collateral": p.collateral,
        "premium": p.premium,
        "payout": p.payout,
        "settled": p.settled,
    })
}

fn epoch_json(r: &EpochRecord) -> Value {
    json!({
        "epoch": r.epoch,
        "epoch_end": r.epoch_end,
        "settle_price": r.settle_price,
        "settle_slot": r.settle_slot,
        "settle_ts": r.settle_ts,
        "sample_count": r.sample_count,
        "total_premium": r.total_premium,
        "total_collateral": r.total_collateral,
        "total_payout": r.total_payout,
        "positions_written": r.positions_written,
        "positions_settled": r.positions_settled,
        "share_price_open": r.share_price_open,
        "share_price_close": r.share_price_close,
        "final": r.positions_settled == r.positions_written,
    })
}

/// One reconciliation pass for every pool, position and epoch record.
async fn index_once(s: &AppState) -> Res<usize> {
    let ts = now();
    let pools = s.chain.all::<Pool>().await?;
    let pool_keys = pools.iter().map(|(key, _)| *key).collect::<HashSet<_>>();

    for (key, pool) in &pools {
        let pool_str = key.to_string();
        let pj = pool_json(key, pool);
        s.db.upsert(
            &pool_str,
            "pool",
            Some(&pool_str),
            None,
            &pj.to_string(),
            ts,
        )?;
        let _ =
            s.tx.send(json!({"type": "pool", "pubkey": pool_str, "data": pj}).to_string());
    }

    for (pk, pos) in s.chain.all::<OptionPosition>().await? {
        if !pool_keys.contains(&pos.pool) {
            continue;
        }
        let pool_str = pos.pool.to_string();
        let pj = position_json(&pos);
        s.db.upsert(
            &pk.to_string(),
            "position",
            Some(&pool_str),
            Some(&pos.owner.to_string()),
            &pj.to_string(),
            ts,
        )?;
        let _ = s
            .tx
            .send(json!({"type": "position", "pubkey": pk.to_string(), "data": pj}).to_string());
    }

    for (pk, rec) in s.chain.all::<EpochRecord>().await? {
        let Some(pool) = pools.iter().find_map(|(pool, _)| {
            let expected = Pubkey::find_program_address(
                &[b"epoch", pool.as_ref(), &rec.epoch.to_le_bytes()],
                &s.cfg.program_id,
            )
            .0;
            (expected == pk).then_some(*pool)
        }) else {
            continue;
        };
        s.db.upsert(
            &pk.to_string(),
            "epoch",
            Some(&pool.to_string()),
            None,
            &epoch_json(&rec).to_string(),
            ts,
        )?;
    }
    Ok(pools.len())
}

async fn indexer(state: AppState) {
    loop {
        tokio::time::sleep(state.cfg.poll).await;
        if let Err(e) = index_once(&state).await {
            eprintln!("index error: {e}");
        }
    }
}

// ---- handlers ----

async fn health() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
struct PoolQuery {
    pool: String,
}

async fn get_pools(State(s): State<AppState>) -> Result<Json<Value>, ApiErr> {
    rows_to_array(s.db.list("pool", None, None).map_err(oops)?)
}

async fn get_pool(
    State(s): State<AppState>,
    Query(q): Query<PoolQuery>,
) -> Result<Json<Value>, ApiErr> {
    match s.db.get(&q.pool).map_err(oops)? {
        Some(j) => Ok(Json(serde_json::from_str(&j).map_err(oops)?)),
        None => Err((StatusCode::NOT_FOUND, "pool not indexed yet".into())),
    }
}

fn rows_to_array(rows: Vec<String>) -> Result<Json<Value>, ApiErr> {
    let items = rows
        .into_iter()
        .map(|j| serde_json::from_str::<Value>(&j))
        .collect::<Result<Vec<_>, _>>()
        .map_err(oops)?;
    Ok(Json(Value::Array(items)))
}

async fn get_epochs(
    State(s): State<AppState>,
    Query(q): Query<PoolQuery>,
) -> Result<Json<Value>, ApiErr> {
    rows_to_array(s.db.list("epoch", Some(&q.pool), None).map_err(oops)?)
}

#[derive(Deserialize)]
struct PosQuery {
    pool: String,
    owner: Option<String>,
}

async fn get_positions(
    State(s): State<AppState>,
    Query(q): Query<PosQuery>,
) -> Result<Json<Value>, ApiErr> {
    rows_to_array(
        s.db.list("position", Some(&q.pool), q.owner.as_deref())
            .map_err(oops)?,
    )
}

#[derive(Deserialize)]
struct QuoteReq {
    pool: String,
    strike: u64,
    size: u64,
}

/// Shared pricing: live oracle median -> quote at/above intrinsic, checked
/// against available collateral. Returns the pool too, which /buy needs.
async fn priced(
    s: &AppState,
    pool_key: &Pubkey,
    strike: u64,
    size: u64,
) -> Result<(Pool, chain::Quote), ApiErr> {
    let pool = s.chain.account::<Pool>(pool_key).await.map_err(oops)?;
    let oracle = s
        .chain
        .account::<Oracle>(&pool.oracle)
        .await
        .map_err(oops)?;
    let spot = spot_from(&oracle).ok_or((StatusCode::CONFLICT, "oracle has no samples".into()))?;
    let q = price(
        pool.kind,
        strike,
        size,
        spot,
        s.cfg.spread_bps,
        s.cfg.quote_ttl,
        now(),
    )
    .map_err(oops)?;
    if q.collateral > pool.available {
        return Err((
            StatusCode::CONFLICT,
            "pool has insufficient available collateral".into(),
        ));
    }
    Ok((pool, q))
}

/// Price an option against the live oracle median, at/above intrinsic.
async fn quote(
    State(s): State<AppState>,
    Json(req): Json<QuoteReq>,
) -> Result<Json<Value>, ApiErr> {
    let pool = req
        .pool
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "bad pool pubkey".into()))?;
    let (_, q) = priced(&s, &pool, req.strike, req.size).await?;
    Ok(Json(serde_json::to_value(q).map_err(oops)?))
}

#[derive(Deserialize)]
struct BuyReq {
    pool: String,
    buyer: String,
    id: u64,
    strike: u64,
    size: u64,
}

/// Re-price server-side (never trust a client premium) and hand back a
/// buy_option tx already signed by the quote_signer.
async fn buy(State(s): State<AppState>, Json(req): Json<BuyReq>) -> Result<Json<Value>, ApiErr> {
    let buyer: Pubkey = req
        .buyer
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "bad buyer pubkey".into()))?;
    let pool_key: Pubkey = req
        .pool
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "bad pool pubkey".into()))?;
    let (pool, q) = priced(&s, &pool_key, req.strike, req.size).await?;
    if pool.quote_signer != s.cfg.quote_signer.pubkey() {
        return Err((
            StatusCode::CONFLICT,
            "configured quote signer does not match this pool".into(),
        ));
    }
    let tx = s
        .chain
        .build_buy(
            &pool_key,
            &pool,
            &buyer,
            req.id,
            req.strike,
            req.size,
            q.premium,
            q.quote_expiry,
            &s.cfg.quote_signer,
        )
        .await
        .map_err(oops)?;
    Ok(Json(json!({
        "transaction": tx,           // base64 bincode, quote_signer-signed; buyer co-signs + submits
        "premium": q.premium,
        "collateral": q.collateral,
        "quote_expiry": q.quote_expiry,
        "spot": q.spot,
    })))
}

async fn ws(State(s): State<AppState>, up: WebSocketUpgrade) -> impl IntoResponse {
    up.on_upgrade(move |socket| ws_loop(socket, s.tx.subscribe()))
}

async fn ws_loop(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    while let Ok(msg) = rx.recv().await {
        if socket.send(Message::Text(msg)).await.is_err() {
            break;
        }
    }
}

#[tokio::main]
async fn main() -> Res<()> {
    load_dotenv();
    let cfg = load_cfg()?;
    let rpc_url = env("RPC_URL").unwrap_or_else(|| "http://127.0.0.1:8899".into());
    let db_path = env("DB_PATH").unwrap_or_else(|| "tessen.db".into());
    let db = Db::open(&db_path)?;
    let chain = Chain::new(rpc_url.clone(), cfg.program_id);
    let (tx, _) = broadcast::channel::<String>(256);

    let state = AppState {
        db: Arc::new(db),
        chain: Arc::new(chain),
        cfg: Arc::new(cfg),
        tx,
    };

    let pool_count = index_once(&state).await?;
    if pool_count == 0 {
        return Err(format!(
            "no pools found for program {} at {rpc_url}",
            state.cfg.program_id
        )
        .into());
    }
    tokio::spawn(indexer(state.clone()));

    let app = Router::new()
        .route("/health", get(health))
        .route("/pools", get(get_pools))
        .route("/pool", get(get_pool))
        .route("/epochs", get(get_epochs))
        .route("/positions", get(get_positions))
        .route("/quote", post(quote))
        .route("/buy", post(buy))
        .route("/ws", get(ws))
        .with_state(state.clone());

    let bind = env("BIND")
        .or_else(|| env("PORT").map(|port| format!("0.0.0.0:{port}")))
        .unwrap_or_else(|| "0.0.0.0:8080".into());
    println!(
        "backend up on {bind}: pools={pool_count} rpc={rpc_url} db={db_path} quote_signer={}",
        state.cfg.quote_signer.pubkey()
    );
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quote_signer_private_key() {
        let signer = Keypair::new();
        let value = serde_json::to_string(&signer.to_bytes().to_vec()).unwrap();
        let parsed = parse_quote_signer(&value).unwrap();
        assert_eq!(parsed.to_bytes(), signer.to_bytes());
    }

    #[test]
    fn rejects_invalid_quote_signer_private_key() {
        assert!(parse_quote_signer("[1,2,3]").is_err());
        assert!(parse_quote_signer("not-json").is_err());
    }
}
