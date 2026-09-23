// Live-chain check for the things selfcheck.js can't cover: that the IDL still
// matches the deployed program, and that Anchor resolves the accounts chain.js
// leaves out (lp pda, vault relation, token/system programs). Builds
// instructions only — nothing is signed or sent.
//
// Needs a validator and the backend up. Run: node ixcheck.mjs
import anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";
import fs from "fs";
const { AnchorProvider, BN, Program } = anchor;

const idl = JSON.parse(fs.readFileSync("../target/idl/stocklana.json", "utf8"));
const PROGRAM_ID = new PublicKey(idl.address);
const conn = new Connection("http://127.0.0.1:8899", "confirmed");
const kp = Keypair.fromSecretKey(new Uint8Array(JSON.parse(fs.readFileSync("../keys/buyer.json", "utf8"))));
const wallet = { publicKey: kp.publicKey, signTransaction: async t => t, signAllTransactions: async t => t };
const program = new Program(idl, new AnchorProvider(conn, wallet, { commitment: "confirmed" }));

const pool = await (await fetch("http://127.0.0.1:8080/pool")).json();
const poolPda = PublicKey.findProgramAddressSync(
  [Buffer.from("pool"), new PublicKey(pool.collateral_mint).toBuffer(), Uint8Array.of(pool.kind)], PROGRAM_ID)[0];
console.log("pool pda      :", poolPda.toBase58());

const o = await program.account.oracle.fetch(new PublicKey(pool.oracle));
const med = s => { const b = s.samples.slice(0, s.count).map(Number).sort((a, b) => a - b); return b[Math.floor(s.count / 2)]; };
console.log("oracle        : count=%d spot=%d lastUpdate=%d", o.count, med(o), Number(o.lastUpdate));

const userToken = getAssociatedTokenAddressSync(new PublicKey(pool.collateral_mint), kp.publicKey);
const show = async (name, b) => {
  const ix = await b.instruction();
  console.log("%s -> %d accounts, all resolved", name.padEnd(8), ix.keys.length);
};
await show("deposit", program.methods.deposit(new BN(1_000_000)).accounts({ user: kp.publicKey, pool: poolPda, userToken }));
await show("withdraw", program.methods.withdraw(new BN(1_000_000)).accounts({ user: kp.publicKey, pool: poolPda, userToken }));

const optPda = (p, owner, id) => PublicKey.findProgramAddressSync(
  [Buffer.from("opt"), p.toBuffer(), owner.toBuffer(), new BN(id).toArrayLike(Buffer, "le", 8)], PROGRAM_ID)[0];
await show("claim", program.methods.claim().accounts({
  owner: kp.publicKey, pool: poolPda, ownerToken: userToken, position: optPda(poolPda, kp.publicKey, 1)
}));

const positions = await (await fetch("http://127.0.0.1:8080/positions")).json();
console.log("positions     :", positions.length, positions[0] ? "first id=" + positions[0].id : "");
if (positions[0]) {
  const derived = optPda(poolPda, new PublicKey(positions[0].owner), positions[0].id);
  const info = await conn.getAccountInfo(derived);
  console.log("opt pda derive:", derived.toBase58(), info ? "EXISTS on chain" : "MISSING");
}
