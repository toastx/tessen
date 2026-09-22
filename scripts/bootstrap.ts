// One-shot: init an oracle + PUT pool on a running localnet, fund it, open an
// epoch, seed one price. Writes keys/bootstrap.json for the keeper/backend runs.
// Run: ANCHOR_PROVIDER_URL=http://127.0.0.1:8899 npx ts-node scripts/bootstrap.ts
import * as anchor from "@coral-xyz/anchor";
import { BN } from "@coral-xyz/anchor";
import { Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAssociatedTokenAccount,
  mintTo,
} from "@solana/spl-token";
import * as fs from "fs";

const S = 1_000_000;
const PUT = 0;
const load = (p: string) =>
  Keypair.fromSecretKey(new Uint8Array(JSON.parse(fs.readFileSync(p, "utf8"))));

(async () => {
  const url = process.env.ANCHOR_PROVIDER_URL || "http://127.0.0.1:8899";
  const admin = load("keys/admin.json");
  const conn = new anchor.web3.Connection(url, "confirmed");
  const provider = new anchor.AnchorProvider(conn, new anchor.Wallet(admin), {
    commitment: "confirmed",
  });
  anchor.setProvider(provider);
  const idl = JSON.parse(fs.readFileSync("target/idl/stocklana.json", "utf8"));
  const program = new anchor.Program(idl, provider);
  const pid = program.programId;

  const keeper = load("keys/keeper.json");
  const quote = load("keys/quotesigner.json");
  const buyer = load("keys/buyer.json");

  const usdc = await createMint(conn, admin, admin.publicKey, null, 6);
  const underlying = await createMint(conn, admin, admin.publicKey, null, 6);
  // ATAs so the buyer account matches what the backend derives in /buy
  const adminUsdc = await createAssociatedTokenAccount(conn, admin, usdc, admin.publicKey);
  const buyerUsdc = await createAssociatedTokenAccount(conn, admin, usdc, buyer.publicKey);
  await mintTo(conn, admin, usdc, adminUsdc, admin, 1_000_000 * S);
  await mintTo(conn, admin, usdc, buyerUsdc, admin, 100_000 * S);

  const [oracle] = PublicKey.findProgramAddressSync(
    [Buffer.from("oracle"), underlying.toBuffer()],
    pid
  );
  await program.methods
    .initOracle(keeper.publicKey)
    .accountsPartial({ admin: admin.publicKey, underlying, oracle, systemProgram: SystemProgram.programId })
    .rpc();

  const [pool] = PublicKey.findProgramAddressSync(
    [Buffer.from("pool"), usdc.toBuffer(), Buffer.from([PUT])],
    pid
  );
  const [vault] = PublicKey.findProgramAddressSync(
    [Buffer.from("vault"), pool.toBuffer()],
    pid
  );
  await program.methods
    .initPool(PUT, quote.publicKey, keeper.publicKey)
    .accountsPartial({
      admin: admin.publicKey,
      collateralMint: usdc,
      underlying,
      oracle,
      pool,
      vault,
      tokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  const [lp] = PublicKey.findProgramAddressSync(
    [Buffer.from("lp"), pool.toBuffer(), admin.publicKey.toBuffer()],
    pid
  );
  await program.methods
    .deposit(new BN(500_000 * S))
    .accountsPartial({
      user: admin.publicKey,
      pool,
      vault,
      userToken: adminUsdc,
      lp,
      tokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  // seed one price so /quote works before the keeper's first push
  await program.methods
    .pushPrice(new BN(200 * S))
    .accountsPartial({ oracle, keeper: keeper.publicKey })
    .signers([keeper])
    .rpc();

  const now = (await conn.getBlockTime(await conn.getSlot()))!;
  await program.methods
    .rollEpoch(new BN(now + 3600))
    .accountsPartial({ pool, authority: admin.publicKey })
    .rpc();

  const out = {
    url,
    programId: pid.toBase58(),
    pool: pool.toBase58(),
    oracle: oracle.toBase58(),
    vault: vault.toBase58(),
    usdc: usdc.toBase58(),
    underlying: underlying.toBase58(),
    admin: admin.publicKey.toBase58(),
    keeper: keeper.publicKey.toBase58(),
    quoteSigner: quote.publicKey.toBase58(),
    buyer: buyer.publicKey.toBase58(),
    buyerUsdc: buyerUsdc.toBase58(),
  };
  fs.writeFileSync("keys/bootstrap.json", JSON.stringify(out, null, 2));
  console.log("bootstrap ok:\n" + JSON.stringify(out, null, 2));
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
