/**
 * polis :: shared script utilities
 *
 * Centralises:
 *   - Loading the deploy-time `.anchor-config.json` (populated by `bootstrap.ts`
 *     after the first deploy). Holds the program IDs for the five primitive
 *     programs, the USDC mint, and the admin key reference.
 *   - PDA derivation helpers keyed by the agent's code-hash.
 *   - Keypair loading from filesystem JSON byte arrays (Solana CLI format).
 *   - USDC denomination helpers.
 *
 * Why a separate file? Because we cannot derive PDAs without the program IDs,
 * and the program IDs are not known until after the Anchor deploy. The
 * `.anchor-config.json` is the rendezvous point: `bootstrap.ts` writes it the
 * first time a deploy lands; every other script reads it.
 */

import * as fs from "fs";
import * as path from "path";
import {
  Connection,
  Keypair,
  PublicKey,
  clusterApiUrl,
  Commitment,
} from "@solana/web3.js";
import * as anchor from "@coral-xyz/anchor";
import * as dotenv from "dotenv";

dotenv.config();

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** USDC has six decimals on both mainnet and devnet. */
export const USDC_DECIMALS = 6;

/** Convert a USD-denominated number (e.g. 50) into base units (50_000_000). */
export function usdToBase(usd: number): bigint {
  if (!Number.isFinite(usd) || usd < 0) {
    throw new Error(`usdToBase: invalid amount ${usd}`);
  }
  // Use bigint arithmetic to avoid float drift on cents.
  const cents = Math.round(usd * 10 ** USDC_DECIMALS);
  return BigInt(cents);
}

/** Inverse of usdToBase, for display. */
export function baseToUsd(base: bigint | number): number {
  const b = typeof base === "bigint" ? Number(base) : base;
  return b / 10 ** USDC_DECIMALS;
}

// ---------------------------------------------------------------------------
// Anchor deploy config
// ---------------------------------------------------------------------------

export interface AnchorConfig {
  cluster: "devnet" | "mainnet-beta" | "localnet";
  rpcUrl: string;
  usdcMint: string;
  adminPubkey: string;
  programs: {
    identity: string;
    wallet: string;
    x402: string;
    credit: string;
    reputation: string;
  };
  // Pool PDA cached for downstream scripts; written at bootstrap time.
  poolPubkey?: string;
  poolUsdcAta?: string;
  // The single demo agent's code-hash, hex-encoded. Cached at bootstrap.
  agentCodeHashHex?: string;
  // The agent operator pubkey + the wallet/credit/vouch PDAs for that agent.
  agentOperatorPubkey?: string;
  agentProfilePda?: string;
  agentStakeBondPda?: string;
  agentWalletPda?: string;
  agentWalletAta?: string;
  agentCreditLinePda?: string;
  agentVouchPoolPda?: string;
  // Customer keypair paths used by simulate-customers.
  customerKeypairPaths?: string[];
  // Demo run start (ISO timestamp) — written at bootstrap.
  demoStart?: string;
}

const DEFAULT_CONFIG_PATH = path.resolve(__dirname, "..", ".anchor-config.json");

export function configPath(): string {
  return process.env.POLIS_CONFIG_PATH || DEFAULT_CONFIG_PATH;
}

export function loadConfig(): AnchorConfig {
  const p = configPath();
  if (!fs.existsSync(p)) {
    throw new Error(
      `polis :: .anchor-config.json not found at ${p}. ` +
        `Run \`yarn bootstrap\` (or \`anchor deploy\` then bootstrap) first.`,
    );
  }
  const raw = fs.readFileSync(p, "utf-8");
  return JSON.parse(raw) as AnchorConfig;
}

export function saveConfig(cfg: AnchorConfig): void {
  const p = configPath();
  fs.writeFileSync(p, JSON.stringify(cfg, null, 2) + "\n", "utf-8");
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

export function makeConnection(cfg: AnchorConfig, commitment: Commitment = "confirmed"): Connection {
  // Allow env override of the RPC URL for rate-limit dodging.
  const url =
    process.env.SOLANA_RPC_URL ||
    cfg.rpcUrl ||
    clusterApiUrl(cfg.cluster === "localnet" ? "devnet" : (cfg.cluster as "devnet" | "mainnet-beta"));
  return new Connection(url, { commitment });
}

// ---------------------------------------------------------------------------
// Keypair loading
// ---------------------------------------------------------------------------

/**
 * Load a Solana CLI-format JSON keypair from disk.
 * Throws with a clear message on bad input.
 */
export function loadKeypair(filepath: string): Keypair {
  const resolved = filepath.startsWith("~")
    ? path.join(process.env.HOME || "", filepath.slice(1))
    : path.resolve(filepath);
  if (!fs.existsSync(resolved)) {
    throw new Error(`Keypair file not found: ${resolved}`);
  }
  const bytes = JSON.parse(fs.readFileSync(resolved, "utf-8"));
  if (!Array.isArray(bytes) || bytes.length !== 64) {
    throw new Error(`Keypair file ${resolved} is not a 64-byte array.`);
  }
  return Keypair.fromSecretKey(Uint8Array.from(bytes));
}

// ---------------------------------------------------------------------------
// PDA derivation
// ---------------------------------------------------------------------------

/** Encode a 32-byte hex string (no 0x prefix) to a Buffer. */
export function codeHashFromHex(hex: string): Buffer {
  const clean = hex.startsWith("0x") ? hex.slice(2) : hex;
  if (clean.length !== 64) {
    throw new Error(`code_hash hex must be 32 bytes (64 chars), got ${clean.length}`);
  }
  return Buffer.from(clean, "hex");
}

/** Read a code-hash from a `code-hash.txt` file. */
export function readCodeHashFile(filepath: string): Buffer {
  const resolved = path.resolve(filepath);
  if (!fs.existsSync(resolved)) {
    throw new Error(`Code-hash file not found: ${resolved}`);
  }
  const raw = fs.readFileSync(resolved, "utf-8").trim();
  return codeHashFromHex(raw);
}

export function deriveAgentProfilePda(programId: PublicKey, codeHash: Buffer): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("agent"), codeHash],
    programId,
  );
}

