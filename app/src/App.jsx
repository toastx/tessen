import React, { useCallback, useEffect, useMemo, useState } from "react";
import { useConnection, useWallet } from "@solana/wallet-adapter-react";
import { WalletMultiButton } from "@solana/wallet-adapter-react-ui";
import { CLUSTERS } from "./cluster";
import { useBackend } from "./api";
import { PROGRAM_ID, poolPda, useOracle, useProgram, usePoolOnCluster, useLpShares, useTokenBalance } from "./chain";
import { clock, dur, sharePrice, short, ui, usd } from "./format";
import { Dashboard, Trade, Liquidity, Positions, History, Status } from "./tabs";

const TABS = [
  ["dashboard", "Dashboard", Dashboard], ["trade", "Trade", Trade],
  ["lp", "Liquidity", Liquidity], ["positions", "Positions", Positions],
  ["history", "History", History], ["status", "Status", Status]
];

const useNow = () => {
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => {
    const t = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000);
    return () => clearInterval(t);
  }, []);
  return now;
};

const Banner = ({ tone = "warn", children }) => (
  <div style={{
    display: "flex", alignItems: "center", gap: 12, padding: "12px 18px", marginBottom: 14,
    borderRadius: "var(--radius-lg)", fontSize: 13,
    background: tone === "warn" ? "var(--color-neutral-900)" : "color-mix(in srgb,var(--color-accent-900) 80%,transparent)",
    boxShadow: `0 0 0 1px ${tone === "warn" ? "var(--color-neutral-800)" : "var(--color-accent-700)"}`,
    color: tone === "warn" ? "var(--color-neutral-300)" : "var(--color-accent-300)"
  }}>{children}</div>
);

