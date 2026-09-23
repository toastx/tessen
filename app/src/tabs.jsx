import React, { useEffect, useState } from "react";
import { useWallet } from "@solana/wallet-adapter-react";
import { WalletMultiButton } from "@solana/wallet-adapter-react-ui";
import { quote as quoteApi } from "./api";
import { buyOption, claim, deposit, newPositionId, withdraw } from "./chain";
import { clock, dur, num, raw, SCALE, short, strikeLadder, ui, usd } from "./format";

const TICKER = import.meta.env.VITE_TICKER || null;
const ticker = oracle => TICKER || (oracle ? short(oracle.underlying) : "underlying");

const Panel = ({ edge, style, ...p }) => (
  <div className="panel" style={{
    ...(edge ? { boxShadow: `inset 0 1px 0 color-mix(in srgb,var(--color-text) 6%,transparent),0 24px 48px -28px rgba(0,0,0,.8),0 0 0 1px ${edge}` } : null),
    ...style
  }} {...p} />
);

const Row = ({ label, value, color, labelColor }) => (
  <div className="rule" style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", gap: 12, padding: "9px 0" }}>
    <span style={{ fontSize: 13, color: labelColor || "var(--color-neutral-500)" }}>{label}</span>
    <span className="mono" style={{ fontSize: 14, color: color || "var(--color-text)" }}>{value}</span>
  </div>
);

const Empty = ({ title, body, cta, onCta }) => (
  <Panel style={{ padding: "52px 40px", maxWidth: 620 }}>
    <h4 style={{ margin: "0 0 8px" }}>{title}</h4>
    <p style={{ fontSize: 13, color: "var(--color-neutral-500)", margin: "0 0 16px", maxWidth: "52ch" }}>{body}</p>
    {cta && <button className="btn btn-primary" onClick={onCta}>{cta}</button>}
  </Panel>
);

/** Surface the program's own error message rather than the raw anchor dump. */
const reason = e => {
  const m = e?.error?.errorMessage || e?.message || String(e);
  return m.length > 160 ? m.slice(0, 157) + "…" : m;
};

const useTx = (settled, flash) => {
  const [busy, setBusy] = useState(false);
  const run = async (label, thunk) => {
    setBusy(true);
    try { settled(label(await thunk())); }
    catch (e) { flash("Failed — " + reason(e)); }
    finally { setBusy(false); }
  };
  return [busy, run];
};

