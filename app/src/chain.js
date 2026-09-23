// On-chain reads and the three instructions the user signs themselves.
// buy_option is NOT built here: it needs the pool's quote_signer as a second
// signer, so the backend builds and partial-signs it (see backend/src/chain.rs).
import { useEffect, useMemo, useState } from "react";
import { AnchorProvider, BN, Program } from "@coral-xyz/anchor";
import { useConnection, useAnchorWallet } from "@solana/wallet-adapter-react";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";
import { PublicKey, Transaction } from "@solana/web3.js";
// straight from anchor's output — a copied IDL goes stale the moment an
// accounts struct changes, and the failure is a confusing account error
import idl from "../../target/idl/stocklana.json";
import { buildBuy } from "./api";
import { medianOf } from "./format";

export const PROGRAM_ID = new PublicKey(idl.address);
const key = s => (s ? new PublicKey(s) : null);

/** The pool's own address — seeds are [b"pool", collateral_mint, kind], both of
 *  which the backend already returns, so it needs no extra config to find. */
export const poolPda = (mint, kind) =>
  PublicKey.findProgramAddressSync([Buffer.from("pool"), new PublicKey(mint).toBuffer(), Uint8Array.of(kind)], PROGRAM_ID)[0];

export const lpPda = (pool, owner) =>
  PublicKey.findProgramAddressSync([Buffer.from("lp"), pool.toBuffer(), owner.toBuffer()], PROGRAM_ID)[0];
export const optPda = (pool, owner, id) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("opt"), pool.toBuffer(), owner.toBuffer(), new BN(id).toArrayLike(Buffer, "le", 8)],
    PROGRAM_ID
  )[0];

export function useProgram() {
  const { connection } = useConnection();
  const wallet = useAnchorWallet();
  return useMemo(() => {
    if (!wallet) return null;
    return new Program(idl, new AnchorProvider(connection, wallet, { commitment: "confirmed" }));
  }, [connection, wallet]);
}

/** Live oracle: spot (raw 1e6), age in seconds, sample count. */
export function useOracle(oracleKey) {
  const { connection } = useConnection();
  const [o, setO] = useState(null);
  useEffect(() => {
    if (!oracleKey) return;
    const pk = key(oracleKey);
    // read-only, so build a provider-less Program on the bare connection
    const program = new Program(idl, new AnchorProvider(connection, {
      publicKey: PublicKey.default, signTransaction: async t => t, signAllTransactions: async t => t
    }, { commitment: "confirmed" }));
    let dead = false;
    const read = async () => {
      try {
        const a = await program.account.oracle.fetch(pk);
        if (!dead) setO({
          spot: medianOf(a.samples, a.count),
          count: a.count,
          lastUpdate: Number(a.lastUpdate),
          underlying: a.underlying.toBase58()
        });
      } catch { if (!dead) setO(null); }
    };
    read();
    const t = setInterval(read, 10000);
    return () => { dead = true; clearInterval(t); };
  }, [connection, oracleKey]);
  return o;
}

/** This wallet's LpPosition.shares (raw), or 0 if they've never deposited. */
export function useLpShares(poolKey, owner, nonce) {
  const program = useProgram();
  const [shares, setShares] = useState(0);
  useEffect(() => {
    if (!program || !poolKey || !owner) { setShares(0); return; }
    let dead = false;
    program.account.lpPosition.fetch(lpPda(key(poolKey), owner))
      .then(a => !dead && setShares(Number(a.shares)))
      .catch(() => !dead && setShares(0)); // no account yet == no shares
    return () => { dead = true; };
  }, [program, poolKey, owner, nonce]);
  return shares;
}

/** Wallet's collateral (USDC) balance, raw. null while unknown. */
export function useTokenBalance(mint, owner, nonce) {
  const { connection } = useConnection();
  const [bal, setBal] = useState(null);
  useEffect(() => {
    if (!mint || !owner) { setBal(null); return; }
    let dead = false;
    connection.getTokenAccountBalance(getAssociatedTokenAddressSync(key(mint), owner))
      .then(r => !dead && setBal(Number(r.value.amount)))
      .catch(() => !dead && setBal(0)); // no ATA == no balance
    return () => { dead = true; };
  }, [connection, mint, owner, nonce]);
  return bal;
}

/** Does the backend's pool actually exist on the cluster the wallet is on? */
export function usePoolOnCluster(poolKey) {
  const { connection } = useConnection();
  const [ok, setOk] = useState(true);
  useEffect(() => {
    if (!poolKey) return;
    let dead = false;
    connection.getAccountInfo(key(poolKey))
      .then(i => !dead && setOk(!!i))
      .catch(() => !dead && setOk(false));
    return () => { dead = true; };
  }, [connection, poolKey]);
  return ok;
}

// ── writes
export const deposit = (program, pool, amountRaw) =>
  program.methods.deposit(new BN(amountRaw)).accounts({
    user: program.provider.publicKey,
    pool: key(pool.pubkey),
    userToken: getAssociatedTokenAddressSync(key(pool.collateral_mint), program.provider.publicKey)
  }).rpc();

export const withdraw = (program, pool, sharesRaw) =>
  program.methods.withdraw(new BN(sharesRaw)).accounts({
    user: program.provider.publicKey,
    pool: key(pool.pubkey),
    userToken: getAssociatedTokenAddressSync(key(pool.collateral_mint), program.provider.publicKey)
  }).rpc();

export const claim = (program, pool, position) =>
  program.methods.claim().accounts({
    owner: program.provider.publicKey,
    pool: key(pool.pubkey),
    ownerToken: getAssociatedTokenAddressSync(key(pool.collateral_mint), program.provider.publicKey),
    position: optPda(key(pool.pubkey), program.provider.publicKey, position.id)
  }).rpc();

/**
 * Ask the backend to re-price and build the tx, then co-sign and submit it.
 * The premium the user signs is the backend's, not the displayed quote — the
 * caller must show it before this runs.
 */
export async function buyOption(connection, signTransaction, buyer, id, strikeRaw, sizeRaw) {
  const built = await buildBuy(buyer.toBase58(), id, strikeRaw, sizeRaw);
  const tx = Transaction.from(Uint8Array.from(atob(built.transaction), c => c.charCodeAt(0)));
  const signed = await signTransaction(tx); // quote_signer already signed
  const sig = await connection.sendRawTransaction(signed.serialize());
  await connection.confirmTransaction(sig, "confirmed");
  return { sig, ...built };
}

/** A u64 position id, unique per buyer: the opt PDA seeds it, so two buys in
 *  the same millisecond would collide on the same account. Millisecond clock. */
export const newPositionId = () => Date.now();
