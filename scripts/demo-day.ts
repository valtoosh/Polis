/**
 * polis :: demo-day.ts
 *
 * Daily cron. Polls every relevant PDA, writes a JSON snapshot under
 * `scripts/logs/daily-snapshots/YYYY-MM-DD.json`, and prints a coloured summary.
 *
 * Snapshotted state per day (everything we need to audit at end of demo):
 *   - AgentProfile.is_retired
 *   - StakeBond.amount
 *   - Wallet USDC balance
 *   - Wallet.total_inflow / total_outflow
 *   - Pool USDC balance
 *   - VouchPool USDC balance
 *   - Customer traffic counters (parsed from logs/customer-traffic.csv if present)
 *
 * Schedule (operator-side) example:
 *   0 0 * * *  cd ~/polis/scripts && yarn demo-day
 *
 * CLI:
 *   yarn demo-day [--date=YYYY-MM-DD]   # override snapshot date (testing)
 */

import * as fs from "fs";
import * as path from "path";
import { PublicKey, Connection, Keypair } from "@solana/web3.js";
import { getAccount } from "@solana/spl-token";
import * as anchor from "@coral-xyz/anchor";
import chalk from "chalk";

import { loadConfig, makeConnection, parseArgs, logsDir, baseToUsd, setupProgram } from "./_common";

interface DailySnapshot {
  date: string;
  iso: string;
  cluster: string;
  agent: {
    code_hash_hex: string;
    operator: string;
    profile_pda: string;
    is_retired: boolean | null;
  };
  balances_usd: {
    wallet: number;
    stake_bond: number;
    pool: number;
    vouch_pool: number;
  };
  wallet_counters: {
    total_inflow_usd: number | null;
    total_outflow_usd: number | null;
    is_paused: boolean | null;
    per_trade_cap_bps: number | null;
  };
  reputation: {
    /** total_staked across this agent's VouchPool, USD. */
    total_vouched_usd: number;
    /** Placeholder until we sample max-staked across all agents. */
    rep_ratio: number | null;
  };
  credit: {
    active: boolean | null;
    principal_usd: number | null;
    accrued_interest_usd: number | null;
    apr_bps: number | null;
    last_accrual_iso: string | null;
  };
  traffic: {
    calls_today: number;
    earnings_today_usd: number;
    cumulative_calls: number;
    cumulative_earnings_usd: number;
  };
}

async function fetchAtaAmount(connection: Connection, ata: PublicKey): Promise<bigint> {
  try {
    const acct = await getAccount(connection, ata, "confirmed");
    return acct.amount;
  } catch {
    return 0n;
  }
}

interface WalletAccount {
  isPaused: boolean;
  perTradeCapBps: number;
  totalInflow: bigint;
  totalOutflow: bigint;
}

interface AgentProfileAccount {
  isRetired: boolean;
}

interface CreditLineAccount {
  isActive: boolean;
  principal: bigint;
  accruedInterest: bigint;
  aprBps: number;
  lastAccrual: bigint;
}

/**
 * Decode a Wallet account through the IDL. Returns null if the account is
 * absent / wrong type. The IDL ensures we read the canonical field layout
 * even if `programs/wallet/src/lib.rs` shifts field order in a future build.
 */
async function fetchWalletAccount(
  walletProgram: anchor.Program,
  walletPda: PublicKey,
): Promise<WalletAccount | null> {
  try {
    const w = (await (walletProgram.account as any).wallet.fetch(walletPda)) as {
      isPaused: boolean;
      perTradeCapBps: number;
      totalInflow: anchor.BN;
      totalOutflow: anchor.BN;
    };
    return {
      isPaused: w.isPaused,
      perTradeCapBps: w.perTradeCapBps,
      totalInflow: BigInt(w.totalInflow.toString()),
      totalOutflow: BigInt(w.totalOutflow.toString()),
    };
  } catch {
    return null;
  }
}

async function fetchAgentProfile(
  identityProgram: anchor.Program,
  profilePda: PublicKey,
): Promise<AgentProfileAccount | null> {
  try {
    const p = (await (identityProgram.account as any).agentProfile.fetch(profilePda)) as {
      isRetired: boolean;
    };
    return { isRetired: p.isRetired };
  } catch {
    return null;
  }
}

async function fetchCreditLine(
  creditProgram: anchor.Program,
  creditLinePda: PublicKey,
): Promise<CreditLineAccount | null> {
  try {
    const c = (await (creditProgram.account as any).creditLine.fetch(creditLinePda)) as {
      isActive: boolean;
      principal: anchor.BN;
      accruedInterest: anchor.BN;
      aprBps: number;
      lastAccrual: anchor.BN;
    };
    return {
      isActive: c.isActive,
      principal: BigInt(c.principal.toString()),
      accruedInterest: BigInt(c.accruedInterest.toString()),
      aprBps: c.aprBps,
      lastAccrual: BigInt(c.lastAccrual.toString()),
    };
  } catch {
    return null;
  }
}

