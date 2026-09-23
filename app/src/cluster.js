import { clusterApiUrl } from "@solana/web3.js";

// Two clusters, switchable. `?cluster=` wins over the stored choice so a link
// can pin one; the choice then sticks for the tab.
export const CLUSTERS = {
  devnet: { label: "Devnet", endpoint: clusterApiUrl("devnet") },
  localnet: { label: "Localnet", endpoint: "http://127.0.0.1:8899" }
};

const KEY = "stocklana.cluster";

export function initialCluster() {
  const q = new URLSearchParams(location.search).get("cluster");
  if (q && CLUSTERS[q]) return q;
  const saved = localStorage.getItem(KEY);
  return saved && CLUSTERS[saved] ? saved : "devnet";
}

export const rememberCluster = c => localStorage.setItem(KEY, c);
