import * as anchor from "@coral-xyz/anchor";
import { BN, Program } from "@coral-xyz/anchor";
import { Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram } from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAccount,
  getAccount,
  mintTo,
} from "@solana/spl-token";
import { assert } from "chai";
import { Stocklana } from "../target/types/stocklana";

const S = 1_000_000;
const PUT = 0;
const usd = (n: number) => new BN(n * S);

describe("stocklana cash-secured put vault", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.stocklana as Program<Stocklana>;
  const admin = (provider.wallet as anchor.Wallet).payer;

  const lp = Keypair.generate();
  const buyer = Keypair.generate();

  let usdc: PublicKey, underlying: PublicKey;
  let oracle: PublicKey, pool: PublicKey, vault: PublicKey, lpPos: PublicKey;
  let lpToken: PublicKey, buyerToken: PublicKey;

  const positionPda = (id: number) =>
    PublicKey.findProgramAddressSync(
      [
        Buffer.from("opt"),
        pool.toBuffer(),
        buyer.publicKey.toBuffer(),
        new BN(id).toArrayLike(Buffer, "le", 8),
      ],
      program.programId
    )[0];

  const balance = async (a: PublicKey) => Number((await getAccount(provider.connection, a)).amount);

  // the test validator clock lags wall clock, so expiries follow the chain
  const chainTime = async () =>
    (await provider.connection.getBlockTime(await provider.connection.getSlot())) as number;

  before(async () => {
    for (const kp of [lp, buyer]) {
      const sig = await provider.connection.requestAirdrop(kp.publicKey, 2 * LAMPORTS_PER_SOL);
      await provider.connection.confirmTransaction(sig);
    }

    usdc = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    underlying = await createMint(provider.connection, admin, admin.publicKey, null, 6);

    lpToken = await createAccount(provider.connection, admin, usdc, lp.publicKey);
    buyerToken = await createAccount(provider.connection, admin, usdc, buyer.publicKey);
    await mintTo(provider.connection, admin, usdc, lpToken, admin, 1000 * S);
    await mintTo(provider.connection, admin, usdc, buyerToken, admin, 100 * S);

    [oracle] = PublicKey.findProgramAddressSync(
      [Buffer.from("oracle"), underlying.toBuffer()],
      program.programId
    );
    [pool] = PublicKey.findProgramAddressSync(
      [Buffer.from("pool"), usdc.toBuffer(), Buffer.from([PUT])],
      program.programId
    );
    [vault] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), pool.toBuffer()],
      program.programId
    );
    [lpPos] = PublicKey.findProgramAddressSync(
      [Buffer.from("lp"), pool.toBuffer(), lp.publicKey.toBuffer()],
      program.programId
    );

    await program.methods
      .initOracle(admin.publicKey)
      .accountsPartial({ admin: admin.publicKey, underlying, oracle, systemProgram: SystemProgram.programId })
      .rpc();

    await program.methods
      .initPool(PUT, admin.publicKey)
      .accountsPartial({
        admin: admin.publicKey,
        collateralMint: usdc,
        oracle,
        pool,
        vault,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();
  });

  it("deposits collateral and mints shares", async () => {
    await program.methods
      .deposit(usd(1000))
      .accountsPartial({
        user: lp.publicKey,
        pool,
        vault,
        userToken: lpToken,
        lp: lpPos,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([lp])
      .rpc();

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.available.toNumber(), 1000 * S);
    assert.equal(p.totalShares.toNumber(), 1000 * S);
    assert.equal(await balance(vault), 1000 * S);
  });

  // strike / premium / expected payout at a $200 settlement price
  const cases = [
    { name: "in the money", id: 1, strike: 220, premium: 5, payout: 20 },
    { name: "at the money", id: 2, strike: 200, premium: 3, payout: 0 },
    { name: "out of the money", id: 3, strike: 180, premium: 1, payout: 0 },
  ];

  let expiry: number;

  it("sells puts and locks full collateral", async () => {
    expiry = (await chainTime()) + 5;
    for (const c of cases) {
      await program.methods
        .buyOption(
          new BN(c.id),
          usd(c.strike),
          new BN(S),
          new BN(expiry),
          usd(c.premium),
          new BN(expiry + 60)
        )
        .accountsPartial({
          buyer: buyer.publicKey,
          quoteSigner: admin.publicKey,
          pool,
          vault,
          buyerToken,
          position: positionPda(c.id),
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([buyer])
        .rpc();
    }

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.locked.toNumber(), 600 * S);
    assert.equal(p.available.toNumber(), (1000 - 600 + 9) * S);
    assert.equal(await balance(vault), 1009 * S);
  });

  it("rejects a quote without the backend signer", async () => {
    const rogue = Keypair.generate();
    try {
      await program.methods
        .buyOption(new BN(9), usd(200), new BN(S), new BN(expiry), usd(1), new BN(expiry + 60))
        .accountsPartial({
          buyer: buyer.publicKey,
          quoteSigner: rogue.publicKey,
          pool,
          vault,
          buyerToken,
          position: positionPda(9),
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([buyer, rogue])
        .rpc();
      assert.fail("expected BadQuoteSigner");
    } catch (e) {
      assert.include(e.toString(), "BadQuoteSigner");
    }
  });

  it("settles on the median of the oracle samples", async () => {
    for (const price of [198, 200, 202, 200, 205]) {
      await program.methods
        .pushPrice(usd(price))
        .accountsPartial({ oracle, keeper: admin.publicKey })
        .rpc();
    }
    const o = await program.account.oracle.fetch(oracle);
    assert.equal(o.count, 5);

    while ((await chainTime()) < expiry) {
      await new Promise((r) => setTimeout(r, 500));
    }

    for (const c of cases) {
      const position = positionPda(c.id);
      await program.methods
        .settle()
        .accountsPartial({ pool, oracle, position })
        .rpc();
      const pos = await program.account.optionPosition.fetch(position);
      assert.equal(pos.payout.toNumber(), c.payout * S, `${c.name} payout`);
      assert.isTrue(pos.settled);
    }

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.locked.toNumber(), 0);
    assert.equal(p.available.toNumber(), 989 * S);
  });

  it("pays the buyer on claim and closes the position", async () => {
    for (const c of cases) {
      const position = positionPda(c.id);
      const before = await balance(buyerToken);
      await program.methods
        .claim()
        .accountsPartial({
          owner: buyer.publicKey,
          pool,
          vault,
          ownerToken: buyerToken,
          position,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer])
        .rpc();
      assert.equal((await balance(buyerToken)) - before, c.payout * S, `${c.name} claim`);
      assert.isNull(await provider.connection.getAccountInfo(position));
    }
    assert.equal(await balance(vault), 989 * S);
  });

  it("lets the writer withdraw premiums minus losses", async () => {
    const shares = (await program.account.lpPosition.fetch(lpPos)).shares;
    await program.methods
      .withdraw(shares)
      .accountsPartial({
        user: lp.publicKey,
        pool,
        vault,
        userToken: lpToken,
        lp: lpPos,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([lp])
      .rpc();

    assert.equal(await balance(lpToken), 989 * S);
    assert.equal(await balance(vault), 0);
  });
});
