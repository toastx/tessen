// Browser smoke test: boots the app, clicks through every tab, and — if the
// local keypairs are around — connects a stub Wallet Standard wallet holding
// the buyer's pubkey and checks MAX fills the deposit box with the real
// on-chain balance. Fails on any uncaught error.
//
// Catches what the bundler can't: module-eval ordering (the Buffer polyfill),
// render-time crashes on real indexed data, duplicate React keys.
//
// Needs the validator, the backend and `npm run dev` up. Run: npm run smoke
//   PLAYWRIGHT_CHROMIUM=<path>  reuse an already-cached chromium build
import { chromium } from "playwright";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";
import fs from "node:fs";

const URL = process.argv[2] || "http://localhost:5173/?cluster=localnet";
const KEY = "../keys/buyer.json";
const errors = [];

const b = await chromium.launch(process.env.PLAYWRIGHT_CHROMIUM ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM } : {});
const p = await b.newPage();
p.on("pageerror", e => errors.push(String(e).split("\n")[0]));
p.on("console", m => m.type() === "error" && errors.push("console: " + m.text().slice(0, 160)));

// ── optional: a stub wallet, so the wallet-gated UI is exercised too
let wallet = null;
if (fs.existsSync(KEY)) {
  const kp = Keypair.fromSecretKey(new Uint8Array(JSON.parse(fs.readFileSync(KEY, "utf8"))));
  const pool = await (await fetch("http://127.0.0.1:8080/pool")).json();
  const ata = getAssociatedTokenAddressSync(new PublicKey(pool.collateral_mint), kp.publicKey);
  const bal = await new Connection("http://127.0.0.1:8899", "confirmed")
    .getTokenAccountBalance(ata).then(r => r.value.amount).catch(() => null);
  if (bal && bal !== "0") {
    wallet = { address: kp.publicKey.toBase58(), expected: String(Number(bal) / 1e6) };
    await p.addInitScript(({ address, bytes }) => {
      const account = {
        address, publicKey: Uint8Array.from(bytes),
        chains: ["solana:localnet", "solana:devnet", "solana:mainnet"],
        features: ["solana:signTransaction"], label: "Test"
      };
      const w = {
        version: "1.0.0", name: "TestWallet",
        icon: "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciLz4=",
        chains: account.chains, accounts: [],
        features: {
          "standard:connect": { version: "1.0.0", connect: async () => ({ accounts: (w.accounts = [account]) }) },
          "standard:disconnect": { version: "1.0.0", disconnect: async () => { w.accounts = []; } },
          "standard:events": { version: "1.0.0", on: () => () => {} },
          // the smoke test never signs; buying is covered by ixcheck.mjs
          "solana:signTransaction": { version: "1.0.0", supportedTransactionVersions: ["legacy", 0], signTransaction: async () => { throw new Error("stub"); } }
        }
      };
      const register = api => api.register(w);
      window.addEventListener("wallet-standard:app-ready", ({ detail }) => register(detail));
      window.dispatchEvent(new CustomEvent("wallet-standard:register-wallet", { detail: register }));
    }, { address: wallet.address, bytes: [...kp.publicKey.toBytes()] });
  }
}
if (!wallet) console.log("(no funded keys/buyer.json — running without a wallet)");

await p.goto(URL, { waitUntil: "networkidle", timeout: 30000 });
await p.waitForTimeout(2000);

const buffer = await p.evaluate(() => typeof globalThis.Buffer);
if (buffer !== "function") errors.push("Buffer polyfill did not run (typeof = " + buffer + ")");

for (const tab of ["Dashboard", "Trade", "Liquidity", "Positions", "History", "Status"]) {
  await p.getByRole("button", { name: tab, exact: true }).click();
  await p.waitForTimeout(900);
  const body = (await p.evaluate(() => document.body.innerText)).replace(/\s+/g, " ");
  if (body.length < 80) errors.push(`${tab}: rendered nothing`);
  console.log(tab.padEnd(10), body.slice(body.indexOf("Wallet") + 7, body.indexOf("Wallet") + 120).trim());
}

// ── MAX, on the Liquidity tab we're already on
await p.getByRole("button", { name: "Liquidity", exact: true }).click();
await p.waitForTimeout(400);
const max = p.getByRole("button", { name: "MAX" });
if (await max.count() !== 1) errors.push("expected exactly one MAX button, got " + await max.count());
else if (!wallet) {
  if (await max.isEnabled()) errors.push("MAX is enabled with no wallet connected");
  else console.log("MAX        present and inert (no wallet)");
} else {
  await p.getByRole("button", { name: /Select Wallet/i }).click();
  await p.waitForTimeout(400);
  await p.getByRole("button", { name: /TestWallet/i }).first().click();
  await p.waitForTimeout(2500);
  if (!await max.isEnabled()) errors.push("MAX still disabled after connecting a funded wallet");
  await max.click();
  await p.waitForTimeout(400);
  const value = await p.locator("#dep").inputValue();
  if (value !== wallet.expected) errors.push(`MAX filled ${value}, wallet holds ${wallet.expected}`);
  else console.log(`MAX        filled ${value} — matches the on-chain balance`);
}

await b.close();
if (errors.length) { console.error("\nFAIL:\n  " + errors.join("\n  ")); process.exit(1); }
console.log("\nok — all tabs rendered, MAX correct, no uncaught errors");
