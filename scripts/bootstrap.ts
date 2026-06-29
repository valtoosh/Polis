/**
 * polis :: bootstrap.ts
 *
 * One-time devnet setup for the 14-day demo.
 *
 * Sequence (idempotent — safe to re-run; existing accounts are detected and
 * skipped):
 *   1. Load admin keypair from $ADMIN_KEYPAIR_PATH.
 *   2. Read program IDs from `../.anchor-config.json`.
 *   3. Initialise the reputation `RepStats` global PDA.
 *   4. Initialise the credit `Pool` PDA + Pool's USDC ATA.
 *   5. Seed the Pool with $1,000 USDC from two LPs (LP1=$600, LP2=$400),
 *      via `credit.deposit(...)`.
 *   6. Register the demo agent (`identity.register_agent`) with a $50 stake
 *      bond, then init the agent's Wallet PDA (`wallet.init_wallet`).
 *   7. Initialise the agent's reputation `VouchPool` (`reputation.init_vouch_pool`),
 *      then have ONE initial voucher stake $10 (`reputation.vouch`).
 *   8. Print every PDA + balance and update `.anchor-config.json`.
 *
 * CLI usage:
 *   yarn bootstrap --code-hash-file=../agent/code-hash.txt
 *
 * Required env (loaded via dotenv from polis/.env):
 *   ADMIN_KEYPAIR_PATH=~/.config/solana/id.json
 *   LP1_KEYPAIR_PATH=...
 *   LP2_KEYPAIR_PATH=...
 *   AGENT_OPERATOR_KEYPAIR_PATH=...
 *   VOUCHER_KEYPAIR_PATH=...
 *   SOLANA_RPC_URL=https://api.devnet.solana.com   # optional override
 */

import * as anchor from "@coral-xyz/anchor";
import { PublicKey } from "@solana/web3.js";
import {
  getAssociatedTokenAddress,
  getAccount,
} from "@solana/spl-token";
import chalk from "chalk";

import {
  loadConfig,
  saveConfig,
  loadKeypair,
  makeConnection,
  parseArgs,
  readCodeHashFile,
  deriveAgentProfilePda,
  deriveStakeBondPda,
  deriveWalletPda,
  deriveCreditLinePda,
  derivePoolPda,
  deriveVouchPoolPda,
  usdToBase,
  baseToUsd,
  setupProgram,
} from "./_common";

interface Env {
  ADMIN_KEYPAIR_PATH: string;
  LP1_KEYPAIR_PATH: string;
  LP2_KEYPAIR_PATH: string;
  AGENT_OPERATOR_KEYPAIR_PATH: string;
  VOUCHER_KEYPAIR_PATH: string;
}

function requireEnv(): Env {
  const required: (keyof Env)[] = [
    "ADMIN_KEYPAIR_PATH",
    "LP1_KEYPAIR_PATH",
    "LP2_KEYPAIR_PATH",
    "AGENT_OPERATOR_KEYPAIR_PATH",
    "VOUCHER_KEYPAIR_PATH",
  ];
  const missing = required.filter((k) => !process.env[k]);
  if (missing.length) {
    console.error(chalk.red(`Missing env vars: ${missing.join(", ")}`));
    process.exit(1);
  }
  return {
    ADMIN_KEYPAIR_PATH: process.env.ADMIN_KEYPAIR_PATH!,
    LP1_KEYPAIR_PATH: process.env.LP1_KEYPAIR_PATH!,
    LP2_KEYPAIR_PATH: process.env.LP2_KEYPAIR_PATH!,
    AGENT_OPERATOR_KEYPAIR_PATH: process.env.AGENT_OPERATOR_KEYPAIR_PATH!,
    VOUCHER_KEYPAIR_PATH: process.env.VOUCHER_KEYPAIR_PATH!,
  };
}

