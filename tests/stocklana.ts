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

  const pushPrices = async (prices: number[]) => {
    for (const price of prices) {
      await program.methods
        .pushPrice(usd(price))
        .accountsPartial({ oracle, keeper: admin.publicKey })
        .rpc();
    }
  };

  // nine of sixteen samples is a majority of the ring, so this moves the median
  const setSpot = (price: number) => pushPrices(Array(9).fill(price));

  const waitFor = async (t: number) => {
    while ((await chainTime()) < t) {
      await new Promise((r) => setTimeout(r, 500));
    }
  };

  // never swallows the assertion: the rejection reason is captured, not caught
  const expectRevert = async (send: () => Promise<unknown>, code: string) => {
    const err = await send().then(
      () => null,
      (e: unknown) => String(e)
    );
    assert.isNotNull(err, `expected ${code}, transaction succeeded`);
    assert.include(err as string, code);
  };

  const closeEpoch = () => program.methods.closeEpoch().accountsPartial({ pool, oracle }).rpc();

  const settlePosition = (id: number) =>
    program.methods.settle().accountsPartial({ pool, position: positionPda(id) }).rpc();

  const roll = (epochEnd: number) =>
    program.methods
      .rollEpoch(new BN(epochEnd))
      .accountsPartial({ pool, authority: admin.publicKey })
      .rpc();

  const depositUsd = (n: number) =>
    program.methods
      .deposit(usd(n))
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

  const withdrawShares = (shares: BN) =>
    program.methods
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

  // the price an LP mints or redeems at: NAV over shares
  const sharePrice = async () => {
    const p = await program.account.pool.fetch(pool);
    return (p.available.toNumber() + p.locked.toNumber()) / p.totalShares.toNumber();
  };

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

    // one epoch, one expiry; a keeper would pass now + 86400 for daily
    expiry = (await chainTime()) + 15;
    await program.methods
      .rollEpoch(new BN(expiry))
      .accountsPartial({ pool, authority: admin.publicKey })
      .rpc();

    // spot is $220 when the puts are written, so all three start out of the money
    await pushPrices([218, 220, 222, 220, 221]);
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
    for (const c of cases) {
      await program.methods
        .buyOption(new BN(c.id), usd(c.strike), new BN(S), usd(c.premium), new BN(expiry + 60))
        .accountsPartial({
          buyer: buyer.publicKey,
          quoteSigner: admin.publicKey,
          pool,
          oracle,
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
        .buyOption(new BN(9), usd(200), new BN(S), usd(1), new BN(expiry + 60))
        .accountsPartial({
          buyer: buyer.publicKey,
          quoteSigner: rogue.publicKey,
          pool,
          oracle,
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

  it("rejects a premium below intrinsic value", async () => {
    // $260 strike put at a $220 spot is $40 in the money; nobody sells that for $1
    try {
      await program.methods
        .buyOption(new BN(8), usd(260), new BN(S), usd(1), new BN(expiry + 60))
        .accountsPartial({
          buyer: buyer.publicKey,
          quoteSigner: admin.publicKey,
          pool,
          oracle,
          vault,
          buyerToken,
          position: positionPda(8),
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([buyer])
        .rpc();
      assert.fail("expected PremiumBelowIntrinsic");
    } catch (e) {
      assert.include(e.toString(), "PremiumBelowIntrinsic");
    }
  });

  it("rejects a quote that expires too far in the future", async () => {
    try {
      await program.methods
        .buyOption(new BN(10), usd(200), new BN(S), usd(1), new BN((await chainTime()) + 86400))
        .accountsPartial({
          buyer: buyer.publicKey,
          quoteSigner: admin.publicKey,
          pool,
          oracle,
          vault,
          buyerToken,
          position: positionPda(10),
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([buyer])
        .rpc();
      assert.fail("expected QuoteTtlTooLong");
    } catch (e) {
      assert.include(e.toString(), "QuoteTtlTooLong");
    }
  });

  it("rejects an oracle that does not match the collateral mint", async () => {
    const [callPool] = PublicKey.findProgramAddressSync(
      [Buffer.from("pool"), usdc.toBuffer(), Buffer.from([1])],
      program.programId
    );
    const [callVault] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), callPool.toBuffer()],
      program.programId
    );
    try {
      await program.methods
        .initPool(1, admin.publicKey)
        .accountsPartial({
          admin: admin.publicKey,
          collateralMint: usdc,
          oracle,
          pool: callPool,
          vault: callVault,
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .rpc();
      assert.fail("expected OracleMintMismatch");
    } catch (e) {
      assert.include(e.toString(), "OracleMintMismatch");
    }
  });

  it("locks writers in while the epoch has open positions", async () => {
    try {
      await program.methods
        .withdraw(new BN(1 * S))
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
      assert.fail("expected EpochActive");
    } catch (e) {
      assert.include(e.toString(), "EpochActive");
    }
  });

  it("refuses to settle before the epoch price is latched", async () => {
    // spot drops to $200 and fills the ring buffer; the $500 print is an outlier the median ignores
    await pushPrices([...Array(13).fill(200), 198, 202, 500]);
    const o = await program.account.oracle.fetch(oracle);
    assert.equal(o.count, 16);

    await waitFor(expiry);
    await expectRevert(() => settlePosition(cases[0].id), "EpochNotClosed");
  });

  it("latches one settlement price for the whole epoch", async () => {
    await closeEpoch();
    const p = await program.account.pool.fetch(pool);
    assert.equal(p.settlePrice.toNumber(), 200 * S);
    assert.equal(p.settledEpoch.toNumber(), 1);

    // spot collapses to $150 after the latch: a second close must not re-pick the
    // price off the newer median, and settlement below still pays at $200
    await setSpot(150);
    await expectRevert(closeEpoch, "EpochAlreadyClosed");
    assert.equal(
      (await program.account.pool.fetch(pool)).settlePrice.toNumber(),
      200 * S,
      "latched price must not move"
    );
  });

  it("settles every position at the price the epoch latched", async () => {
    for (const c of cases) {
      const position = positionPda(c.id);
      await settlePosition(c.id);
      const pos = await program.account.optionPosition.fetch(position);
      assert.equal(pos.payout.toNumber(), c.payout * S, `${c.name} payout`);
      assert.isTrue(pos.settled);
      assert.equal(pos.epoch.toNumber(), 1, `${c.name} epoch`);
    }

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.locked.toNumber(), 0);
    assert.equal(p.openPositions, 0);
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

  // ---- second epoch, back to back with the first ----

  // strike / premium / expected payout at the $190 epoch-2 settlement price
  const epoch2 = [
    { name: "in the money", id: 4, strike: 200, premium: 4, payout: 10 },
    { name: "out of the money", id: 5, strike: 180, premium: 1, payout: 0 },
  ];

  let expiry2: number;
  let expiry3: number;

  it("rolls into a second epoch once the first has drained", async () => {
    await setSpot(220); // spot back to $220 so both new puts start out of the money
    expiry2 = (await chainTime()) + 20;
    await roll(expiry2);

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.epoch.toNumber(), 2);
    assert.equal(p.settledEpoch.toNumber(), 1, "epoch 2 is not closed yet");
    assert.equal(p.epochEnd.toNumber(), expiry2);
  });

  it("mints every deposit of the epoch at the same share price", async () => {
    await depositUsd(500);
    const first = await sharePrice();
    await depositUsd(100);
    const second = await sharePrice();

    assert.equal(first, 1);
    assert.equal(second, first, "share price must not move between deposits");

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.totalShares.toNumber(), 600 * S);
    assert.equal(p.available.toNumber(), 600 * S);
  });

  it("stamps the current epoch on every position it writes", async () => {
    for (const c of epoch2) {
      await program.methods
        .buyOption(new BN(c.id), usd(c.strike), new BN(S), usd(c.premium), new BN(expiry2 + 60))
        .accountsPartial({
          buyer: buyer.publicKey,
          quoteSigner: admin.publicKey,
          pool,
          oracle,
          vault,
          buyerToken,
          position: positionPda(c.id),
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([buyer])
        .rpc();

      const pos = await program.account.optionPosition.fetch(positionPda(c.id));
      assert.equal(pos.epoch.toNumber(), 2, `${c.name} epoch`);
      assert.equal(pos.expiry.toNumber(), expiry2, `${c.name} expiry`);
    }

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.openPositions, 2);
    assert.equal(p.locked.toNumber(), 380 * S);
    assert.equal(p.available.toNumber(), (600 - 380 + 5) * S);
  });

  it("holds the epoch shut while positions are open", async () => {
    await expectRevert(() => depositUsd(1), "EpochActive");
    await expectRevert(() => withdrawShares(new BN(S)), "EpochActive");
    await expectRevert(() => roll(expiry2 + 3600), "EpochActive");
  });

  it("settles the second epoch at its own latched price", async () => {
    await waitFor(expiry2);
    await setSpot(190);
    await closeEpoch();

    const latched = await program.account.pool.fetch(pool);
    assert.equal(latched.settlePrice.toNumber(), 190 * S);
    assert.equal(latched.settledEpoch.toNumber(), 2);

    for (const c of epoch2) {
      await settlePosition(c.id);
      const pos = await program.account.optionPosition.fetch(positionPda(c.id));
      assert.equal(pos.payout.toNumber(), c.payout * S, `${c.name} payout`);
    }

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.locked.toNumber(), 0);
    assert.equal(p.openPositions, 0);
    assert.equal(p.available.toNumber(), (600 + 5 - 10) * S);
  });

  it("pays the buyer and lets the writer redeem at the new price", async () => {
    const before = await balance(buyerToken);
    await program.methods
      .claim()
      .accountsPartial({
        owner: buyer.publicKey,
        pool,
        vault,
        ownerToken: buyerToken,
        position: positionPda(epoch2[0].id),
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([buyer])
      .rpc();
    assert.equal((await balance(buyerToken)) - before, 10 * S);
    // position 5 is deliberately left unclaimed: it is the straggler epoch 3 must refuse

    const priceBefore = await sharePrice();
    assert.closeTo(priceBefore, 595 / 600, 1e-9);

    const lpBefore = await balance(lpToken);
    await withdrawShares(new BN(300 * S));
    assert.equal((await balance(lpToken)) - lpBefore, 297.5 * S);
    assert.equal(await sharePrice(), priceBefore, "redeeming must not move the price");
  });

  it("rolls again only after the epoch it is leaving is closed", async () => {
    const priceBefore = await sharePrice();
    expiry3 = (await chainTime()) + 10;
    await roll(expiry3);

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.epoch.toNumber(), 3);
    assert.equal(p.settledEpoch.toNumber(), 2);
    assert.equal(await sharePrice(), priceBefore, "the roll itself must not move the price");

    await expectRevert(closeEpoch, "EpochNotEnded");
    await expectRevert(async () => roll((await chainTime()) + 3600), "EpochNotClosed");
  });

  it("refuses a straggler from an earlier epoch", async () => {
    await waitFor(expiry3);
    await setSpot(230);
    await closeEpoch();

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.settlePrice.toNumber(), 230 * S);
    assert.equal(p.settledEpoch.toNumber(), 3);

    // position 5 belongs to epoch 2; at $230 a $180 put is still worthless, but a
    // $230 latch must never be applied to it at all. The epoch check runs ahead of
    // the settled flag, so this is EpochMismatch and not AlreadySettled.
    await expectRevert(() => settlePosition(epoch2[1].id), "EpochMismatch");
    assert.equal((await program.account.optionPosition.fetch(positionPda(5))).payout.toNumber(), 0);
  });

  it("rolls a fourth time and drains the vault", async () => {
    await roll((await chainTime()) + 3600);
    assert.equal((await program.account.pool.fetch(pool)).epoch.toNumber(), 4);

    const shares = (await program.account.lpPosition.fetch(lpPos)).shares;
    await withdrawShares(shares);

    assert.equal(await balance(lpToken), 984 * S);
    assert.equal(await balance(buyerToken), 116 * S);
    assert.equal(await balance(vault), 0);
  });
});
