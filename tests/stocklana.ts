import * as anchor from "@coral-xyz/anchor";
import { BN, Program } from "@coral-xyz/anchor";
import {
  ComputeBudgetProgram,
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
  Transaction,
} from "@solana/web3.js";
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
const RING = 32;
// the program's own floor: fewer in-window samples than this and the epoch refuses to latch
const MIN_SETTLEMENT_SAMPLES = 8;
// the program's floor on the gap between two accepted pushes. Every push in this
// suite now has to be spaced by at least this much chain time — the ring can no
// longer be stuffed, and the suite pays for that in wall clock seconds.
const MIN_PUSH_INTERVAL = 30;
const usd = (n: number) => new BN(n * S);

describe("stocklana cash-secured put vault", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.stocklana as Program<Stocklana>;
  const admin = (provider.wallet as anchor.Wallet).payer;

  const lp = Keypair.generate();
  const buyer = Keypair.generate();
  // deliberately not the authority: it is what tells `settle`'s gate apart from
  // `force_settle`'s
  const poolKeeper = Keypair.generate();

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

  const epochRecordPda = (poolKey: PublicKey, epoch: number) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("epoch"), poolKey.toBuffer(), new BN(epoch).toArrayLike(Buffer, "le", 8)],
      program.programId
    )[0];

  const balance = async (a: PublicKey) => Number((await getAccount(provider.connection, a)).amount);

  // the test validator clock lags wall clock, so expiries follow the chain
  const chainTime = async () =>
    (await provider.connection.getBlockTime(await provider.connection.getSlot())) as number;

  // Two byte-identical transactions have the same signature, and the second one is
  // silently dropped as already-processed rather than rejected. Batches of the same
  // flat price are exactly that shape, so every batch carries a nonce that changes
  // the bytes without touching what the batch does.
  let batchNonce = 0;

  const medianOf = (xs: number[]) => {
    const sorted = [...xs].sort((a, b) => a - b);
    return sorted[Math.floor(sorted.length / 2)];
  };

  // A client-side mirror of one oracle's 32-slot ring. The 10% deviation guard
  // means a price can no longer simply be asserted — it has to be walked to — and
  // the walk has to know what the on-chain median currently is. Every push in the
  // suite goes through a feed so the mirror never drifts from the account.
  const makeFeed = (oracleKey: PublicKey) => {
    const ring: number[] = [];
    let idx = 0;

    const median = () => medianOf(ring);

    // The push floor means samples can no longer share a transaction. Each push
    // is its own transaction preceded by a wait that guarantees at least
    // MIN_PUSH_INTERVAL of chain time has passed since the previous one.
    const send = async (micros: number[]) => {
      let last = 0;
      for (const price of micros) {
        const now = await chainTime();
        const target = last === 0 ? now : last + MIN_PUSH_INTERVAL;
        while ((await chainTime()) < target) {
          await new Promise((r) => setTimeout(r, 250));
        }
        const tx = new Transaction().add(
          ComputeBudgetProgram.setComputeUnitLimit({ units: 900_000 + batchNonce++ })
        );
        tx.add(
          await program.methods
            .pushPrice(new BN(price))
            .accountsPartial({ oracle: oracleKey, keeper: admin.publicKey })
            .instruction()
        );
        await provider.sendAndConfirm(tx, []);
        last = await chainTime();
        ring[idx] = price;
        idx = (idx + 1) % RING;
      }
    };

    // the largest move the program accepts, in its own integer arithmetic:
    // diff * 10_000 <= med * 1_000
    const band = (med: number) => Math.floor(med / 10);

    // walks to `target` in steps the guard accepts, then keeps pushing it until a
    // majority of the ring carries it and the median lands exactly there
    const plan = (target: number) => {
      const sim = [...ring];
      let cursor = idx;
      const out: number[] = [];
      while (sim.length === 0 || medianOf(sim) !== target) {
        const med = sim.length === 0 ? target : medianOf(sim);
        const next = Math.min(med + band(med), Math.max(med - band(med), target));
        sim[cursor] = next;
        cursor = (cursor + 1) % RING;
        out.push(next);
        assert.isBelow(out.length, 400, `setSpot(${target}) did not converge`);
      }
      return out;
    };

    return {
      push: (prices: number[]) => send(prices.map((p) => Math.round(p * S))),
      setSpot: (price: number) => send(plan(Math.round(price * S))),
      median: () => median() / S,
      size: () => ring.length,
    };
  };

  let feed: ReturnType<typeof makeFeed>;
  const pushPrices = (prices: number[]) => feed.push(prices);
  const setSpot = (price: number) => feed.setSpot(price);

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
    assert.include(err as string, code, err as string);
  };

  const signersFor = (kp: Keypair) => (kp.publicKey.equals(admin.publicKey) ? [] : [kp]);

  const closeEpochFor = async (poolKey: PublicKey, oracleKey: PublicKey, operator = admin) => {
    const epoch = (await program.account.pool.fetch(poolKey)).epoch.toNumber();
    return program.methods
      .closeEpoch()
      .accountsPartial({
        operator: operator.publicKey,
        pool: poolKey,
        oracle: oracleKey,
        epochRecord: epochRecordPda(poolKey, epoch),
        systemProgram: SystemProgram.programId,
      })
      .signers(signersFor(operator))
      .rpc();
  };

  const closeEpoch = (operator = admin) => closeEpochFor(pool, oracle, operator);

  const settlePosition = async (id: number, operator = admin) => {
    const epoch = (await program.account.optionPosition.fetch(positionPda(id))).epoch.toNumber();
    return program.methods
      .settle()
      .accountsPartial({
        operator: operator.publicKey,
        pool,
        position: positionPda(id),
        epochRecord: epochRecordPda(pool, epoch),
      })
      .signers(signersFor(operator))
      .rpc();
  };

  const forceSettlePosition = async (id: number, authority = admin) => {
    const epoch = (await program.account.optionPosition.fetch(positionPda(id))).epoch.toNumber();
    return program.methods
      .forceSettle()
      .accountsPartial({
        authority: authority.publicKey,
        pool,
        position: positionPda(id),
        epochRecord: epochRecordPda(pool, epoch),
      })
      .signers(signersFor(authority))
      .rpc();
  };

  const rollFor = (poolKey: PublicKey, epochEnd: number) =>
    program.methods
      .rollEpoch(new BN(epochEnd))
      .accountsPartial({ pool: poolKey, authority: admin.publicKey })
      .rpc();

  const roll = (epochEnd: number) => rollFor(pool, epochEnd);

  // anchor decodes a fieldless enum as a single-key object: { open: {} }
  const stateOf = (p: { state: object }) => Object.keys(p.state)[0];

  const buyPut = (id: number, strike: number, premium: number, quoteExpiry: number) =>
    program.methods
      .buyOption(new BN(id), usd(strike), new BN(S), usd(premium), new BN(quoteExpiry))
      .accountsPartial({
        buyer: buyer.publicKey,
        quoteSigner: admin.publicKey,
        pool,
        oracle,
        vault,
        buyerToken,
        position: positionPda(id),
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([buyer])
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

  const newFeed = async () => {
    const u = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    const [o] = PublicKey.findProgramAddressSync(
      [Buffer.from("oracle"), u.toBuffer()],
      program.programId
    );
    await program.methods
      .initOracle(admin.publicKey)
      .accountsPartial({
        admin: admin.publicKey,
        underlying: u,
        oracle: o,
        systemProgram: SystemProgram.programId,
      })
      .rpc();
    return { underlying: u, oracle: o, feed: makeFeed(o) };
  };

  const newPool = async (oracleKey: PublicKey, underlyingMint: PublicKey) => {
    const collateral = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    const [p] = PublicKey.findProgramAddressSync(
      [Buffer.from("pool"), collateral.toBuffer(), Buffer.from([PUT])],
      program.programId
    );
    const [v] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), p.toBuffer()],
      program.programId
    );
    await program.methods
      .initPool(PUT, admin.publicKey, poolKeeper.publicKey)
      .accountsPartial({
        admin: admin.publicKey,
        collateralMint: collateral,
        underlying: underlyingMint,
        oracle: oracleKey,
        pool: p,
        vault: v,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();
    return p;
  };

  // the price an LP mints or redeems at: NAV over shares
  const sharePrice = async () => {
    const p = await program.account.pool.fetch(pool);
    return (p.available.toNumber() + p.locked.toNumber()) / p.totalShares.toNumber();
  };

  before(async () => {
    for (const kp of [lp, buyer, poolKeeper]) {
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
    feed = makeFeed(oracle);

    await program.methods
      .initOracle(admin.publicKey)
      .accountsPartial({ admin: admin.publicKey, underlying, oracle, systemProgram: SystemProgram.programId })
      .rpc();

    await program.methods
      .initPool(PUT, admin.publicKey, poolKeeper.publicKey)
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

    // one epoch, one expiry; a keeper would pass now + 86400 for daily
    expiry = (await chainTime()) + 15;
    await program.methods
      .rollEpoch(new BN(expiry))
      .accountsPartial({ pool, authority: admin.publicKey })
      .rpc();

    // spot is $220 when the puts are written, so all three start out of the money
    await pushPrices([218, 220, 222, 220, 221]);
  });

  it("refuses a dust deposit that would set the share scale", async () => {
    // total_shares == 0: this deposit would otherwise mint 1:1 and fix a coarse scale
    await expectRevert(() => depositUsd(0.5), "FirstDepositTooSmall");
  });

  it("deposits collateral and mints shares", async () => {
    await depositUsd(1000);

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
      await buyPut(c.id, c.strike, c.premium, expiry + 60);
    }

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.locked.toNumber(), 600 * S);
    assert.equal(p.available.toNumber(), (1000 - 600 + 9) * S);
    assert.equal(await balance(vault), 1009 * S);
    // the round counters the epoch record will carry
    assert.equal(p.epochPositions, 3);
    assert.equal(p.epochPremium.toNumber(), 9 * S);
    assert.equal(p.epochCollateral.toNumber(), 600 * S);
  });

  it("rejects a quote without the backend signer", async () => {
    const rogue = Keypair.generate();
    await expectRevert(
      () =>
        program.methods
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
          .rpc(),
      "BadQuoteSigner"
    );
  });

  it("rejects a premium below intrinsic value", async () => {
    // $260 strike put at a $220 spot is $40 in the money; nobody sells that for $1
    await expectRevert(() => buyPut(8, 260, 1, expiry + 60), "PremiumBelowIntrinsic");
  });

  it("rejects a quote that expires too far in the future", async () => {
    const far = (await chainTime()) + 86400;
    await expectRevert(() => buyPut(10, 200, 1, far), "QuoteTtlTooLong");
  });

  it("rejects a call pool whose oracle prices a different mint", async () => {
    // CALL wants collateral_mint == underlying; this passes USDC as both, so the
    // oracle's underlying is now a different mint from the one the pool names
    const [callPool] = PublicKey.findProgramAddressSync(
      [Buffer.from("pool"), usdc.toBuffer(), Buffer.from([1])],
      program.programId
    );
    const [callVault] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), callPool.toBuffer()],
      program.programId
    );
    await expectRevert(
      () =>
        program.methods
          .initPool(1, admin.publicKey, poolKeeper.publicKey)
          .accountsPartial({
            admin: admin.publicKey,
            collateralMint: usdc,
            underlying,
            oracle,
            pool: callPool,
            vault: callVault,
            tokenProgram: TOKEN_PROGRAM_ID,
            systemProgram: SystemProgram.programId,
          })
          .rpc(),
      "OracleMintMismatch"
    );
  });

  it("rejects a pool pointed at an oracle for another asset", async () => {
    const otherMint = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    const [otherOracle] = PublicKey.findProgramAddressSync(
      [Buffer.from("oracle"), otherMint.toBuffer()],
      program.programId
    );
    await program.methods
      .initOracle(admin.publicKey)
      .accountsPartial({
        admin: admin.publicKey,
        underlying: otherMint,
        oracle: otherOracle,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    // PUT wants collateral_mint != underlying, so this pairing is otherwise legal:
    // the only thing rejecting it is the oracle/underlying binding themselves
    const [wrongPool] = PublicKey.findProgramAddressSync(
      [Buffer.from("pool"), otherMint.toBuffer(), Buffer.from([PUT])],
      program.programId
    );
    const [wrongVault] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), wrongPool.toBuffer()],
      program.programId
    );
    await expectRevert(
      () =>
        program.methods
          .initPool(PUT, admin.publicKey, poolKeeper.publicKey)
          .accountsPartial({
            admin: admin.publicKey,
            collateralMint: otherMint,
            underlying,
            oracle: otherOracle,
            pool: wrongPool,
            vault: wrongVault,
            tokenProgram: TOKEN_PROGRAM_ID,
            systemProgram: SystemProgram.programId,
          })
          .rpc(),
      "OracleMintMismatch"
    );
  });

  it("locks writers in while the epoch has open positions", async () => {
    await expectRevert(() => withdrawShares(new BN(1 * S)), "EpochActive");
  });

  it("refuses to settle before the epoch price is latched", async () => {
    // spot drops to $200 and fills two thirds of the ring; the $216 print is a
    // wick the median ignores
    await pushPrices([...Array(13).fill(200), 198, 202, 216]);
    const o = await program.account.oracle.fetch(oracle);
    assert.equal(o.count, 21);

    await waitFor(expiry);
    // the epoch record does not exist until `close_epoch` creates it, so an
    // unclosed epoch cannot even assemble a settle instruction
    await expectRevert(() => settlePosition(cases[0].id), "AccountNotInitialized");
  });

  it("refuses a new option once the epoch has run out of time", async () => {
    // the epoch is still `open` — nothing has closed it — but its boundary has
    // passed, and that is a different rejection from a closed or never-opened one
    await expectRevert(
      async () => buyPut(7, 180, 1, (await chainTime()) + 60),
      "EpochExpired"
    );
    assert.equal(stateOf(await program.account.pool.fetch(pool)), "open");
    assert.isNull(await provider.connection.getAccountInfo(positionPda(7)));
  });

  it("rejects a push that deviates more than 10% from the median", async () => {
    // the median is $200; $500 is the decimal slip this guard exists to catch
    await expectRevert(
      () =>
        program.methods
          .pushPrice(usd(500))
          .accountsPartial({ oracle, keeper: admin.publicKey })
          .rpc(),
      "OracleDeviation"
    );
    assert.equal((await program.account.oracle.fetch(oracle)).count, 21, "a rejected push is not stored");
  });

  it("refuses a close from anyone but the keeper or the authority", async () => {
    // funded on purpose: Anchor creates every `init` account before it checks any
    // other constraint, so the rogue has to be able to pay the epoch record's rent
    // to even reach the gate that turns it away
    const rogue = Keypair.generate();
    const sig = await provider.connection.requestAirdrop(rogue.publicKey, LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(sig);

    await expectRevert(() => closeEpoch(rogue), "NotKeeper");
    assert.isNull(
      await provider.connection.getAccountInfo(epochRecordPda(pool, 1)),
      "a rejected close leaves no record behind"
    );
  });

  it("latches one settlement price for the whole epoch", async () => {
    await closeEpoch();
    const p = await program.account.pool.fetch(pool);
    assert.equal(p.settlePrice.toNumber(), 200 * S);
    assert.equal(stateOf(p), "closed");

    // spot slides to $182 after the latch: a second close must not re-pick the
    // price off the newer median, and settlement below still pays at $200
    await setSpot(182);
    await expectRevert(() => closeEpoch(), "EpochAlreadyClosed");
    assert.equal(
      (await program.account.pool.fetch(pool)).settlePrice.toNumber(),
      200 * S,
      "latched price must not move"
    );
  });

  it("refuses a settle from anyone but the keeper or the authority", async () => {
    const rogue = Keypair.generate();
    await expectRevert(() => settlePosition(cases[0].id, rogue), "NotKeeper");
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

  it("records the round permanently, after the last position settles", async () => {
    const rec = await program.account.epochRecord.fetch(epochRecordPda(pool, 1));
    assert.equal(rec.epoch.toNumber(), 1);
    assert.equal(rec.epochEnd.toNumber(), expiry);
    assert.equal(rec.settlePrice.toNumber(), 200 * S);
    assert.equal(rec.windowEnd.toNumber(), expiry, "the window ends at expiry, not at the close");
    assert.equal(rec.windowStart.toNumber(), expiry - 1800);
    assert.isAtLeast(rec.sampleCount, MIN_SETTLEMENT_SAMPLES);
    assert.isAbove(rec.settleSlot.toNumber(), 0);
    assert.equal(rec.totalPremium.toNumber(), 9 * S);
    assert.equal(rec.totalCollateral.toNumber(), 600 * S);
    assert.equal(rec.totalPayout.toNumber(), 20 * S);
    assert.equal(rec.positionsWritten, 3);
    assert.equal(rec.positionsSettled, 3, "the payout side is final only at this point");
    assert.equal(rec.sharePriceOpen.toNumber(), S, "the round opened at par");
    assert.equal(rec.sharePriceClose.toNumber(), 0.989 * S);
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
  let expiry4: number;

  it("rolls into a second epoch once the first has drained", async () => {
    await setSpot(220); // spot back to $220 so both new puts start out of the money
    expiry2 = (await chainTime()) + 40;
    await roll(expiry2);

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.epoch.toNumber(), 2);
    assert.equal(stateOf(p), "open", "epoch 2 is not closed yet");
    assert.equal(p.epochEnd.toNumber(), expiry2);
    assert.equal(p.epochPositions, 0, "the round counters reset at the roll");
    assert.equal(p.epochPremium.toNumber(), 0);
    assert.equal(p.epochCollateral.toNumber(), 0);
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
      await buyPut(c.id, c.strike, c.premium, expiry2 + 60);

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
    // the move has to happen INSIDE the window that ends at expiry: a print
    // after the boundary belongs to the next epoch, not this one
    await setSpot(190);
    await waitFor(expiry2);
    await closeEpoch();

    const latched = await program.account.pool.fetch(pool);
    assert.equal(latched.settlePrice.toNumber(), 190 * S);
    assert.equal(stateOf(latched), "closed");

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
    expiry3 = (await chainTime()) + 30;
    await roll(expiry3);

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.epoch.toNumber(), 3);
    assert.equal(stateOf(p), "open");
    assert.equal(await sharePrice(), priceBefore, "the roll itself must not move the price");

    await expectRevert(() => closeEpoch(), "EpochNotEnded");
    await expectRevert(async () => roll((await chainTime()) + 3600), "EpochNotClosed");
  });

  it("refuses a straggler from an earlier epoch", async () => {
    await setSpot(230);
    await waitFor(expiry3);
    await closeEpoch();

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.settlePrice.toNumber(), 230 * S);
    assert.equal(stateOf(p), "closed");

    // position 5 belongs to epoch 2; at $230 a $180 put is still worthless, but a
    // $230 latch must never be applied to it at all. The epoch check runs ahead of
    // the settled flag, so this is EpochMismatch and not AlreadySettled.
    await expectRevert(() => settlePosition(epoch2[1].id), "EpochMismatch");
    assert.equal((await program.account.optionPosition.fetch(positionPda(5))).payout.toNumber(), 0);
  });

  it("rolls a fourth time and drains the vault", async () => {
    expiry4 = (await chainTime()) + 20;
    await roll(expiry4);
    assert.equal((await program.account.pool.fetch(pool)).epoch.toNumber(), 4);

    const shares = (await program.account.lpPosition.fetch(lpPos)).shares;
    await withdrawShares(shares);

    assert.equal(await balance(lpToken), 984 * S);
    assert.equal(await balance(buyerToken), 116 * S);
    assert.equal(await balance(vault), 0);
  });

  // ---- fifth epoch: the operator-gated settlement paths ----

  // strike / premium / expected payout at the $210 epoch-5 settlement price
  const epoch5 = [
    { name: "settled by the keeper", id: 11, strike: 220, premium: 3, payout: 10 },
    { name: "swept by the authority", id: 12, strike: 200, premium: 1, payout: 0 },
  ];

  let expiry5: number;

  it("records an empty round too", async () => {
    await waitFor(expiry4);
    await closeEpoch();

    const rec = await program.account.epochRecord.fetch(epochRecordPda(pool, 4));
    assert.equal(rec.settlePrice.toNumber(), 230 * S);
    assert.equal(rec.positionsWritten, 0);
    assert.equal(rec.positionsSettled, 0);
    assert.equal(rec.totalPayout.toNumber(), 0);
  });

  it("opens a fifth epoch and writes two puts into it", async () => {
    expiry5 = (await chainTime()) + 45;
    await roll(expiry5);
    assert.equal(
      (await program.account.pool.fetch(pool)).sharePriceOpen.toNumber(),
      S,
      "an empty pool opens at par"
    );

    await depositUsd(500);
    for (const c of epoch5) {
      await buyPut(c.id, c.strike, c.premium, expiry5 + 60);
    }

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.openPositions, 2);
    assert.equal(p.locked.toNumber(), 420 * S);
    assert.equal(p.available.toNumber(), (500 - 420 + 4) * S);
  });

  it("settles it through the keeper and sweeps the rest with the authority", async () => {
    await setSpot(210);
    await waitFor(expiry5);
    await closeEpoch();
    assert.equal((await program.account.pool.fetch(pool)).settlePrice.toNumber(), 210 * S);

    // the keeper is not the authority: it may settle, it may not force-settle
    await settlePosition(epoch5[0].id, poolKeeper);
    assert.equal(
      (await program.account.optionPosition.fetch(positionPda(epoch5[0].id))).payout.toNumber(),
      epoch5[0].payout * S
    );

    await expectRevert(() => forceSettlePosition(epoch5[1].id, poolKeeper), "NotAuthority");
    await forceSettlePosition(epoch5[1].id);

    const pos = await program.account.optionPosition.fetch(positionPda(epoch5[1].id));
    assert.isTrue(pos.settled);
    assert.equal(pos.payout.toNumber(), epoch5[1].payout * S);

    const p = await program.account.pool.fetch(pool);
    assert.equal(p.openPositions, 0, "the sweep decrements the counter like a settle");
    assert.equal(p.locked.toNumber(), 0);
    assert.equal(p.available.toNumber(), (500 - 10 + 4) * S);
  });

  it("refuses to sweep a position twice", async () => {
    await expectRevert(() => forceSettlePosition(epoch5[1].id), "AlreadySettled");
  });

  it("writes the fifth round's record from what actually happened", async () => {
    const rec = await program.account.epochRecord.fetch(epochRecordPda(pool, 5));
    assert.equal(rec.epoch.toNumber(), 5);
    assert.equal(rec.epochEnd.toNumber(), expiry5);
    assert.equal(rec.settlePrice.toNumber(), 210 * S);
    assert.equal(rec.windowStart.toNumber(), expiry5 - 1800);
    assert.equal(rec.windowEnd.toNumber(), expiry5);
    assert.equal(rec.sampleCount, RING, "a full ring inside the window");
    assert.equal(rec.totalPremium.toNumber(), 4 * S);
    assert.equal(rec.totalCollateral.toNumber(), 420 * S);
    assert.equal(rec.totalPayout.toNumber(), 10 * S);
    assert.equal(rec.positionsWritten, 2);
    assert.equal(rec.positionsSettled, 2);
    assert.equal(rec.sharePriceOpen.toNumber(), S);
    // 500 deposited, 4 earned, 10 paid away, over 500 shares
    assert.equal(rec.sharePriceClose.toNumber(), 0.988 * S);
    assert.isAbove(rec.settleSlot.toNumber(), 0);
    assert.isAtLeast(rec.settleTs.toNumber(), expiry5);
  });

  // ---- the settlement window itself, on pools of its own ----

  // Two pools sharing one oracle and one epoch_end. The whole point of anchoring
  // the window on the boundary is that WHEN the operator closes cannot change the
  // number, and two pools closed minutes apart is the only way to actually watch
  // that happen.
  describe("boundary-anchored settlement window", () => {
    let windowFeed: ReturnType<typeof makeFeed>;
    let windowOracle: PublicKey;
    let early: PublicKey, late: PublicKey;
    let windowEnd: number;

    before(async () => {
      const f = await newFeed();
      windowOracle = f.oracle;
      windowFeed = f.feed;
      early = await newPool(f.oracle, f.underlying);
      late = await newPool(f.oracle, f.underlying);

      // twelve honest prints and one wick, all inside the window
      await windowFeed.push([...Array(12).fill(100), 110]);

      windowEnd = (await chainTime()) + 12;
      await rollFor(early, windowEnd);
      await rollFor(late, windowEnd);
      await waitFor(windowEnd);
    });

    it("ignores a wick inside the window", async () => {
      await closeEpochFor(early, windowOracle);
      const rec = await program.account.epochRecord.fetch(epochRecordPda(early, 1));
      assert.equal(rec.settlePrice.toNumber(), 100 * S, "one print out of thirteen moves nothing");
      assert.equal(rec.sampleCount, 13);
    });

    it("latches the same price early or late, whatever spot does afterwards", async () => {
      // The operator stalls, and spot runs away from the boundary while it stalls.
      // Kept inside the ring's spare capacity on purpose: evicting the window is a
      // different failure, and it has a test of its own below. The extra second is
      // not cosmetic — the chain clock moves in lumps, and a push that lands ON the
      // boundary is inside the inclusive window, not after it.
      await waitFor(windowEnd + 2);
      await windowFeed.setSpot(110);
      assert.equal(windowFeed.median(), 110, "the live median really did move");
      assert.isAbove((await chainTime()) - windowEnd, 0);

      await closeEpochFor(late, windowOracle);

      const first = await program.account.epochRecord.fetch(epochRecordPda(early, 1));
      const second = await program.account.epochRecord.fetch(epochRecordPda(late, 1));
      assert.equal(
        second.settlePrice.toNumber(),
        first.settlePrice.toNumber(),
        "the close time must not be worth anything"
      );
      assert.equal(second.windowStart.toNumber(), first.windowStart.toNumber());
      assert.equal(second.windowEnd.toNumber(), first.windowEnd.toNumber());
      assert.equal(second.sampleCount, first.sampleCount);
      assert.isAbove(
        second.settleSlot.toNumber(),
        first.settleSlot.toNumber(),
        "and they really were latched at different times"
      );
    });

    it("follows a sustained move that happens inside the window", async () => {
      const end = (await chainTime()) + 12;
      await rollFor(early, end);

      // same shape of move as the one above, on the other side of the boundary
      await windowFeed.setSpot(130);
      await waitFor(end);
      await closeEpochFor(early, windowOracle);

      const rec = await program.account.epochRecord.fetch(epochRecordPda(early, 2));
      assert.equal(rec.settlePrice.toNumber(), 130 * S);
    });

    it("cannot close once the keeper's own pushes have evicted the window", async () => {
      const lost = await newFeed();
      const lostPool = await newPool(lost.oracle, lost.underlying);
      await lost.feed.push(Array(12).fill(100));

      const end = (await chainTime()) + 8;
      await rollFor(lostPool, end);
      // strictly past the boundary, so none of what follows counts as in-window
      await waitFor(end + 2);

      // the keeper stalls but keeps the feed running: a whole ring of flat prints
      // passes the deviation guard and rotates every in-window sample out of
      // existence. The recovery is to stop pushing BEFORE this point — after it,
      // the inputs the window needs are simply gone.
      await lost.feed.push(Array(RING).fill(100));
      await expectRevert(() => closeEpochFor(lostPool, lost.oracle), "InsufficientSamples");
    });

    it("refuses to latch a window too thin to be a settlement price", async () => {
      const thin = await newFeed();
      const thinPool = await newPool(thin.oracle, thin.underlying);
      await thin.feed.push(Array(MIN_SETTLEMENT_SAMPLES - 1).fill(100));

      const end = (await chainTime()) + 8;
      await rollFor(thinPool, end);
      await waitFor(end);

      await expectRevert(
        () => closeEpochFor(thinPool, thin.oracle),
        "InsufficientSamples"
      );
      assert.isNull(
        await provider.connection.getAccountInfo(epochRecordPda(thinPool, 1)),
        "a failed close leaves no record behind"
      );
    });
  });

  // ---- genesis: a pool that has never opened an epoch ----

  describe("genesis epoch state", () => {
    it("rolls from genesis straight into epoch 1, with no close in between", async () => {
      const fresh = await newPool(oracle, underlying);

      const born = await program.account.pool.fetch(fresh);
      assert.equal(stateOf(born), "genesis");
      assert.equal(born.epoch.toNumber(), 0);
      // zero, and unambiguously not a settlement: the state says so, and nothing
      // reads this field unless the state is `closed`
      assert.equal(born.settlePrice.toNumber(), 0);

      await rollFor(fresh, (await chainTime()) + 60);

      const opened = await program.account.pool.fetch(fresh);
      assert.equal(opened.epoch.toNumber(), 1);
      assert.equal(stateOf(opened), "open");
    });

    it("refuses to close an epoch the pool never opened", async () => {
      const fresh = await newPool(oracle, underlying);
      await expectRevert(() => closeEpochFor(fresh, oracle), "EpochNotStarted");
      assert.equal(stateOf(await program.account.pool.fetch(fresh)), "genesis");
    });
  });
});
