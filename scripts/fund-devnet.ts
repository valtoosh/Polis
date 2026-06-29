/**
 * polis :: devnet funding helper
 *
 * Drips devnet SOL + USDC into the keypairs needed for the 14-day demo:
 *   - operator        (registers the agent, posts stake bond, pays tx fees)
 *   - lp1, lp2        (seed the credit Pool with $600 + $400)
 *   - voucher1        (initial vouch of $10)
 *   - customer1..N    (Poisson-distributed traffic in simulate-customers.ts)
 *
 * Devnet USDC mint is `4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU` (Circle's
 * official devnet faucet). Mint authority is held by Circle; the canonical
 * faucet UI is at https://faucet.circle.com — but for programmatic funding we
 * use the `airdropTo` endpoint of Solana's standard RPC for SOL, and require
 * the operator to pre-fund a "devnet USDC dispenser" account that this script
 * drains pro-rata into the demo wallets.
 *
 * Why a dispenser instead of an automated mint? Because Circle's devnet USDC
 * mint authority is not held by us. The cleanest path:
 *   1. Operator gets devnet USDC manually from https://faucet.circle.com
 *      into a "dispenser" keypair (passed via --dispenser-keypair).
 *   2. This script transfers from dispenser -> each demo wallet's ATA.
 *
 * Usage:
 *   ts-node fund-devnet.ts \
 *     --dispenser-keypair ~/.config/solana/dispenser.json \
 *     --operator <pubkey> \
 *     --lp1 <pubkey> --lp1-usdc 600 \
 *     --lp2 <pubkey> --lp2-usdc 400 \
 *     --voucher1 <pubkey> --voucher1-usdc 10 \
 *     --customer-keys ~/customers/c1.json,~/customers/c2.json \
 *     --customer-usdc 5
 */

import * as fs from "fs";
import {
  Connection,
  Keypair,
  PublicKey,
  Transaction,
  sendAndConfirmTransaction,
  LAMPORTS_PER_SOL,
} from "@solana/web3.js";
import {
  getOrCreateAssociatedTokenAccount,
  transfer,
  getMint,
} from "@solana/spl-token";
import {
  loadConfig,
  loadKeypair,
  usdToBase,
  baseToUsd,
} from "./_common";

// ---------------------------------------------------------------------------
// arg parsing (minimal — no yargs)
// ---------------------------------------------------------------------------

function getArg(name: string): string | undefined {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : undefined;
}

function requireArg(name: string): string {
  const v = getArg(name);
  if (!v) {
    console.error(`Missing required argument: --${name}`);
    process.exit(1);
  }
  return v;
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
  const config = loadConfig();
  const conn = new Connection(config.rpcUrl, "confirmed");

  // Dispenser is the keypair that already holds devnet USDC (from Circle faucet).
  const dispenserPath = requireArg("dispenser-keypair");
  const dispenser = loadKeypair(dispenserPath);
  const usdcMint = new PublicKey(config.usdcMint);

  console.log(`[fund-devnet] dispenser: ${dispenser.publicKey.toBase58()}`);
  console.log(`[fund-devnet] USDC mint:  ${usdcMint.toBase58()}`);

  // Verify dispenser has SOL for fees + USDC to disburse.
  const dispenserSol = await conn.getBalance(dispenser.publicKey);
  if (dispenserSol < 0.1 * LAMPORTS_PER_SOL) {
    console.warn(
      `[fund-devnet] dispenser has < 0.1 SOL (${dispenserSol / LAMPORTS_PER_SOL}); ` +
        `airdropping 1 SOL via RPC faucet`,
    );
    const sig = await conn.requestAirdrop(dispenser.publicKey, LAMPORTS_PER_SOL);
    await conn.confirmTransaction(sig, "confirmed");
  }

  const dispenserUsdcAta = await getOrCreateAssociatedTokenAccount(
    conn,
    dispenser,
    usdcMint,
    dispenser.publicKey,
  );
  const dispenserUsdc = baseToUsd(BigInt(dispenserUsdcAta.amount.toString()));
  console.log(`[fund-devnet] dispenser USDC: $${dispenserUsdc.toFixed(2)}`);

  // Targets.
  type Target = { name: string; pubkey: PublicKey; usdAmount: number };
  const targets: Target[] = [];

  const operator = getArg("operator");
  if (operator) {
    targets.push({ name: "operator", pubkey: new PublicKey(operator), usdAmount: 60 });
    // $60 covers the $50 stake bond + spare for ATA rent.
  }

  for (const [flag, defaultUsd] of [
    ["lp1", 600],
    ["lp2", 400],
    ["voucher1", 10],
  ] as const) {
    const pk = getArg(flag);
    if (pk) {
      const usd = Number(getArg(`${flag}-usdc`) ?? defaultUsd);
      targets.push({ name: flag, pubkey: new PublicKey(pk), usdAmount: usd });
    }
  }

  // Customers: comma-separated list of keypair file paths; identical USDC each.
  const customerKeysCsv = getArg("customer-keys");
  const customerUsdc = Number(getArg("customer-usdc") ?? 5);
  if (customerKeysCsv) {
    for (const path of customerKeysCsv.split(",").map((s) => s.trim())) {
      const kp = loadKeypair(path);
      targets.push({
        name: `customer:${kp.publicKey.toBase58().slice(0, 8)}`,
        pubkey: kp.publicKey,
        usdAmount: customerUsdc,
      });
    }
  }

  if (targets.length === 0) {
    console.error("[fund-devnet] no targets specified; pass --operator and/or --lp1 etc.");
    process.exit(1);
  }

  const totalNeeded = targets.reduce((s, t) => s + t.usdAmount, 0);
  if (dispenserUsdc < totalNeeded) {
    console.error(
      `[fund-devnet] dispenser short: needs $${totalNeeded}, has $${dispenserUsdc.toFixed(2)}. ` +
        `Top up at https://faucet.circle.com (devnet) and rerun.`,
    );
    process.exit(1);
  }

  // SOL airdrop for each target first (so they can pay tx fees + ATA rent).
  for (const t of targets) {
    const bal = await conn.getBalance(t.pubkey);
    if (bal < 0.05 * LAMPORTS_PER_SOL) {
      console.log(`[fund-devnet] airdrop 0.5 SOL → ${t.name} (${t.pubkey.toBase58().slice(0, 8)})`);
      try {
        const sig = await conn.requestAirdrop(t.pubkey, 0.5 * LAMPORTS_PER_SOL);
        await conn.confirmTransaction(sig, "confirmed");
      } catch (err) {
        console.warn(`[fund-devnet] airdrop failed for ${t.name}: ${err}`);
      }
    }
  }

  // USDC transfer to each target.
  for (const t of targets) {
    const targetAta = await getOrCreateAssociatedTokenAccount(
      conn,
      dispenser,           // payer (dispenser pays rent for new ATAs)
      usdcMint,
      t.pubkey,
    );
    const sig = await transfer(
      conn,
      dispenser,           // payer of tx fees
      dispenserUsdcAta.address,
      targetAta.address,
      dispenser,           // authority on the dispenser ATA
      usdToBase(t.usdAmount),
    );
    console.log(
      `[fund-devnet] $${t.usdAmount} USDC → ${t.name} (${t.pubkey.toBase58().slice(0, 8)})  sig: ${sig.slice(0, 12)}…`,
    );
  }

  console.log(`[fund-devnet] done. funded ${targets.length} accounts with $${totalNeeded} total.`);
}

main().catch((err) => {
  console.error("[fund-devnet] fatal:", err);
  process.exit(1);
});