async function main(): Promise<void> {
  console.log(chalk.cyan.bold("\n╭───────────────────────────────────────────╮"));
  console.log(chalk.cyan.bold("│  polis :: bootstrap (14-day demo setup)   │"));
  console.log(chalk.cyan.bold("╰───────────────────────────────────────────╯\n"));

  const { flags } = parseArgs();
  const codeHashFile = (flags["code-hash-file"] as string) || "../agent/code-hash.txt";

  const env = requireEnv();
  const cfg = loadConfig();
  const connection = makeConnection(cfg);

  const admin = loadKeypair(env.ADMIN_KEYPAIR_PATH);
  const lp1 = loadKeypair(env.LP1_KEYPAIR_PATH);
  const lp2 = loadKeypair(env.LP2_KEYPAIR_PATH);
  const operator = loadKeypair(env.AGENT_OPERATOR_KEYPAIR_PATH);
  const voucher = loadKeypair(env.VOUCHER_KEYPAIR_PATH);

  console.log(chalk.gray(`  cluster:  ${cfg.cluster}`));
  console.log(chalk.gray(`  RPC URL:  ${connection.rpcEndpoint}`));
  console.log(chalk.gray(`  USDC mint:    ${cfg.usdcMint}`));
  console.log(chalk.gray(`  admin:        ${admin.publicKey.toBase58()}`));
  console.log(chalk.gray(`  LP1:          ${lp1.publicKey.toBase58()}`));
  console.log(chalk.gray(`  LP2:          ${lp2.publicKey.toBase58()}`));
  console.log(chalk.gray(`  operator:     ${operator.publicKey.toBase58()}`));
  console.log(chalk.gray(`  voucher:      ${voucher.publicKey.toBase58()}\n`));

  // -----------------------------------------------------------------------
  // Program clients. One AnchorProvider per signer so account writes are
  // attributed correctly. The "view" program (admin signer) handles
  // admin-only inits; per-LP / per-voucher operations use their own clients
  // so they sign their own transfers.
  // -----------------------------------------------------------------------
  const adminWallet = new anchor.Wallet(admin);
  const lp1Wallet = new anchor.Wallet(lp1);
  const lp2Wallet = new anchor.Wallet(lp2);
  const operatorWallet = new anchor.Wallet(operator);
  const voucherWallet = new anchor.Wallet(voucher);

  // Default provider — used for read-only queries and admin-signed calls.
  anchor.setProvider(
    new anchor.AnchorProvider(connection, adminWallet, {
      commitment: "confirmed",
      preflightCommitment: "confirmed",
    }),
  );

  const identityAsOperator = setupProgram("identity", connection, operatorWallet);
  const walletAsAdmin = setupProgram("wallet", connection, adminWallet);
  const creditAsAdmin = setupProgram("credit", connection, adminWallet);
  const creditAsLp1 = setupProgram("credit", connection, lp1Wallet);
  const creditAsLp2 = setupProgram("credit", connection, lp2Wallet);
  const reputationAsAdmin = setupProgram("reputation", connection, adminWallet);
  const reputationAsVoucher = setupProgram("reputation", connection, voucherWallet);

  const usdcMint = new PublicKey(cfg.usdcMint);
  const identityPid = new PublicKey(cfg.programs.identity);
  const walletPid = new PublicKey(cfg.programs.wallet);
  const x402Pid = new PublicKey(cfg.programs.x402);
  const creditPid = new PublicKey(cfg.programs.credit);
  const reputationPid = new PublicKey(cfg.programs.reputation);

  // Sanity: IDLs must match the IDs in .anchor-config.json. Anchor 0.30 embeds
  // the canonical address inside the IDL; if they differ, programs will fail
  // discriminator/PDA checks in confusing ways.
  for (const [name, pid, program] of [
    ["identity", identityPid, identityAsOperator],
    ["wallet", walletPid, walletAsAdmin],
    ["credit", creditPid, creditAsAdmin],
    ["reputation", reputationPid, reputationAsAdmin],
  ] as const) {
    if (!program.programId.equals(pid)) {
      throw new Error(
        `IDL/config mismatch for ${name}: IDL says ${program.programId.toBase58()}, ` +
          `.anchor-config.json says ${pid.toBase58()}. Re-run \`anchor build\` after deploy.`,
      );
    }
  }

  // -----------------------------------------------------------------------
  // Code-hash + PDA derivations
  // -----------------------------------------------------------------------
  const codeHash = readCodeHashFile(codeHashFile);
  console.log(chalk.cyan(`  code_hash:    ${codeHash.toString("hex")}\n`));

  const [agentProfilePda] = deriveAgentProfilePda(identityPid, codeHash);
  const [stakeBondPda] = deriveStakeBondPda(identityPid, codeHash);
  const [walletPda] = deriveWalletPda(walletPid, codeHash);
  const [creditLinePda] = deriveCreditLinePda(creditPid, codeHash);
  const [poolPda] = derivePoolPda(creditPid);
  const [vouchPoolPda] = deriveVouchPoolPda(reputationPid, codeHash);
  const [repStatsPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("rep_stats")],
    reputationPid,
  );

  const walletAta = await getAssociatedTokenAddress(usdcMint, walletPda, true);
  const stakeBondAta = await getAssociatedTokenAddress(usdcMint, stakeBondPda, true);
  const poolAta = await getAssociatedTokenAddress(usdcMint, poolPda, true);
  const vouchPoolAta = await getAssociatedTokenAddress(usdcMint, vouchPoolPda, true);

  // -----------------------------------------------------------------------
  // Step 1: RepStats global init (admin-only)
  // -----------------------------------------------------------------------
  console.log(chalk.yellow("→ Step 1: Reputation RepStats global init"));
  await ensureRepStats(reputationAsAdmin, repStatsPda, creditPid);

  // -----------------------------------------------------------------------
  // Step 2: Credit Pool PDA + Pool USDC ATA
  // -----------------------------------------------------------------------
  console.log(chalk.yellow("\n→ Step 2: Credit Pool init"));
  await ensurePool(creditAsAdmin, poolPda, poolAta, usdcMint, reputationPid);

  // -----------------------------------------------------------------------
  // Step 3: Seed pool with $1,000 ($600 from LP1, $400 from LP2)
  // -----------------------------------------------------------------------
  console.log(chalk.yellow("\n→ Step 3: Seed Pool ($600 LP1 + $400 LP2)"));
  await poolDeposit(creditAsLp1, lp1, usdcMint, poolAta, usdToBase(600));
  await poolDeposit(creditAsLp2, lp2, usdcMint, poolAta, usdToBase(400));

  // -----------------------------------------------------------------------
  // Step 4: Register agent (identity + stake bond)
  // -----------------------------------------------------------------------
  console.log(chalk.yellow("\n→ Step 4: Register agent + $50 stake bond"));
  await registerAgent(
    identityAsOperator,
    agentProfilePda,
    stakeBondPda,
    stakeBondAta,
    usdcMint,
    operator,
    codeHash,
    usdToBase(50),
  );

  // -----------------------------------------------------------------------
  // Step 5: Init agent's Wallet PDA
  // -----------------------------------------------------------------------
  console.log(chalk.yellow("\n→ Step 5: Init agent Wallet PDA + USDC ATA"));
  await initWallet(walletAsAdmin, walletPda, walletAta, usdcMint, codeHash);

  // -----------------------------------------------------------------------
  // Step 6: Init VouchPool + initial voucher stakes $10
  // -----------------------------------------------------------------------
  console.log(chalk.yellow("\n→ Step 6: Init VouchPool + initial voucher stake ($10)"));
  await ensureVouchPool(reputationAsVoucher, vouchPoolPda, vouchPoolAta, usdcMint, codeHash);
  await voucherStake(
    reputationAsVoucher,
    voucher,
    vouchPoolPda,
    vouchPoolAta,
    repStatsPda,
    usdcMint,
    codeHash,
    usdToBase(10),
  );

  // -----------------------------------------------------------------------
  // Step 7: Print summary, write config
  // -----------------------------------------------------------------------
  console.log(chalk.green.bold("\n✓ Bootstrap complete.\n"));

  const poolBalance = await safeBalance(connection, poolAta);
  const walletBalance = await safeBalance(connection, walletAta);
  const stakeBalance = await safeBalance(connection, stakeBondAta);
  const vouchBalance = await safeBalance(connection, vouchPoolAta);

  console.log(chalk.bold("PDAs:"));
  console.log(`  RepStats:      ${repStatsPda.toBase58()}`);
  console.log(`  Pool:          ${poolPda.toBase58()}    balance: $${baseToUsd(poolBalance)}`);
  console.log(`  AgentProfile:  ${agentProfilePda.toBase58()}`);
  console.log(`  StakeBond:     ${stakeBondPda.toBase58()}  balance: $${baseToUsd(stakeBalance)}`);
  console.log(`  Wallet:        ${walletPda.toBase58()}    balance: $${baseToUsd(walletBalance)}`);
  console.log(`  CreditLine:    ${creditLinePda.toBase58()} (not yet drawn)`);
  console.log(`  VouchPool:     ${vouchPoolPda.toBase58()}  balance: $${baseToUsd(vouchBalance)}`);

  console.log(chalk.bold("\nATAs:"));
  console.log(`  Pool USDC:     ${poolAta.toBase58()}`);
  console.log(`  Wallet USDC:   ${walletAta.toBase58()}`);
  console.log(`  StakeBond USDC:${stakeBondAta.toBase58()}`);
  console.log(`  VouchPool USDC:${vouchPoolAta.toBase58()}`);

  console.log(chalk.bold("\nAgent .env values:"));
  console.log(chalk.gray("# Paste these into agent/.env"));
  console.log(`POLIS_CLUSTER=${cfg.cluster}`);
  console.log(`POLIS_RPC_URL=${connection.rpcEndpoint}`);
  console.log(`POLIS_USDC_MINT=${cfg.usdcMint}`);
  console.log(`POLIS_AGENT_CODE_HASH=${codeHash.toString("hex")}`);
  console.log(`POLIS_OPERATOR_PUBKEY=${operator.publicKey.toBase58()}`);
  console.log(`POLIS_AGENT_PROFILE=${agentProfilePda.toBase58()}`);
  console.log(`POLIS_STAKE_BOND=${stakeBondPda.toBase58()}`);
  console.log(`POLIS_WALLET=${walletPda.toBase58()}`);
  console.log(`POLIS_WALLET_ATA=${walletAta.toBase58()}`);
  console.log(`POLIS_CREDIT_LINE=${creditLinePda.toBase58()}`);
  console.log(`POLIS_POOL=${poolPda.toBase58()}`);
  console.log(`POLIS_POOL_ATA=${poolAta.toBase58()}`);
  console.log(`POLIS_VOUCH_POOL=${vouchPoolPda.toBase58()}`);
  console.log(`POLIS_IDENTITY_PROGRAM=${identityPid.toBase58()}`);
  console.log(`POLIS_WALLET_PROGRAM=${walletPid.toBase58()}`);
  console.log(`POLIS_X402_PROGRAM=${x402Pid.toBase58()}`);
  console.log(`POLIS_CREDIT_PROGRAM=${creditPid.toBase58()}`);
  console.log(`POLIS_REPUTATION_PROGRAM=${reputationPid.toBase58()}`);

  saveConfig({
    ...cfg,
    poolPubkey: poolPda.toBase58(),
    poolUsdcAta: poolAta.toBase58(),
    agentCodeHashHex: codeHash.toString("hex"),
    agentOperatorPubkey: operator.publicKey.toBase58(),
    agentProfilePda: agentProfilePda.toBase58(),
    agentStakeBondPda: stakeBondPda.toBase58(),
    agentWalletPda: walletPda.toBase58(),
    agentWalletAta: walletAta.toBase58(),
    agentCreditLinePda: creditLinePda.toBase58(),
    agentVouchPoolPda: vouchPoolPda.toBase58(),
    demoStart: cfg.demoStart || new Date().toISOString(),
  });

  console.log(chalk.green.bold(`\n  → wrote ${process.env.POLIS_CONFIG_PATH || "../.anchor-config.json"}`));
}

