// Backend reads. The indexer is authoritative for pool/positions/epochs; the
// websocket pushes every change, so polling is only the reconnect fallback.
import { useCallback, useEffect, useRef, useState } from "react";

const BACKEND_URL = (import.meta.env.VITE_BACKEND_URL ||
  "https://backend.tessen.xyz").replace(/\/$/, "");

const j = async (path, init) => {
  const r = await fetch(BACKEND_URL + path, init);
  const body = await r.text();
  if (!r.ok) throw Object.assign(new Error(body || r.statusText), { status: r.status });
  return body ? JSON.parse(body) : null;
};

// Pools whose epoch_end passed while no keeper was running. close_epoch needs
// oracle samples timestamped inside the settlement window, those rotated out of
// the 32-slot ring long ago, and no later push can be stamped into the past — so
// these can never close, roll or quote again. The backend indexes every Pool
// account on chain, dead ones included, so the filter lives here.
const RETIRED = new Set([
  "7PFdFWATxJfecWKSwhAnFH29b9uHCmGVpa4MqZWG3CJX", // POOL:ANTHROPIC:1
  "421TXEePi5d5JaDaudmSaZnn4CurRZY8P7BUgYpQfGe7", // POOL:ANTHROPIC:2
  "DPnaJ8hm5qA4fY5xyJpi2YSZnf9iGwFCAM7h99qEcZw7", // POOL:ANTHROPIC:3
  "Hu2tdEtFAf6oSpWwPPGXZcsqQik98ys5Y8c9VzyufXfm", // POOL:ANTHROPIC:4
]);

const query = values => "?" + new URLSearchParams(values);
export const getPools = async () => (await j("/pools")).filter(pool => !RETIRED.has(pool.pubkey));
export const getEpochs = pool => j("/epochs" + query({ pool }));
export const getPositions = (pool, owner) => j("/positions" + query({ pool, ...(owner ? { owner } : {}) }));

const post = (path, body) =>
  j(path, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });

/** Price an option. strike/size are raw 1e6 integers. */
export const quote = (pool, strike, size) => post("/quote", { pool, strike, size });
/** Returns a buy_option tx already signed by the pool's quote_signer. */
export const buildBuy = (pool, buyer, id, strike, size) => post("/buy", { pool, buyer, id, strike, size });

export function useBackend(owner) {
  const [state, setState] = useState({ pools: [], pool: null, epochs: [], positions: [], error: null, live: false });
  const [poolKey, setPoolKey] = useState(() => sessionStorage.getItem("tessen.pool"));
  const ownerRef = useRef(owner);
  const poolRef = useRef(poolKey);
  ownerRef.current = owner;
  poolRef.current = poolKey;

  const refresh = useCallback(async () => {
    try {
      const pools = await getPools();
      const selected = pools.find(pool => pool.pubkey === poolRef.current) || pools[0] || null;
      const selectedKey = selected?.pubkey || null;
      if (selectedKey !== poolRef.current) {
        poolRef.current = selectedKey;
        setPoolKey(selectedKey);
      }
      const [epochs, positions] = selectedKey ? await Promise.all([
        getEpochs(selectedKey), ownerRef.current ? getPositions(selectedKey, ownerRef.current) : Promise.resolve([])
      ]) : [[], []];
      epochs.sort((a, b) => b.epoch - a.epoch);
      setState(s => ({ ...s, pools, pool: selected, epochs, positions, error: null }));
    } catch (e) {
      setState(s => ({ ...s, error: e.status === 404 ? "Backend is up but hasn't indexed any pools yet." : "Backend unreachable — is the backend running?" }));
    }
  }, []);

  const selectPool = useCallback(key => {
    poolRef.current = key;
    setPoolKey(key);
    sessionStorage.setItem("tessen.pool", key);
  }, []);

  useEffect(() => { refresh(); }, [refresh, owner, poolKey]);

  // live updates; the poll is what covers a dropped socket
  useEffect(() => {
    const url = BACKEND_URL.startsWith("/")
      ? (location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws"
      : BACKEND_URL.replace(/^http/, "ws") + "/ws";
    let ws, closed = false;
    const open = () => {
      ws = new WebSocket(url);
      ws.onopen = () => setState(s => ({ ...s, live: true }));
      ws.onclose = () => { setState(s => ({ ...s, live: false })); if (!closed) setTimeout(open, 3000); };
      ws.onmessage = ev => {
        const m = JSON.parse(ev.data);
        if (m.type === "pool" && !RETIRED.has(m.pubkey)) setState(s => ({
          ...s,
          pools: s.pools.some(pool => pool.pubkey === m.pubkey)
            ? s.pools.map(pool => pool.pubkey === m.pubkey ? m.data : pool)
            : [...s.pools, m.data],
          pool: m.pubkey === poolRef.current ? m.data : s.pool
        }));
        // a position change also moves pool totals and may finalise an epoch
        if (m.type === "position" && m.data.owner === ownerRef.current && m.data.pool === poolRef.current) refresh();
      };
    };
    open();
    const poll = setInterval(refresh, 15000);
    return () => { closed = true; clearInterval(poll); ws && ws.close(); };
  }, [refresh]);

  return { ...state, selectPool, refresh };
}