// ─────────────────────────────────────────────────────────── Dashboard
export function Dashboard({ pool, epochs, epoch, nav, spot, oracle, setTab, publicKey }) {
  const genesis = pool.state === "genesis";
  const closed = pool.state === "closed";
  const assets = ui(pool.assets);
  const locked = ui(pool.locked);

  if (genesis) return (
    <Empty title="Pool hasn't started trading"
      body="The vault is deployed and accepting deposits, but no epoch has opened yet. NAV per share is fixed at 1.000000 until the first epoch rolls."
      cta="Deposit USDC" onCta={() => setTab("lp")} />
  );

  const C = 2 * Math.PI * 104, C2 = 2 * Math.PI * 120;
  const ringPct = closed ? 100 : epoch.pct;
  const span = Math.max(1, epoch.end - epoch.opened);
  const winW = C2 * Math.min(1, 1800 / span);

  const series = epochs.slice().reverse().map(e => ui(e.share_price_close)).filter(Boolean);
  const lo = Math.min(...series), hi = Math.max(...series);
  const pts = series.map((v, i) => [(i / Math.max(1, series.length - 1)) * 300, 80 - ((v - lo) / (hi - lo || 1)) * 62]);
  const navLine = pts.map((p, i) => (i ? "L" : "M") + p[0].toFixed(1) + "," + p[1].toFixed(1)).join(" ");
  const navDelta = series.length > 1 ? ((series[series.length - 1] / series[0] - 1) * 100).toFixed(2) : null;

  const presets = strikeLadder(ui(spot));
  const goTrade = strike => () => { sessionStorage.setItem("stocklana.strike", strike.toFixed(2)); setTab("trade"); };

  return (
    <>
      {closed && (
        <div style={{ display: "flex", alignItems: "center", gap: 12, padding: "13px 18px", borderRadius: 999, background: "color-mix(in srgb,var(--color-accent-900) 80%,transparent)", boxShadow: "0 0 0 1px color-mix(in srgb,var(--color-accent-700) 60%,transparent)", marginBottom: 14 }}>
          <span style={{ width: 14, height: 14, border: "2px solid var(--color-accent-400)", borderRightColor: "transparent", borderRadius: "50%", animation: "spin 0.9s linear infinite" }} />
          <span style={{ fontSize: 14, color: "var(--color-accent-300)" }}>
            Settlement in progress — price latched at {usd(ui(pool.settle_price))}. {pool.open_positions} position{pool.open_positions === 1 ? "" : "s"} still to write; the epoch rolls when all are settled.
          </span>
        </div>
      )}
      <div style={{ display: "grid", gridTemplateColumns: "repeat(12,minmax(0,1fr))", gridAutoRows: "minmax(118px,auto)", gridAutoFlow: "dense", gap: 14 }}>

        <Panel style={{ gridColumn: "span 5", gridRow: "span 3", padding: 22, display: "flex", flexDirection: "column", gap: 14 }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: 10 }}>
            <span className="kicker">Epoch {pool.epoch}</span>
            <span className={"tag " + (pool.state === "open" ? "tag-accent" : "tag-neutral")} style={{ borderRadius: 999 }}>
              {pool.state === "open" ? "Open · trading" : "Closed · settling"}
            </span>
          </div>
          <div style={{ position: "relative", alignSelf: "center", width: 236, height: 236, flex: "none" }}>
            <svg viewBox="0 0 240 240" width="236" height="236" style={{ display: "block", transform: "rotate(-90deg)", overflow: "visible" }}>
              <defs><linearGradient id="ringGrad" x1="0" y1="0" x2="1" y2="1">
                <stop offset="0" style={{ stopColor: "var(--color-accent-800)" }} />
                <stop offset="1" style={{ stopColor: "var(--color-accent-400)" }} />
              </linearGradient></defs>
              <circle cx="120" cy="120" r="104" fill="none" style={{ stroke: "var(--color-neutral-900)" }} strokeWidth="12" />
              <circle cx="120" cy="120" r="104" fill="none" strokeWidth="12" strokeLinecap="round"
                style={{ stroke: "url(#ringGrad)", filter: "drop-shadow(0 0 8px color-mix(in srgb,var(--color-accent-500) 45%,transparent))" }}
                strokeDasharray={`${(ringPct / 100 * C).toFixed(1)} ${C.toFixed(1)}`} />
              <circle cx="120" cy="120" r="120" fill="none" style={{ stroke: "var(--color-accent-300)" }} strokeWidth="2" strokeLinecap="round"
                strokeDasharray={`${winW.toFixed(1)} ${C2.toFixed(1)}`} strokeDashoffset={(-(C2 - winW)).toFixed(1)} />
            </svg>
            <div style={{ position: "absolute", inset: 0, display: "flex", flexDirection: "column", alignItems: "center", justifyContent: "center", gap: 4, textAlign: "center" }}>
              <span className="kicker">{closed ? "Settlement window" : "Closes in"}</span>
              <span className="mono" style={{ fontSize: 25, fontWeight: 500, letterSpacing: "-0.02em", color: epoch.secs < 1800 && !closed ? "var(--color-accent-400)" : "var(--color-text)" }}>
                {closed ? "latching price" : dur(epoch.secs)}
              </span>
              <span className="mono" style={{ fontSize: 12, color: "var(--color-neutral-500)" }}>settles {clock(epoch.end)}</span>
            </div>
          </div>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 10, marginTop: "auto" }}>
            <div style={{ display: "flex", flexDirection: "column", gap: 3, padding: "10px 14px", borderRadius: "var(--radius-lg)", background: "color-mix(in srgb,var(--color-neutral-900) 70%,transparent)" }}>
              <span className="kicker">Oracle spot</span>
              <span className="mono" style={{ fontSize: 17 }}>{spot == null ? "—" : usd(ui(spot))}</span>
            </div>
            <div style={{ display: "flex", flexDirection: "column", gap: 3, padding: "10px 14px", borderRadius: "var(--radius-lg)", background: "color-mix(in srgb,var(--color-neutral-900) 70%,transparent)" }}>
              <span className="kicker">Settlement</span>
              <span style={{ fontSize: 13, color: "var(--color-neutral-400)", lineHeight: 1.35 }}>30-min median, outer arc</span>
            </div>
          </div>
        </Panel>

        <Panel style={{ gridColumn: "span 7", gridRow: "span 2", padding: "22px 22px 0", display: "flex", flexDirection: "column", overflow: "hidden" }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "flex-start", gap: 20 }}>
            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
              <span className="kicker">Total value locked</span>
              <span className="mono" style={{ fontSize: 42, fontWeight: 500, letterSpacing: "-0.03em", lineHeight: 1.1 }}>{usd(assets, 0)}</span>
              <span style={{ fontSize: 12, color: "var(--color-neutral-600)" }}>{num(ui(pool.total_shares), 0)} shares</span>
            </div>
            <div style={{ display: "flex", flexDirection: "column", gap: 4, alignItems: "flex-end", textAlign: "right" }}>
              <span className="kicker">NAV / share</span>
              <span className="mono" style={{ fontSize: 24, fontWeight: 500 }}>{nav.toFixed(6)}</span>
              {navDelta && <span className="tag tag-accent mono" style={{ borderRadius: 999 }}>{navDelta >= 0 ? "+" : ""}{navDelta}% · {series.length} epochs</span>}
            </div>
          </div>
          {series.length > 1 && (
            <svg viewBox="0 0 300 90" preserveAspectRatio="none" style={{ display: "block", width: "calc(100% + 44px)", height: 108, margin: "auto -22px 0" }}>
              <defs><linearGradient id="navFill" x1="0" y1="0" x2="0" y2="1">
                <stop offset="0" style={{ stopColor: "var(--color-accent-600)", stopOpacity: 0.35 }} />
                <stop offset="1" style={{ stopColor: "var(--color-accent-600)", stopOpacity: 0 }} />
              </linearGradient></defs>
              <path d={navLine + " L300,90 L0,90 Z"} style={{ fill: "url(#navFill)" }} />
              <path d={navLine} fill="none" style={{ stroke: "var(--color-accent-400)" }} strokeWidth="1.6" vectorEffect="non-scaling-stroke" strokeLinejoin="round" />
            </svg>
          )}
        </Panel>

        <Panel style={{ gridColumn: "span 4", padding: "18px 20px", display: "flex", flexDirection: "column", gap: 10 }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline" }}>
            <span className="kicker">Utilisation</span>
            <span className="mono" style={{ fontSize: 22, fontWeight: 500 }}>{(assets ? (locked / assets) * 100 : 0).toFixed(1)}%</span>
          </div>
          <div style={{ height: 10, borderRadius: 999, background: "var(--color-neutral-900)", overflow: "hidden" }}>
            <div style={{ height: "100%", borderRadius: 999, background: "linear-gradient(90deg,var(--color-accent-800),var(--color-accent-500))", width: (assets ? (locked / assets) * 100 : 0) + "%" }} />
          </div>
          <div className="mono" style={{ display: "flex", justifyContent: "space-between", gap: 10, fontSize: 12, color: "var(--color-neutral-600)" }}>
            <span>{usd(locked)} locked</span><span>{usd(ui(pool.available))} free</span>
          </div>
        </Panel>

        <Panel style={{ gridColumn: "span 3", padding: "18px 20px", display: "flex", flexDirection: "column", gap: 4 }}>
          <span className="kicker">Premium · epoch</span>
          <span className="mono" style={{ fontSize: 24, fontWeight: 500, color: "var(--color-accent-400)" }}>{usd(ui(pool.epoch_premium))}</span>
          <span style={{ fontSize: 12, color: "var(--color-neutral-600)", marginTop: "auto" }}>{pool.open_positions} open positions</span>
        </Panel>

        <div className="panel" style={{ gridColumn: "span 4", gridRow: "span 2", padding: 22, display: "flex", flexDirection: "column", gap: 12, background: "radial-gradient(120% 120% at 100% 0%,color-mix(in srgb,var(--color-accent-800) 60%,var(--color-surface)) 0%,var(--color-surface) 55%,color-mix(in srgb,var(--color-surface) 55%,var(--color-bg)) 100%)", boxShadow: "inset 0 1px 0 color-mix(in srgb,var(--color-text) 6%,transparent),0 0 0 1px color-mix(in srgb,var(--color-accent-700) 55%,transparent),0 0 40px -12px color-mix(in srgb,var(--color-accent-600) 45%,transparent),0 24px 48px -28px rgba(0,0,0,.8)" }}>
          <span className="kicker" style={{ color: "var(--color-accent-300)" }}>Quick quote</span>
          <h3 style={{ margin: 0 }}>Buy a put on {ticker(oracle)}</h3>
          <p style={{ fontSize: 13, color: "var(--color-neutral-400)", margin: 0, lineHeight: 1.5 }}>Pick a strike for 1 contract. The quote opens on the Trade tab and is signed by the pool.</p>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8, marginTop: "auto" }}>
            {presets.map(v => (
              <button key={v} className="btn btn-secondary mono" onClick={goTrade(v)} disabled={pool.state !== "open" || !publicKey}
                style={{ borderRadius: 999, padding: "9px 12px", display: "flex", justifyContent: "space-between", gap: 8 }}>
                <span>{usd(v, 0)}</span>
                <span style={{ color: "var(--color-neutral-500)" }}>{v >= ui(spot) ? "ITM" : (((ui(spot) - v) / ui(spot)) * 100).toFixed(1) + "% OTM"}</span>
              </button>
            ))}
            {!presets.length && <span style={{ fontSize: 13, color: "var(--color-neutral-500)" }}>Waiting for an oracle sample.</span>}
          </div>
        </div>

        <Panel style={{ gridColumn: "span 5", gridRow: "span 2", padding: "18px 20px", display: "flex", flexDirection: "column" }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 6 }}>
            <span className="kicker">Recent settlements</span>
            <button className="btn btn-ghost" onClick={() => setTab("history")} style={{ fontSize: 12 }}>All epochs</button>
          </div>
          {epochs.slice(0, 4).map(e => {
            const net = ui(e.total_premium) - ui(e.total_payout);
            return (
              <div key={e.epoch} className="mono rule" style={{ display: "grid", gridTemplateColumns: "44px 1fr auto", gap: 12, alignItems: "baseline", padding: "11px 0" }}>
                <span style={{ fontSize: 12, color: "var(--color-neutral-500)" }}>#{e.epoch}</span>
                <span style={{ fontSize: 14 }}>{usd(ui(e.settle_price))}</span>
                <span style={{ fontSize: 14, color: net >= 0 ? "var(--color-accent-400)" : "var(--color-neutral-300)" }}>{net >= 0 ? "+" : "−"}{usd(Math.abs(net))}</span>
              </div>
            );
          })}
          {!epochs.length && <span style={{ fontSize: 13, color: "var(--color-neutral-600)", padding: "11px 0" }}>No epoch has settled yet.</span>}
        </Panel>

        <Panel style={{ gridColumn: "span 3", gridRow: "span 2", padding: "18px 20px", display: "flex", flexDirection: "column", gap: 12 }}>
          <span className="kicker">Instrument</span>
          {[
            ["Underlying", ticker(oracle)],
            ["Instrument", pool.kind === 0 ? "Cash-secured put" : "Covered call"],
            ["Collateral", short(pool.collateral_mint)],
            ["Settlement", "30-min median"]
          ].map(([label, value]) => (
            <div key={label} style={{ display: "flex", flexDirection: "column", gap: 2 }}>
              <span style={{ fontSize: 12, color: "var(--color-neutral-600)" }}>{label}</span>
              <span className="mono" style={{ fontSize: 13 }}>{value}</span>
            </div>
          ))}
        </Panel>
      </div>
    </>
  );
}