function trafficForDay(date: string): { calls: number; earnings: number } {
  const csv = path.join(logsDir(), "customer-traffic.csv");
  if (!fs.existsSync(csv)) return { calls: 0, earnings: 0 };
  const lines = fs.readFileSync(csv, "utf-8").split("\n").slice(1).filter(Boolean);
  let calls = 0;
  let earnings = 0;
  for (const line of lines) {
    const cols = line.split(",");
    if (cols.length < 8) continue;
    const iso = cols[0];
    if (!iso.startsWith(date)) continue;
    const amount = parseFloat(cols[3]);
    const ok = cols[7] === "true";
    calls += 1;
    if (ok && Number.isFinite(amount)) earnings += amount;
  }
  return { calls, earnings };
}

function trafficCumulative(throughDate: string): { calls: number; earnings: number } {
  const csv = path.join(logsDir(), "customer-traffic.csv");
  if (!fs.existsSync(csv)) return { calls: 0, earnings: 0 };
  const lines = fs.readFileSync(csv, "utf-8").split("\n").slice(1).filter(Boolean);
  let calls = 0;
  let earnings = 0;
  for (const line of lines) {
    const cols = line.split(",");
    if (cols.length < 8) continue;
    const iso = cols[0];
    if (iso.slice(0, 10) > throughDate) continue;
    const amount = parseFloat(cols[3]);
    const ok = cols[7] === "true";
    calls += 1;
    if (ok && Number.isFinite(amount)) earnings += amount;
  }
  return { calls, earnings };
}

async function main(): Promise<void> {
  const { flags } = parseArgs();
  const cfg = loadConfig();
  const connection = makeConnection(cfg);

  const today = (flags["date"] as string) || new Date().toISOString().slice(0, 10);
  const iso = new Date().toISOString();

  if (!cfg.agentWalletAta || !cfg.agentProfilePda || !cfg.agentWalletPda) {
    throw new Error("Agent PDA addresses missing in .anchor-config.json — run bootstrap first.");
  }

  // Programs are loaded read-only — we never sign with the burner wallet,
  // we only call `program.account.<Name>.fetch(...)`. A throwaway keypair
  // satisfies the Anchor provider's `Wallet` constructor without needing the
  // operator key on this host.
  const readerWallet = new anchor.Wallet(Keypair.generate());
  const walletProgram = setupProgram("wallet", connection, readerWallet);
  const identityProgram = setupProgram("identity", connection, readerWallet);
  const creditProgram = setupProgram("credit", connection, readerWallet);
  const reputationProgram = setupProgram("reputation", connection, readerWallet);

  // RepStats records the rolling max of `total_staked` across all VouchPools;
  // dividing our pool's stake by that gives the on-chain reputation ratio
  // without iterating program accounts.
  const reputationPid = new PublicKey(cfg.programs.reputation);
  const [repStatsPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("rep_stats")],
    reputationPid,
  );
  const [agentVouchPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("vouch"), Buffer.from(cfg.agentCodeHashHex || "", "hex")],
    reputationPid,
  );
  let repRatio: number | null = null;
  try {
    const repStats = (await (reputationProgram.account as any).repStats.fetch(repStatsPda)) as {
      maxTotalStakedSeen: anchor.BN;
    };
    const vouchPool = (await (reputationProgram.account as any).vouchPool.fetch(agentVouchPda)) as {
      totalStaked: anchor.BN;
    };
    const maxSeen = BigInt(repStats.maxTotalStakedSeen.toString());
    const own = BigInt(vouchPool.totalStaked.toString());
    if (maxSeen > 0n) {
      // Compute ratio in fp via Number(); fine since vouch totals never exceed
      // safe integer USDC base units in the prototype.
      repRatio = Math.min(1.0, Number(own) / Number(maxSeen));
    } else {
      repRatio = own > 0n ? 1.0 : 0.0;
    }
  } catch {
    // Reputation accounts not yet initialised → leave null.
  }

  const walletAta = new PublicKey(cfg.agentWalletAta);
  const walletPda = new PublicKey(cfg.agentWalletPda);
  const profilePda = new PublicKey(cfg.agentProfilePda);
  const creditLinePda = new PublicKey(cfg.agentCreditLinePda!);
  const stakeBondAta = await deriveAtaSafe(connection, new PublicKey(cfg.agentStakeBondPda!), cfg.usdcMint);
  const poolAta = new PublicKey(cfg.poolUsdcAta!);
  const vouchPoolAta = await deriveAtaSafe(connection, new PublicKey(cfg.agentVouchPoolPda!), cfg.usdcMint);

  const [walletBal, stakeBal, poolBal, vouchBal, walletParsed, profileParsed, creditParsed] = await Promise.all([
    fetchAtaAmount(connection, walletAta),
    fetchAtaAmount(connection, stakeBondAta),
    fetchAtaAmount(connection, poolAta),
    fetchAtaAmount(connection, vouchPoolAta),
    fetchWalletAccount(walletProgram, walletPda),
    fetchAgentProfile(identityProgram, profilePda),
    fetchCreditLine(creditProgram, creditLinePda),
  ]);

  const today_traffic = trafficForDay(today);
  const cumulative = trafficCumulative(today);

  const snapshot: DailySnapshot = {
    date: today,
    iso,
    cluster: cfg.cluster,
    agent: {
      code_hash_hex: cfg.agentCodeHashHex || "",
      operator: cfg.agentOperatorPubkey || "",
      profile_pda: cfg.agentProfilePda || "",
      is_retired: profileParsed?.isRetired ?? null,
    },
    balances_usd: {
      wallet: baseToUsd(walletBal),
      stake_bond: baseToUsd(stakeBal),
      pool: baseToUsd(poolBal),
      vouch_pool: baseToUsd(vouchBal),
    },
    wallet_counters: {
      total_inflow_usd: walletParsed ? baseToUsd(walletParsed.totalInflow) : null,
      total_outflow_usd: walletParsed ? baseToUsd(walletParsed.totalOutflow) : null,
      is_paused: walletParsed?.isPaused ?? null,
      per_trade_cap_bps: walletParsed?.perTradeCapBps ?? null,
    },
    reputation: {
      total_vouched_usd: baseToUsd(vouchBal),
      rep_ratio: repRatio,
    },
    credit: {
      active: creditParsed?.isActive ?? null,
      principal_usd: creditParsed ? baseToUsd(creditParsed.principal) : null,
      accrued_interest_usd: creditParsed ? baseToUsd(creditParsed.accruedInterest) : null,
      apr_bps: creditParsed?.aprBps ?? null,
      last_accrual_iso: creditParsed
        ? new Date(Number(creditParsed.lastAccrual) * 1000).toISOString()
        : null,
    },
    traffic: {
      calls_today: today_traffic.calls,
      earnings_today_usd: today_traffic.earnings,
      cumulative_calls: cumulative.calls,
      cumulative_earnings_usd: cumulative.earnings,
    },
  };

  // Persist
  const dir = path.join(logsDir(), "daily-snapshots");
  if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
  const outPath = path.join(dir, `${today}.json`);
  fs.writeFileSync(outPath, JSON.stringify(snapshot, null, 2) + "\n");

  printSummary(snapshot);

  console.log(chalk.gray(`\n  → snapshot written: ${outPath}`));
}