export function deriveStakeBondPda(programId: PublicKey, codeHash: Buffer): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("stake"), codeHash],
    programId,
  );
}

export function deriveWalletPda(programId: PublicKey, codeHash: Buffer): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("wallet"), codeHash],
    programId,
  );
}

export function deriveCreditLinePda(programId: PublicKey, codeHash: Buffer): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("credit"), codeHash],
    programId,
  );
}

export function derivePoolPda(programId: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync([Buffer.from("pool")], programId);
}

export function derivePolisConfigPda(programId: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync([Buffer.from("polis_config")], programId);
}

export function deriveVouchPoolPda(programId: PublicKey, codeHash: Buffer): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("vouch"), codeHash],
    programId,
  );
}

// ---------------------------------------------------------------------------
// CLI arg parsing (lightweight; no yargs)
// ---------------------------------------------------------------------------

/**
 * Parse --key=value and --flag from process.argv. Positional args are returned
 * in `_`. Lossy by design; we only need a handful of flags.
 */
export function parseArgs(argv: string[] = process.argv.slice(2)): {
  flags: Record<string, string | boolean>;
  positionals: string[];
} {
  const flags: Record<string, string | boolean> = {};
  const positionals: string[] = [];
  for (const arg of argv) {
    if (arg.startsWith("--")) {
      const eq = arg.indexOf("=");
      if (eq === -1) {
        flags[arg.slice(2)] = true;
      } else {
        flags[arg.slice(2, eq)] = arg.slice(eq + 1);
      }
    } else {
      positionals.push(arg);
    }
  }
  return { flags, positionals };
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

export function ensureDir(dir: string): void {
  if (!fs.existsSync(dir)) {
    fs.mkdirSync(dir, { recursive: true });
  }
}

export function logsDir(): string {
  const d = path.resolve(__dirname, "logs");
  ensureDir(d);
  return d;
}

// ---------------------------------------------------------------------------
// IDL loading + Anchor Program construction
// ---------------------------------------------------------------------------

/**
 * Resolve the directory holding the JSON IDLs. Anchor writes these to
 * `target/idl/<name>.json` during `anchor build`. Override with $IDL_PATH if
 * the build artefacts live elsewhere.
 */
export function idlDir(): string {
  if (process.env.IDL_PATH) {
    return path.resolve(process.env.IDL_PATH);
  }
  return path.resolve(__dirname, "..", "target", "idl");
}

/**
 * Read and parse a JSON IDL by bare name. Throws with a helpful message if the
 * file is missing — usually that means `anchor build` hasn't been run.
 */
export function loadIdl(name: string): anchor.Idl {
  const file = path.join(idlDir(), `${name}.json`);
  if (!fs.existsSync(file)) {
    throw new Error(
      `IDL not found at ${file}. Run \`anchor build\` (or set $IDL_PATH) first.`,
    );
  }
  const raw = fs.readFileSync(file, "utf-8");
  return JSON.parse(raw) as anchor.Idl;
}

/**
 * Build an `anchor.Program` instance wired to the supplied connection + wallet.
 *
 * The program ID is taken from the IDL itself (Anchor 0.30 embeds it under
 * `address`), so the deploy-time `.anchor-config.json` IDs and the IDLs must
 * agree — bootstrap.ts asserts this.
 */
export function setupProgram(
  idlName: string,
  connection: Connection,
  wallet: anchor.Wallet,
): anchor.Program {
  const idl = loadIdl(idlName);
  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
    preflightCommitment: "confirmed",
  });
  // Anchor 0.30 reads programId from `idl.address`; we don't need to pass it
  // explicitly. We do pin the provider so calls flow through the chosen RPC.
  return new anchor.Program(idl, provider);
}
