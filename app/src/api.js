// Backend reads. The indexer is authoritative for pool/positions/epochs; the
// websocket pushes every change, so polling is only the reconnect fallback.
import { useCallback, useEffect, useRef, useState } from "react";

const j = async (path, init) => {
  const r = await fetch("/api" + path, init);
  const body = await r.text();
  if (!r.ok) throw Object.assign(new Error(body || r.statusText), { status: r.status });
  return body ? JSON.parse(body) : null;
};

export const getPool = () => j("/pool");
export const getEpochs = () => j("/epochs");
export const getPositions = owner => j("/positions" + (owner ? "?owner=" + owner : ""));

const post = (path, body) =>
  j(path, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });

/** Price an option. strike/size are raw 1e6 integers. */
export const quote = (strike, size) => post("/quote", { strike, size });
/** Returns a buy_option tx already signed by the pool's quote_signer. */
export const buildBuy = (buyer, id, strike, size) => post("/buy", { buyer, id, strike, size });

export function useBackend(owner) {
  const [state, setState] = useState({ pool: null, epochs: [], positions: [], error: null, live: false });
  const ownerRef = useRef(owner);
  ownerRef.current = owner;

  const refresh = useCallback(async () => {
    try {
      const [pool, epochs, positions] = await Promise.all([
        getPool(), getEpochs(), ownerRef.current ? getPositions(ownerRef.current) : Promise.resolve([])
      ]);
      epochs.sort((a, b) => b.epoch - a.epoch);
      setState(s => ({ ...s, pool, epochs, positions, error: null }));
    } catch (e) {
      setState(s => ({ ...s, error: e.status === 404 ? "Backend is up but hasn't indexed the pool yet." : "Backend unreachable — is `cargo run -p stocklana-backend` running?" }));
    }
  }, []);

  useEffect(() => { refresh(); }, [refresh, owner]);

  // live updates; the poll is what covers a dropped socket
  useEffect(() => {
    const url = (location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws";
    let ws, closed = false;
    const open = () => {
      ws = new WebSocket(url);
      ws.onopen = () => setState(s => ({ ...s, live: true }));
      ws.onclose = () => { setState(s => ({ ...s, live: false })); if (!closed) setTimeout(open, 3000); };
      ws.onmessage = ev => {
        const m = JSON.parse(ev.data);
        if (m.type === "pool") setState(s => ({ ...s, pool: m.data }));
        // a position change also moves pool totals and may finalise an epoch
        if (m.type === "position" && m.data.owner === ownerRef.current) refresh();
      };
    };
    open();
    const poll = setInterval(refresh, 15000);
    return () => { closed = true; clearInterval(poll); ws && ws.close(); };
  }, [refresh]);

  return { ...state, refresh };
}
