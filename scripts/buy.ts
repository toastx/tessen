// Ask the backend to build a buy_option tx (quote_signer-cosigned), add the
// buyer signature, and submit. Run: npx ts-node scripts/buy.ts
import { Connection, Keypair, Transaction } from "@solana/web3.js";
import * as fs from "fs";

const load = (p: string) =>
  Keypair.fromSecretKey(new Uint8Array(JSON.parse(fs.readFileSync(p, "utf8"))));

(async () => {
  const bs = JSON.parse(fs.readFileSync("keys/bootstrap.json", "utf8"));
  const conn = new Connection(bs.url, "confirmed");
  const buyer = load("keys/buyer.json");

  const req = { buyer: buyer.publicKey.toBase58(), id: 1, strike: 210_000_000, size: 1_000_000 };
  const res = await fetch("http://127.0.0.1:8080/buy", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(req),
  });
  const body: any = await res.json();
  console.log("backend /buy ->", body);
  if (!body.transaction) throw new Error("no tx from backend");

  const tx = Transaction.from(Buffer.from(body.transaction, "base64"));
  tx.partialSign(buyer); // quote_signer already signed server-side
  const sig = await conn.sendRawTransaction(tx.serialize());
  await conn.confirmTransaction(sig, "confirmed");
  console.log("buy submitted:", sig);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