// ===========================================================================
// Step helpers — each one prints `[ok]` / `[skip]` and tolerates idempotent
// re-runs by sniffing for existing accounts first.
// ===========================================================================

async function safeBalance(connection: anchor.web3.Connection, ata: PublicKey): Promise<bigint> {
  try {
    const acct = await getAccount(connection, ata, "confirmed");
    return acct.amount;
  } catch {
    return 0n;
  }
}

async function ensureRepStats(
  reputation: anchor.Program,
  repStatsPda: PublicKey,
  creditProgramId: PublicKey,
): Promise<void> {
  const existing = await reputation.provider.connection.getAccountInfo(repStatsPda, "confirmed");
  if (existing) {
    console.log(chalk.gray(`  [skip] RepStats already initialised at ${repStatsPda.toBase58()}`));
    return;
  }
  try {
    const sig = await reputation.methods
      .initRepStats(creditProgramId)
      .accounts({
        repStats: repStatsPda,
        admin: reputation.provider.publicKey!,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .rpc();
    console.log(chalk.green(`  ✓ init_rep_stats ok (sig ${sig.slice(0, 16)}…)`));
  } catch (e) {
    console.log(chalk.red(`  ✗ init_rep_stats failed: ${(e as Error).message}`));
    throw e;
  }
}

async function ensurePool(
  credit: anchor.Program,
  poolPda: PublicKey,
  poolAta: PublicKey,
  usdcMint: PublicKey,
  reputationProgram: PublicKey,
): Promise<void> {
  const existing = await credit.provider.connection.getAccountInfo(poolPda, "confirmed");
  if (existing) {
    console.log(chalk.gray(`  [skip] Pool already initialised at ${poolPda.toBase58()}`));
    return;
  }
  try {
    // 30 days default (in seconds) until a missed-repay counts as default.
    const defaultAfterSeconds = new anchor.BN(30 * 24 * 60 * 60);
    // usdc_mint + reputation_program are plain inputs Anchor cannot resolve
    // from the IDL; supplying usdc_mint also lets it derive the pool ATA.
    const sig = await credit.methods
      .initPool(defaultAfterSeconds)
      .accounts({ usdcMint, reputationProgram })
      .rpc();
    console.log(chalk.green(`  ✓ init_pool ok (sig ${sig.slice(0, 16)}…)`));
    console.log(chalk.gray(`    Pool:     ${poolPda.toBase58()}`));
    console.log(chalk.gray(`    Pool ATA: ${poolAta.toBase58()}`));
  } catch (e) {
    console.log(chalk.red(`  ✗ init_pool failed: ${(e as Error).message}`));
    throw e;
  }
}

async function poolDeposit(
  credit: anchor.Program,
  lp: anchor.web3.Keypair,
  usdcMint: PublicKey,
  poolAta: PublicKey,
  amount: bigint,
): Promise<void> {
  const lpAta = await getAssociatedTokenAddress(usdcMint, lp.publicKey);
  try {
    await getAccount(credit.provider.connection, lpAta);
  } catch {
    console.log(chalk.red(`  ✗ LP ${lp.publicKey.toBase58()} has no USDC ATA — fund it on devnet first.`));
    throw new Error("LP USDC ATA missing");
  }

  try {
    // pool_usdc uses token::authority (not associated_token::), so Anchor
    // cannot derive it — supply the pool's ATA explicitly.
    const sig = await credit.methods
      .deposit(new anchor.BN(amount.toString()))
      .accounts({
        lp: lp.publicKey,
        lpUsdc: lpAta,
        poolUsdc: poolAta,
        usdcMint,
      })
      .rpc();
    console.log(
      chalk.green(
        `  ✓ deposit $${baseToUsd(amount)} from ${lp.publicKey.toBase58().slice(0, 8)}… (sig ${sig.slice(0, 16)}…)`,
      ),
    );
  } catch (e) {
    console.log(chalk.red(`  ✗ deposit failed: ${(e as Error).message}`));
    throw e;
  }
}

async function registerAgent(
  identity: anchor.Program,
  agentProfilePda: PublicKey,
  _stakeBondPda: PublicKey,
  _stakeBondAta: PublicKey,
  usdcMint: PublicKey,
  operator: anchor.web3.Keypair,
  codeHash: Buffer,
  bondAmount: bigint,
): Promise<void> {
  const existing = await identity.provider.connection.getAccountInfo(agentProfilePda, "confirmed");
  if (existing) {
    console.log(chalk.gray(`  [skip] AgentProfile already exists at ${agentProfilePda.toBase58()}`));
    return;
  }

  const operatorUsdc = await getAssociatedTokenAddress(usdcMint, operator.publicKey);
  try {
    // Anchor pulls the PDA addresses for `agent_profile`, `stake_bond`, and
    // `stake_bond_usdc` from the IDL's `pda.seeds` block — we only have to
    // supply the dynamic accounts. `code_hash` is sourced from the arg.
    const sig = await identity.methods
      .registerAgent([...codeHash], operator.publicKey, new anchor.BN(bondAmount.toString()))
      .accounts({
        operator: operator.publicKey,
        usdcMint,
        operatorUsdc,
      })
      .rpc();
    console.log(chalk.green(`  ✓ register_agent ok (sig ${sig.slice(0, 16)}…), bond $${baseToUsd(bondAmount)}`));
  } catch (e) {
    console.log(chalk.red(`  ✗ register_agent failed: ${(e as Error).message}`));
    throw e;
  }
}

async function initWallet(
  wallet: anchor.Program,
  walletPda: PublicKey,
  _walletAta: PublicKey,
  usdcMint: PublicKey,
  codeHash: Buffer,
): Promise<void> {
  const existing = await wallet.provider.connection.getAccountInfo(walletPda, "confirmed");
  if (existing) {
    console.log(chalk.gray(`  [skip] Wallet already exists at ${walletPda.toBase58()}`));
    return;
  }
  try {
    const sig = await wallet.methods
      .initWallet([...codeHash])
      .accounts({
        usdcMint,
        admin: wallet.provider.publicKey!,
      })
      .rpc();
    console.log(chalk.green(`  ✓ init_wallet ok (sig ${sig.slice(0, 16)}…)`));
  } catch (e) {
    console.log(chalk.red(`  ✗ init_wallet failed: ${(e as Error).message}`));
    throw e;
  }
}

async function ensureVouchPool(
  reputation: anchor.Program,
  vouchPoolPda: PublicKey,
  _vouchPoolAta: PublicKey,
  usdcMint: PublicKey,
  codeHash: Buffer,
): Promise<void> {
  const existing = await reputation.provider.connection.getAccountInfo(vouchPoolPda, "confirmed");
  if (existing) {
    console.log(chalk.gray(`  [skip] VouchPool already exists at ${vouchPoolPda.toBase58()}`));
    return;
  }
  try {
    const sig = await reputation.methods
      .initVouchPool([...codeHash])
      .accounts({
        usdcMint,
        payer: reputation.provider.publicKey!,
      })
      .rpc();
    console.log(chalk.green(`  ✓ init_vouch_pool ok (sig ${sig.slice(0, 16)}…)`));
  } catch (e) {
    console.log(chalk.red(`  ✗ init_vouch_pool failed: ${(e as Error).message}`));
    throw e;
  }
}

async function voucherStake(
  reputation: anchor.Program,
  voucher: anchor.web3.Keypair,
  _vouchPoolPda: PublicKey,
  vouchPoolAta: PublicKey,
  _repStatsPda: PublicKey,
  usdcMint: PublicKey,
  codeHash: Buffer,
  amount: bigint,
): Promise<void> {
  const stakerUsdc = await getAssociatedTokenAddress(usdcMint, voucher.publicKey);
  try {
    // vouch_pool_usdc uses token::authority (not associated_token::), so it
    // must be supplied explicitly.
    const sig = await reputation.methods
      .vouch([...codeHash], new anchor.BN(amount.toString()))
      .accounts({
        staker: voucher.publicKey,
        stakerUsdc,
        vouchPoolUsdc: vouchPoolAta,
        usdcMint,
      })
      .rpc();
    console.log(chalk.green(`  ✓ vouch $${baseToUsd(amount)} (sig ${sig.slice(0, 16)}…)`));
  } catch (e) {
    console.log(chalk.red(`  ✗ vouch failed: ${(e as Error).message}`));
    throw e;
  }
}

main().catch((e: Error) => {
  console.error(chalk.red.bold(`\n✗ bootstrap failed: ${e.message}\n`));
  console.error(e.stack);
  process.exit(1);
});