export default function App({ cluster, onCluster }) {
  const now = useNow();
  const { connection } = useConnection();
  const { publicKey } = useWallet();
  const owner = publicKey?.toBase58() ?? null;

  const { pool: indexed, epochs, positions, error, live, refresh } = useBackend(owner);
  // the backend serves pool state but not the pool's own address; its PDA seeds
  // are the mint and kind it already returns, so derive rather than configure
  const pool = useMemo(() => indexed && {
    ...indexed,
    pubkey: poolPda(indexed.collateral_mint, indexed.kind).toBase58(),
    program_id: PROGRAM_ID.toBase58()
  }, [indexed]);
  const program = useProgram();
  const oracle = useOracle(pool?.oracle);
  const poolOnCluster = usePoolOnCluster(pool?.pubkey);

  // bumped after every signed transaction to re-read wallet-side accounts
  const [nonce, setNonce] = useState(0);
  const shares = useLpShares(pool?.pubkey, publicKey, nonce);
  const balance = useTokenBalance(pool?.collateral_mint, publicKey, nonce);

  const [tab, setTab] = useState("dashboard");
  const [toast, setToast] = useState(null);
  const flash = useCallback(msg => {
    setToast(msg);
    setTimeout(() => setToast(t => (t === msg ? null : t)), 4500);
  }, []);
  const settled = useCallback(msg => { setNonce(n => n + 1); refresh(); flash(msg); }, [refresh, flash]);

  const epoch = useMemo(() => {
    if (!pool) return null;
    const prev = epochs.find(e => e.epoch === Number(pool.epoch) - 1);
    const end = Number(pool.epoch_end);
    const opened = prev ? Number(prev.epoch_end) : end - 86400;
    const secs = Math.max(0, end - now);
    return {
      end, opened, secs,
      pct: Math.min(100, Math.max(0, ((now - opened) / Math.max(1, end - opened)) * 100))
    };
  }, [pool, epochs, now]);

  const genesis = pool?.state === "genesis";
  const closed = pool?.state === "closed";
  const nav = sharePrice(pool);
  const spot = oracle?.spot ?? null;

  const ctx = {
    pool, epochs, positions, epoch, now, nav, spot, oracle, program, connection,
    publicKey, owner, shares, balance, flash, settled, setTab, cluster
  };

  const Body = TABS.find(([k]) => k === tab)?.[2];
  const showRibbon = tab !== "dashboard" || genesis;

  return (
    <div style={{ minHeight: "100vh", background: "var(--color-bg)", color: "var(--color-text)", fontFamily: "Inter,system-ui,sans-serif", paddingBottom: 48 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 22, padding: "18px 32px", maxWidth: 1280, margin: "0 auto" }}>
        <div style={{ display: "flex", alignItems: "baseline", gap: 9, marginRight: "auto" }}>
          <span style={{ fontSize: 19, fontWeight: 600, letterSpacing: "-0.02em" }}>stocklana</span>
          <span style={{ fontSize: 11, letterSpacing: "0.1em", textTransform: "uppercase", color: "var(--color-accent-500)" }}>cash-secured put vault</span>
        </div>
        <nav style={{ display: "flex", gap: 2, padding: 4, borderRadius: 999, background: "color-mix(in srgb,var(--color-surface) 85%,transparent)", boxShadow: "inset 0 1px 0 color-mix(in srgb,var(--color-text) 6%,transparent),0 0 0 1px color-mix(in srgb,var(--color-neutral-700) 45%,transparent)" }}>
          {TABS.map(([k, label]) => (
            <button key={k} onClick={() => setTab(k)} style={{
              background: tab === k ? "color-mix(in srgb,var(--color-accent-800) 75%,transparent)" : "transparent",
              border: 0, borderRadius: 999, cursor: "pointer", font: "500 13px Inter,system-ui,sans-serif",
              padding: "7px 14px", flex: "none", whiteSpace: "nowrap",
              boxShadow: tab === k ? "inset 0 1px 0 color-mix(in srgb,var(--color-accent-300) 18%,transparent)" : "none",
              color: tab === k ? "var(--color-text)" : "var(--color-neutral-500)"
            }}>{label}</button>
          ))}
        </nav>
        <select value={cluster} onChange={e => onCluster(e.target.value)} title="Cluster" style={{
          background: "var(--color-surface)", color: "var(--color-neutral-300)", fontSize: 12,
          border: "1px solid var(--color-divider)", borderRadius: 999, padding: "6px 10px", cursor: "pointer"
        }}>
          {Object.entries(CLUSTERS).map(([k, c]) => <option key={k} value={k}>{c.label}</option>)}
        </select>
        <WalletMultiButton />
      </div>

      <div style={{ maxWidth: 1280, margin: "0 auto", padding: "0 32px" }}>
        {error && <Banner>{error}</Banner>}
        {pool && !poolOnCluster && (
          <Banner>Pool <span className="mono">{short(pool.pubkey)}</span> is not on {CLUSTERS[cluster].label}. The backend indexes a different cluster — switch, or point the backend's <span className="mono">RPC_URL</span> here. Transactions will fail until they agree.</Banner>
        )}
        {!pool && !error && (
          <div style={{ display: "grid", gridTemplateColumns: "repeat(12,minmax(0,1fr))", gridAutoRows: "118px", gap: 14 }}>
            <div className="sk" style={{ gridColumn: "span 5", gridRow: "span 3" }} />
            <div className="sk" style={{ gridColumn: "span 7", gridRow: "span 2" }} />
            <div className="sk" style={{ gridColumn: "span 4" }} />
            <div className="sk" style={{ gridColumn: "span 3" }} />
          </div>
        )}

        {pool && showRibbon && (
          <div className="panel" style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: "20px 26px", padding: "16px 20px", marginBottom: 22 }}>
            <div style={{ display: "flex", flexDirection: "column", gap: 6, minWidth: 150 }}>
              <span className={"tag " + (pool.state === "open" ? "tag-accent" : "tag-neutral")} style={{ alignSelf: "flex-start" }}>
                {pool.state === "open" ? "Open · trading" : closed ? "Closed · settling" : "Genesis"}
              </span>
              <span style={{ fontSize: 12, color: "var(--color-neutral-500)" }}>Epoch {genesis ? "—" : pool.epoch}</span>
            </div>
            <div style={{ width: 1, height: 44, background: "linear-gradient(to bottom,transparent,var(--color-neutral-700),transparent)" }} />
            <div style={{ display: "flex", flexDirection: "column", gap: 3, minWidth: 230 }}>
              <span className="kicker">{genesis ? "No epoch open" : closed ? "Settlement window" : "Closes in"}</span>
              <span className="mono" style={{ fontSize: 26, fontWeight: 500, letterSpacing: "-0.02em", color: epoch && epoch.secs < 1800 && !closed && !genesis ? "var(--color-accent-400)" : "var(--color-text)" }}>
                {genesis ? "—" : closed ? "latching price" : dur(epoch.secs)}
              </span>
            </div>
            <div style={{ flex: "1 1 300px", display: "flex", flexDirection: "column", gap: 7, minWidth: 300 }}>
              <div style={{ height: 4, borderRadius: 999, background: "var(--color-neutral-900)", overflow: "hidden" }}>
                <div style={{ height: "100%", borderRadius: 999, background: "linear-gradient(90deg,var(--color-accent-700),var(--color-accent-500))", width: (genesis ? 0 : closed ? 100 : epoch.pct) + "%" }} />
              </div>
              <div className="mono" style={{ display: "flex", justifyContent: "space-between", gap: 16, fontSize: 11, color: "var(--color-neutral-600)", whiteSpace: "nowrap" }}>
                <span>opened {genesis ? "—" : clock(epoch.opened)}</span>
                <span>settles {genesis ? "—" : clock(epoch.end)} · 30-min median window</span>
              </div>
            </div>
            <div style={{ width: 1, height: 44, background: "linear-gradient(to bottom,transparent,var(--color-neutral-700),transparent)" }} />
            <div style={{ display: "flex", flexDirection: "column", gap: 3, textAlign: "right", minWidth: 120 }}>
              <span className="kicker">Oracle spot</span>
              <span className="mono" style={{ fontSize: 19 }}>{spot == null ? "—" : usd(ui(spot))}</span>
            </div>
          </div>
        )}

        {pool && Body && <Body {...ctx} />}
      </div>

      <div style={{ position: "fixed", right: 20, bottom: 14, display: "flex", alignItems: "center", gap: 8, fontSize: 11, color: "var(--color-neutral-700)" }}>
        <span style={{ width: 6, height: 6, borderRadius: "50%", background: live ? "var(--color-accent-500)" : "var(--color-neutral-700)" }} />
        {live ? "live" : "reconnecting"} · {CLUSTERS[cluster].label}
      </div>

      {toast && (
        <div style={{ position: "fixed", right: 28, bottom: 40, padding: "13px 18px", borderRadius: "var(--radius-lg)", background: "var(--color-surface)", boxShadow: "0 0 0 1px var(--color-neutral-700),0 16px 40px rgba(0,0,0,.65)", zIndex: 70, maxWidth: 380 }}>
          <div style={{ fontSize: 14, color: "var(--color-accent-300)" }}>{toast}</div>
        </div>
      )}
    </div>
  );
}