async function deriveAtaSafe(_c: Connection, owner: PublicKey, mint: string): Promise<PublicKey> {
  const { getAssociatedTokenAddress } = await import("@solana/spl-token");
  return getAssociatedTokenAddress(new PublicKey(mint), owner, true);
}

function printSummary(s: DailySnapshot): void {
  console.log(chalk.cyan.bold(`\npolis :: daily snapshot — ${s.date}`));
  console.log(chalk.gray(`  cluster: ${s.cluster}  taken: ${s.iso}\n`));

  const tbl: [string, string][] = [
    ["Agent retired", s.agent.is_retired === null ? chalk.gray("unknown") : s.agent.is_retired ? chalk.red("YES") : chalk.green("no")],
    ["Wallet balance", `$${s.balances_usd.wallet.toFixed(2)}`],
    ["Stake bond", `$${s.balances_usd.stake_bond.toFixed(2)}`],
    ["Pool balance", `$${s.balances_usd.pool.toFixed(2)}`],
    ["VouchPool", `$${s.balances_usd.vouch_pool.toFixed(2)}`],
    [
      "Wallet inflow / outflow",
      `${s.wallet_counters.total_inflow_usd === null ? "?" : "$" + s.wallet_counters.total_inflow_usd.toFixed(2)} / ` +
        `${s.wallet_counters.total_outflow_usd === null ? "?" : "$" + s.wallet_counters.total_outflow_usd.toFixed(2)}`,
    ],
    ["Paused", s.wallet_counters.is_paused === null ? chalk.gray("?") : s.wallet_counters.is_paused ? chalk.red("YES") : chalk.green("no")],
    ["Per-trade cap (bps)", s.wallet_counters.per_trade_cap_bps?.toString() ?? "?"],
    ["Calls today", s.traffic.calls_today.toString()],
    ["Earnings today", `$${s.traffic.earnings_today_usd.toFixed(2)}`],
    ["Cumulative calls", s.traffic.cumulative_calls.toString()],
    ["Cumulative earnings", `$${s.traffic.cumulative_earnings_usd.toFixed(2)}`],
  ];

  for (const [k, v] of tbl) {
    console.log(`  ${chalk.bold(k.padEnd(24))} ${v}`);
  }
}

main().catch((e: Error) => {
  console.error(chalk.red.bold(`✗ demo-day failed: ${e.message}`));
  console.error(e.stack);
  process.exit(1);
});