// ─────────────────────────────────────────────────────────── Trade
export function Trade({ pool, epoch, now, spot, oracle, connection, publicKey, settled, flash, setTab }) {
  const { signTransaction } = useWallet();
  const [strike, setStrike] = useState(() => sessionStorage.getItem("stocklana.strike") || "");
  const [size, setSize] = useState("1");
  const [q, setQ] = useState(null);       // {premium, collateral, intrinsic, spot, quote_expiry, at}
  const [err, setErr] = useState(null);
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  useEffect(() => { sessionStorage.removeItem("stocklana.strike"); }, []);

  const locked = pool.state !== "open" || !publicKey;
  const lockReason = !publicKey ? "Connect a wallet to request a quote."
    : pool.state === "closed" ? `Epoch closed for new options — the pool reopens when epoch ${Number(pool.epoch) + 1} rolls.`
    : "The pool hasn't opened an epoch yet.";

  const ttl = q ? Math.max(0, q.quote_expiry - now) : 0;
  const ttlMax = q ? Math.max(1, q.quote_expiry - q.at) : 1;
  const expired = !!q && ttl <= 0;

  const getQuote = async () => {
    setErr(null);
    let s, z;
    try { s = raw(strike); z = raw(size); }
    catch { setQ(null); return setErr({ code: 400, title: "Bad input", body: "Strike and size must both be positive numbers." }); }
    if (!s || !z) { setQ(null); return setErr({ code: 400, title: "Bad input", body: "Strike and size must both be greater than zero." }); }
    try {
      const r = await quoteApi(s, z);
      setQ({ ...r, strikeRaw: s, sizeRaw: z, at: Math.floor(Date.now() / 1000) });
    } catch (e) {
      setQ(null);
      setErr({ code: e.status || 500, title: e.status === 409 ? "Pool can't price this" : "Quote failed", body: String(e.message) });
    }
  };

  const doBuy = async () => {
    setConfirm(false);
    setBusy(true);
    try {
      const r = await buyOption(connection, signTransaction, publicKey, newPositionId(), q.strikeRaw, q.sizeRaw);
      setQ(null);
      settled(`Position opened · premium ${usd(ui(r.premium))} paid · ${short(r.sig)}`);
      setTab("positions");
    } catch (e) { flash("Buy failed — " + reason(e)); }
    finally { setBusy(false); }
  };

  const edge = err ? "var(--color-accent-700)" : q && !expired ? "var(--color-accent-700)" : "var(--color-neutral-800)";
  const presets = strikeLadder(ui(spot));

  return (
    <div style={{ display: "grid", gridTemplateColumns: "1fr 1.05fr", gap: 14, alignItems: "start" }}>
      <Panel style={{ padding: 20 }}>
        <h4 style={{ margin: "0 0 4px" }}>Buy a put</h4>
        <p style={{ fontSize: 13, color: "var(--color-neutral-500)", margin: "0 0 18px" }}>Cash-secured by the pool. Premium is always at or above intrinsic value.</p>

        <div className="field" style={{ marginBottom: 14 }}>
          <label htmlFor="strike">Strike price (USDC)</label>
          <input id="strike" className="input mono" inputMode="decimal" value={strike} placeholder="0.00"
            onChange={e => { setStrike(e.target.value); setQ(null); }} disabled={locked} />
          <div style={{ display: "flex", gap: 6, marginTop: 8 }}>
            {presets.map(v => (
              <button key={v} className="btn btn-secondary" onClick={() => { setStrike(v.toFixed(2)); setQ(null); setErr(null); }}
                style={{ fontSize: 12, padding: "4px 10px" }}>{usd(v, 0)}</button>
            ))}
          </div>
        </div>

        <div className="field" style={{ marginBottom: 18 }}>
          <label htmlFor="size">Size (contracts)</label>
          <input id="size" className="input mono" inputMode="decimal" value={size}
            onChange={e => { setSize(e.target.value); setQ(null); }} disabled={locked} />
          <div className="mono" style={{ fontSize: 11, color: "var(--color-neutral-600)", marginTop: 6 }}>
            size = {num(Math.round((parseFloat(size) || 0) * SCALE))} · 1 contract = 1 {ticker(oracle)}
          </div>
        </div>

        <button className="btn btn-primary btn-block" onClick={getQuote} disabled={locked} style={{ height: 40 }}>
          {q && !expired ? "Refresh quote" : "Get quote"}
        </button>

        {locked && (
          <div style={{ marginTop: 14, padding: "12px 14px", borderRadius: "var(--radius-lg)", background: "var(--color-neutral-900)", boxShadow: "0 0 0 1px var(--color-neutral-800)", fontSize: 13, color: "var(--color-neutral-400)" }}>
            {lockReason}
            {!publicKey && <div style={{ marginTop: 10 }}><WalletMultiButton /></div>}
          </div>
        )}

        <div className="rule rule-top" style={{ marginTop: 20, paddingTop: 16 }}>
          <div className="kicker" style={{ marginBottom: 8 }}>How this settles</div>
          <p style={{ fontSize: 12, color: "var(--color-neutral-600)", margin: 0, lineHeight: 1.6 }}>
            At {clock(epoch.end)} the keeper latches the 30-minute median oracle price. You are paid max(0, strike − settle) × size, capped at the collateral locked for the position. The live spot you see now is not the settlement price.
          </p>
        </div>
      </Panel>

      <Panel edge={edge} style={{ padding: 20, minHeight: 360, display: "flex", flexDirection: "column" }}>
        {!q && !err && (
          <div style={{ flex: 1, display: "flex", flexDirection: "column", justifyContent: "center", gap: 8, maxWidth: "38ch" }}>
            <h4 style={{ margin: 0, color: "var(--color-neutral-600)" }}>No quote yet</h4>
            <p style={{ fontSize: 13, color: "var(--color-neutral-600)", margin: 0 }}>Set a strike and size, then request a quote. Quotes are priced and signed by the pool's quote signer.</p>
          </div>
        )}

        {err && !q && (
          <div style={{ flex: 1, display: "flex", flexDirection: "column", justifyContent: "center", gap: 10, maxWidth: "40ch" }}>
            <span className="tag tag-outline" style={{ alignSelf: "flex-start" }}>HTTP {err.code}</span>
            <h4 style={{ margin: 0 }}>{err.title}</h4>
            <p style={{ fontSize: 13, color: "var(--color-neutral-400)", margin: 0 }}>{err.body}</p>
            <button className="btn btn-secondary" onClick={getQuote} style={{ alignSelf: "flex-start", marginTop: 4 }}>Try again</button>
          </div>
        )}

        {q && (
          <div style={{ flex: 1, display: "flex", flexDirection: "column" }}>
            <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", marginBottom: 18 }}>
              <div>
                <div className="kicker">Premium due</div>
                <div className="mono" style={{ fontSize: 38, fontWeight: 500, letterSpacing: "-0.03em", lineHeight: 1.15, color: expired ? "var(--color-neutral-600)" : "var(--color-text)" }}>{usd(ui(q.premium))}</div>
                <div className="mono" style={{ fontSize: 12, color: "var(--color-neutral-600)" }}>{usd(ui(q.premium) / (ui(q.sizeRaw) || 1), 4)} per contract</div>
              </div>
              <div style={{ display: "flex", flexDirection: "column", alignItems: "flex-end", gap: 5 }}>
                <span className={"tag " + (expired ? "tag-outline" : "tag-accent")}>{expired ? "Quote expired" : `valid ${ttl}s`}</span>
                <div style={{ width: 96, height: 3, borderRadius: 999, background: "var(--color-neutral-900)", overflow: "hidden" }}>
                  <div style={{ height: "100%", background: ttl <= 10 ? "var(--color-accent-400)" : "var(--color-accent-700)", width: (ttl / ttlMax) * 100 + "%" }} />
                </div>
              </div>
            </div>

            <Row label="Oracle spot" value={usd(ui(q.spot))} />
            <Row label="Intrinsic value" value={usd(ui(q.intrinsic))} />
            <Row label="Spread over intrinsic" value={usd(ui(q.premium) - ui(q.intrinsic))} />
            <Row label="Collateral locked by the pool" value={usd(ui(q.collateral))} />
            <Row label="Max payoff to you" value={usd(ui(q.collateral))} color="var(--color-accent-400)" />

            <div style={{ flex: 1 }} />
            <div style={{ display: "flex", gap: 10, marginTop: 20 }}>
              <button className="btn btn-primary" onClick={() => setConfirm(true)} disabled={expired || busy} style={{ flex: 1, height: 42, fontSize: 15 }}>
                {busy ? "Submitting…" : expired ? "Quote expired — re-quote" : `Buy put · ${usd(ui(q.premium))}`}
              </button>
              <button className="btn btn-secondary" onClick={getQuote} style={{ height: 42 }}>Re-quote</button>
            </div>
            <p style={{ fontSize: 11, color: "var(--color-neutral-600)", margin: "10px 0 0", lineHeight: 1.5 }}>
              The pool re-prices on submit; the final premium is shown in the wallet confirm before you sign.
            </p>
          </div>
        )}
      </Panel>

      {confirm && q && (
        <div className="dialog-backdrop" style={{ zIndex: 60 }}>
          <div className="dialog">
            <div className="dialog-title">Confirm purchase</div>
            <div className="dialog-body">
              <p style={{ margin: "0 0 12px" }}>The pool re-prices server-side on submit. Review, then sign in your wallet — the wallet shows the final premium.</p>
              <Row label="Position" value={`${ticker(oracle)} P ${usd(ui(q.strikeRaw))} × ${ui(q.sizeRaw)}`} />
              <Row label="Premium (quoted)" value={usd(ui(q.premium))} color="var(--color-accent-400)" />
              <Row label="Collateral locked" value={usd(ui(q.collateral))} />
              <Row label="Expiry" value={clock(epoch.end)} />
              <p style={{ fontSize: 12, color: "var(--color-neutral-600)", margin: "14px 0 0", lineHeight: 1.5 }}>
                Payoff is measured against the 30-minute median at {clock(epoch.end)}, not the current spot.
              </p>
            </div>
            <div className="dialog-actions">
              <button className="btn btn-secondary" onClick={() => setConfirm(false)}>Cancel</button>
              <button className="btn btn-primary" onClick={doBuy}>Sign &amp; submit</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ─────────────────────────────────────────────────────────── Liquidity
export function Liquidity({ pool, nav, program, publicKey, shares, balance, settled, flash, epoch }) {
  const [amt, setAmt] = useState("");
  const [pct, setPct] = useState(40);
  const [busy, run] = useTx(settled, flash);

  const gated = pool.open_positions > 0 || !publicKey;
  const gateText = !publicKey ? "Connect a wallet to deposit or withdraw."
    : pool.open_positions > 0
      ? `Deposits and withdrawals open between epochs — ${pool.open_positions} option${pool.open_positions === 1 ? " is" : "s are"} live right now. They reopen after settlement, in ${dur(epoch.secs)}.`
      : "Between epochs — no open positions. Deposits and withdrawals are enabled.";

  const dep = parseFloat(amt) || 0;
  const depShares = Number(pool.total_shares) ? dep / nav : dep;
  const wShares = shares * (pct / 100);
  const wOut = (wShares * nav) / SCALE;
  const tooBig = wOut > ui(pool.available);
  const overBalance = balance != null && dep > ui(balance);

  return (
    <>
      <div style={{ display: "flex", alignItems: "center", gap: 14, padding: "13px 18px", borderRadius: "var(--radius-lg)", marginBottom: 14, background: gated ? "var(--color-neutral-900)" : "var(--color-accent-900)", boxShadow: `0 0 0 1px ${gated ? "var(--color-neutral-800)" : "var(--color-accent-700)"}` }}>
        <span style={{ fontSize: 13, color: gated ? "var(--color-neutral-400)" : "var(--color-accent-300)" }}>{gateText}</span>
      </div>

      <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 0.9fr", gap: 14, alignItems: "start" }}>
        <Panel style={{ padding: 20 }}>
          <h4 style={{ margin: "0 0 4px" }}>Deposit</h4>
          <p style={{ fontSize: 13, color: "var(--color-neutral-500)", margin: "0 0 18px" }}>Mint shares at the current NAV. First deposit must be at least 1 USDC.</p>
          <div className="field" style={{ marginBottom: 14 }}>
            <label htmlFor="dep">Amount (USDC)</label>
            <input id="dep" className="input mono" inputMode="decimal" value={amt} placeholder="0.00"
              onChange={e => setAmt(e.target.value)} disabled={gated} />
            {balance != null && (
              <div className="mono" style={{ display: "flex", justifyContent: "space-between", fontSize: 11, color: "var(--color-neutral-600)", marginTop: 6 }}>
                <span>wallet {usd(ui(balance))}</span>
                <button className="btn btn-ghost" style={{ fontSize: 11 }} onClick={() => setAmt(String(ui(balance)))}>max</button>
              </div>
            )}
          </div>
          <Row label="Shares received" value={num(depShares, 4)} />
          <Row label="At NAV" value={nav.toFixed(6)} />
          <Row label="Resulting share of pool" value={
            (((shares / SCALE + depShares) / (ui(pool.total_shares) + depShares || 1)) * 100).toFixed(3) + "%"
          } />
          <button className="btn btn-primary btn-block" style={{ height: 40, marginTop: 16 }}
            disabled={gated || busy || dep <= 0 || overBalance}
            onClick={() => run(() => `Deposited ${usd(dep)} · ${num(depShares, 4)} shares minted`,
              () => deposit(program, pool, raw(dep)))}>
            {overBalance ? "Exceeds wallet balance" : busy ? "Submitting…" : "Deposit USDC"}
          </button>
        </Panel>

        <Panel style={{ padding: 20 }}>
          <h4 style={{ margin: "0 0 4px" }}>Withdraw</h4>
          <p style={{ fontSize: 13, color: "var(--color-neutral-500)", margin: "0 0 18px" }}>Redeem shares for USDC, bounded by the pool's available balance.</p>
          <div className="field" style={{ marginBottom: 6 }}>
            <label htmlFor="wd">Shares to redeem</label>
            <input id="wd" type="range" min="0" max="100" value={pct} onChange={e => setPct(Number(e.target.value))}
              disabled={gated || !shares} style={{ width: "100%", accentColor: "var(--color-accent-500)" }} />
          </div>
          <div className="mono" style={{ display: "flex", justifyContent: "space-between", fontSize: 12, color: "var(--color-neutral-600)", marginBottom: 16 }}>
            <span>{num(wShares / SCALE, 4)} shares</span><span>{pct}% of your position</span>
          </div>
          <Row label="USDC out" value={usd(wOut)} />
          <Row label="Pool available" value={usd(ui(pool.available))} color={tooBig ? "var(--color-accent-400)" : "var(--color-neutral-500)"} />
          <Row label="Shares remaining" value={num((shares - wShares) / SCALE, 4)} />
          <button className="btn btn-primary btn-block" style={{ height: 40, marginTop: 16 }}
            disabled={gated || busy || tooBig || wShares < 1}
            onClick={() => run(() => `Withdrew ${usd(wOut)}`, () => withdraw(program, pool, Math.floor(wShares)))}>
            {tooBig ? "More than the pool can pay now" : busy ? "Submitting…" : "Withdraw USDC"}
          </button>
        </Panel>

        <Panel style={{ padding: 20 }}>
          <h4 style={{ margin: "0 0 16px" }}>Your position</h4>
          <div style={{ display: "flex", flexDirection: "column", gap: 13 }}>
            {[
              ["Your shares", num(shares / SCALE, 4), null],
              ["Value at NAV", usd((shares * nav) / SCALE), null],
              ["Share of pool", (Number(pool.total_shares) ? (shares / Number(pool.total_shares)) * 100 : 0).toFixed(3) + "%", null],
              ["Wallet USDC", balance == null ? "—" : usd(ui(balance)), "var(--color-accent-400)"]
            ].map(([label, value, color]) => (
              <div key={label} style={{ display: "flex", flexDirection: "column", gap: 2 }}>
                <span className="kicker">{label}</span>
                <span className="mono" style={{ fontSize: 19, color: color || "var(--color-text)" }}>{value}</span>
              </div>
            ))}
          </div>
          <p style={{ fontSize: 12, color: "var(--color-neutral-600)", margin: "18px 0 0", lineHeight: 1.5 }}>
            NAV per share only moves at epoch boundaries, when premium is credited and payouts are paid out.
          </p>
        </Panel>
      </div>
    </>
  );
}

// ─────────────────────────────────────────────────────────── Positions
export function Positions({ pool, positions, oracle, program, publicKey, now, settled, flash, setTab }) {
  const [busy, run] = useTx(settled, flash);

  if (!publicKey) return <Empty title="Connect a wallet" body="Your positions in this pool are looked up by your wallet address." />;
  if (!positions.length) return (
    <Empty title="You haven't bought any options"
      body="Positions you buy in this pool appear here with their epoch, expiry and payout."
      cta="Get a quote" onCta={() => setTab("trade")} />
  );

  const rows = positions.slice().sort((a, b) => b.epoch - a.epoch || b.id - a.id).map(p => {
    const expired = now >= Number(p.expiry);
    const state = p.settled ? (Number(p.payout) > 0 ? "claim" : "worth") : expired ? "await" : "active";
    return { p, state, ...{
      active: { status: "Active", tag: "tag-accent", label: "Running", cls: "btn-secondary", off: true, edge: "var(--color-neutral-800)" },
      await: { status: "Expired — awaiting settlement", tag: "tag-neutral", label: "Keeper settles", cls: "btn-secondary", off: true, edge: "var(--color-neutral-800)" },
      claim: { status: "Settled — claimable", tag: "tag-outline", label: "Claim payout", cls: "btn-primary", off: false, edge: "var(--color-accent-700)" },
      worth: { status: "Settled — expired worthless", tag: "tag-neutral", label: "Dismiss", cls: "btn-secondary", off: false, edge: "var(--color-neutral-800)" }
    }[state] };
  });

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
      {rows.map(({ p, status, tag, label, cls, off, edge }) => (
        <Panel key={p.id} edge={edge} style={{ display: "grid", gridTemplateColumns: "minmax(200px,240px) repeat(4,minmax(0,1fr)) auto", gap: 18, alignItems: "center", padding: "16px 20px" }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 5 }}>
            <span className="mono" style={{ fontSize: 16 }}>{ticker(oracle)} P {usd(ui(p.strike))} × {ui(p.size)}</span>
            <span className={"tag " + tag} style={{ alignSelf: "flex-start", whiteSpace: "nowrap" }}>{status}</span>
          </div>
          {[
            ["Premium paid", usd(ui(p.premium)), null],
            ["Collateral", usd(ui(p.collateral)), null],
            ["Epoch / expiry", `#${p.epoch} · ${clock(Number(p.expiry))}`, null],
            ["Payout", p.settled ? usd(ui(p.payout)) : "—", p.settled && Number(p.payout) > 0 ? "var(--color-accent-400)" : "var(--color-neutral-500)"]
          ].map(([k, v, c]) => (
            <div key={k} style={{ display: "flex", flexDirection: "column", gap: 2 }}>
              <span className="kicker">{k}</span>
              <span className="mono" style={{ fontSize: 15, color: c || "var(--color-text)" }}>{v}</span>
            </div>
          ))}
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <button className={"btn " + cls} disabled={off || busy} style={{ whiteSpace: "nowrap" }}
              onClick={() => run(() => Number(p.payout) > 0
                ? `Claimed ${usd(ui(p.payout))} · account closed, rent refunded`
                : "Position closed · rent refunded", () => claim(program, pool, p))}>
              {busy ? "…" : label}
            </button>
          </div>
        </Panel>
      ))}
    </div>
  );
}

// ─────────────────────────────────────────────────────────── History
export function History({ epochs }) {
  if (!epochs.length) return <Empty title="No settled epochs yet" body="Every closed epoch is recorded permanently in an EpochRecord PDA and shows up here." />;
  return (
    <Panel style={{ padding: "8px 4px 20px" }}>
      <table className="table mono">
        <thead><tr>
          <th style={{ paddingLeft: 20 }}>Epoch</th><th>Settle price</th><th>Settled</th><th>Samples</th>
          <th>Premium</th><th>Payout</th><th>Net to LPs</th><th>Share price</th><th style={{ paddingRight: 20 }}>State</th>
        </tr></thead>
        <tbody>
          {epochs.map(e => {
            const net = ui(e.total_premium) - ui(e.total_payout);
            return (
              <tr key={e.epoch}>
                <td style={{ paddingLeft: 20 }}>#{e.epoch}</td>
                <td>{usd(ui(e.settle_price))}</td>
                <td style={{ color: "var(--color-neutral-500)" }}>{clock(Number(e.settle_ts))}</td>
                <td style={{ color: "var(--color-neutral-500)" }}>{e.sample_count}</td>
                <td>{usd(ui(e.total_premium))}</td>
                <td>{Number(e.total_payout) ? "−" + usd(ui(e.total_payout)) : "$0.00"}</td>
                <td style={{ color: net >= 0 ? "var(--color-accent-400)" : "var(--color-neutral-300)" }}>{net >= 0 ? "+" : "−"}{usd(Math.abs(net))}</td>
                <td style={{ color: "var(--color-neutral-500)" }}>{ui(e.share_price_open).toFixed(6)} → {ui(e.share_price_close).toFixed(6)}</td>
                <td style={{ paddingRight: 20 }}>
                  <span className={"tag " + (e.final ? "tag-neutral" : "tag-outline")}>
                    {e.final ? "final" : `settling ${e.positions_settled}/${e.positions_written}`}
                  </span>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </Panel>
  );
}

// ─────────────────────────────────────────────────────────── Status
export function Status({ pool, epoch, oracle, now }) {
  const age = oracle ? Math.max(0, now - oracle.lastUpdate) : null;
  const stale = age != null && age > 900;
  return (
    <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 14, alignItems: "start" }}>
      <Panel style={{ padding: 20 }}>
        <h4 style={{ margin: "0 0 16px" }}>Oracle</h4>
        <div style={{ display: "flex", alignItems: "baseline", gap: 10, marginBottom: 10 }}>
          <span className="mono" style={{ fontSize: 32, fontWeight: 500, color: stale ? "var(--color-accent-400)" : "var(--color-text)" }}>{age == null ? "—" : age + "s"}</span>
          <span style={{ fontSize: 13, color: "var(--color-neutral-500)" }}>since last sample · 900s staleness limit</span>
        </div>
        <div style={{ height: 4, borderRadius: 999, background: "var(--color-neutral-900)", overflow: "hidden", marginBottom: 18 }}>
          <div style={{ height: "100%", background: stale ? "var(--color-accent-300)" : "var(--color-accent-500)", width: Math.min(100, ((age || 0) / 900) * 100) + "%" }} />
        </div>
        <Row label="Ring samples" value={oracle ? `${oracle.count} / 32 filled` : "—"} />
        <Row label="Median (spot)" value={oracle?.spot == null ? "—" : usd(ui(oracle.spot))} />
        <Row label="Median window" value={"30 min ending " + clock(epoch.end)} />
        <Row label="Open positions" value={String(pool.open_positions)} />
        <Row label="Settlement" value={pool.state === "closed" ? `latched at ${usd(ui(pool.settle_price))}` : "idle"} />
      </Panel>
      <Panel style={{ padding: 20 }}>
        <h4 style={{ margin: "0 0 4px" }}>Accounts</h4>
        <p style={{ fontSize: 13, color: "var(--color-neutral-500)", margin: "0 0 16px" }}>Read-only. The keeper and authority act out of band.</p>
        {[
          ["Program", pool.program_id], ["Pool", pool.pubkey], ["Vault", pool.vault],
          ["Oracle", pool.oracle], ["Quote signer", pool.quote_signer],
          ["Keeper", pool.keeper], ["Authority", pool.authority]
        ].map(([label, value]) => (
          <div key={label} className="rule" style={{ display: "flex", justifyContent: "space-between", gap: 12, padding: "9px 0" }}>
            <span style={{ fontSize: 13, color: "var(--color-neutral-500)" }}>{label}</span>
            <span className="mono" style={{ fontSize: 13, color: "var(--color-accent-400)" }} title={value}>{short(value)}</span>
          </div>
        ))}
        <p className="mono" style={{ fontSize: 11, color: "var(--color-neutral-600)", margin: "16px 0 0" }}>
          raw · assets {num(pool.assets)} · shares {num(pool.total_shares)} (1e6-scaled)
        </p>
      </Panel>
    </div>
  );
}
