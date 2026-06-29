/**
 * polis :: simulate-customers.ts
 *
 * Long-running script. Generates Poisson-distributed customer traffic against
 * the reference agent's `/summarize` endpoint:
 *
 *   1. Loads N customer keypairs (devnet) — paths from $CUSTOMER_KEYPAIR_PATHS
 *      (colon-separated). Each must be pre-funded with devnet USDC.
 *   2. Reads agent URL + code-hash from CLI args (or .anchor-config.json).
 *   3. Generates inter-arrival times from an exponential distribution with
 *      mean = 86_400 / RATE seconds (default RATE=5 calls/day). The `--speedup`
 *      flag divides the inter-arrival time (e.g. --speedup=10x → 50 calls/day).
 *   4. For each call:
 *        a. Generate a 32-byte hex payment_id (random).
 *        b. Submit `pay_for_service(agent, amount, memo=payment_id)` against
 *           x402 from a random customer keypair. The on-chain memo binds the
 *           settlement to the off-chain request (paper open-question #2).
 *        c. After confirmation, POST `{ text, payment_id }` to
 *           `<agent_url>/summarize`.
 *        d. Append a row to scripts/logs/customer-traffic.csv with timestamp,
 *           customer, payment_id, amount, http_status, latency_ms, earnings.
 *
 * CLI usage:
 *   yarn simulate-customers --agent-url=http://127.0.0.1:8000 [--speedup=10] [--rate=5]
 *
 * Env:
 *   CUSTOMER_KEYPAIR_PATHS=/path/to/c1.json:/path/to/c2.json:/path/to/c3.json
 *   POLIS_PRICE_USD=0.10   # default per-call price
 */

import * as fs from "fs";
import * as path from "path";
import * as crypto from "crypto";
import {
  PublicKey,
  Connection,
  Keypair,
} from "@solana/web3.js";
import {
  getAssociatedTokenAddress,
} from "@solana/spl-token";
import * as anchor from "@coral-xyz/anchor";
import * as http from "http";
import * as https from "https";
import { URL } from "url";
import chalk from "chalk";
import {
  loadConfig,
  loadKeypair,
  makeConnection,
  parseArgs,
  codeHashFromHex,
  logsDir,
  usdToBase,
  baseToUsd,
  setupProgram,
} from "./_common";

interface CallRecord {
  iso: string;
  customer: string;
  payment_id: string;
  amount_usd: number;
  pay_sig: string;
  http_status: number;
  latency_ms: number;
  ok: boolean;
  error: string;
}

const SAMPLE_PARAGRAPHS: string[] = [
  "The discovery of asynchronous programming was a watershed moment for browsers and servers alike, allowing single-threaded runtimes to interleave I/O with computation gracefully.",
  "An autonomous agent that cannot pay for its own compute will eventually grind to a halt, in exactly the same way a human worker without a bank account does, and for the same reason.",
  "Modern reputation systems frequently conflate the wallet — a transferable artifact — with the actor that controls it, producing scores that say more about the address's age than the agent's behaviour.",
  "Solana's program-derived addresses give us deterministic, code-only authority: a structural property that no possession of a private key can replicate, and one that defines polis's containment primitive.",
  "Stake-bonded jury arbitration borrows from classical Athenian dikastēria the insight that procedural legitimacy can emerge from random sampling under economic alignment.",
  "Markets settle in seconds; institutions settle in centuries. The interesting bet is whether the gap between the two can be made navigable by code.",
];

function exponentialSample(rateLambda: number): number {
  // X = -ln(U) / λ, where U ∈ (0, 1].
  const u = Math.max(Number.EPSILON, Math.random());
  return -Math.log(u) / rateLambda;
}

function randomCustomer(customers: Keypair[]): Keypair {
  return customers[Math.floor(Math.random() * customers.length)];
}

function randomParagraph(): string {
  return SAMPLE_PARAGRAPHS[Math.floor(Math.random() * SAMPLE_PARAGRAPHS.length)];
}

function randomPaymentId(): string {
  return crypto.randomBytes(32).toString("hex");
}

interface HttpResponse {
  status: number;
  body: string;
  latencyMs: number;
}

function httpPost(agentUrl: string, body: object): Promise<HttpResponse> {
  return new Promise((resolve, reject) => {
    const t0 = Date.now();
    const url = new URL(agentUrl + "/summarize");
    const payload = JSON.stringify(body);
    const opts: http.RequestOptions = {
      method: "POST",
      hostname: url.hostname,
      port: url.port || (url.protocol === "https:" ? 443 : 80),
      path: url.pathname + url.search,
      headers: {
        "Content-Type": "application/json",
        "Content-Length": Buffer.byteLength(payload).toString(),
      },
      timeout: 30_000,
    };
    const lib = url.protocol === "https:" ? https : http;
    const req = lib.request(opts, (res) => {
      const chunks: Buffer[] = [];
      res.on("data", (c) => chunks.push(Buffer.from(c)));
      res.on("end", () => {
        resolve({
          status: res.statusCode || 0,
          body: Buffer.concat(chunks).toString("utf-8"),
          latencyMs: Date.now() - t0,
        });
      });
    });
    req.on("error", reject);
    req.on("timeout", () => {
      req.destroy(new Error("HTTP timeout"));
    });
    req.write(payload);
    req.end();
  });
}

/**
 * Call x402.pay_for_service(agent_code_hash, amount, memo=payment_id).
 *
 * The on-chain handler re-derives the agent's Wallet PDA from `agent_code_hash`
 * and the configured wallet program, then asserts the supplied `agent_wallet_ata`
 * is owned by that PDA — so the customer cannot redirect funds.
 */
async function payForService(
  connection: Connection,
  customer: Keypair,
  usdcMint: PublicKey,
  walletAta: PublicKey,
  amount: bigint,
  paymentId: string,
  agentCodeHash: Buffer,
  walletProgramId: PublicKey,
): Promise<string> {
  const x402 = setupProgram("x402", connection, new anchor.Wallet(customer));
  const customerAta = await getAssociatedTokenAddress(usdcMint, customer.publicKey);
  return await x402.methods
    .payForService(
      [...agentCodeHash],
      new anchor.BN(amount.toString()),
      paymentId,
    )
    .accounts({
      payer: customer.publicKey,
      usdcMint,
      payerAta: customerAta,
      agentWalletAta: walletAta,
      walletProgram: walletProgramId,
    })
    .rpc();
}

async function main(): Promise<void> {
  const { flags } = parseArgs();
  const cfg = loadConfig();
  const connection = makeConnection(cfg);

  const agentUrl = (flags["agent-url"] as string) || "http://127.0.0.1:8000";
  const speedupRaw = (flags["speedup"] as string) || "1";
  const speedup = parseFloat(speedupRaw.toString().replace("x", "")) || 1;
  const rate = parseFloat((flags["rate"] as string) || "5"); // calls/day
  const codeHashHex = (flags["code-hash"] as string) || cfg.agentCodeHashHex;
  if (!codeHashHex) {
    throw new Error("code-hash unavailable: pass --code-hash=<hex> or run bootstrap first.");
  }
  const codeHashBuf = codeHashFromHex(codeHashHex);

  if (!cfg.agentWalletAta) {
    throw new Error("agentWalletAta missing from .anchor-config.json — run bootstrap first.");
  }
  const walletAta = new PublicKey(cfg.agentWalletAta);
  const usdcMint = new PublicKey(cfg.usdcMint);
  const walletProgramId = new PublicKey(cfg.programs.wallet);

  const kpPathsEnv = process.env.CUSTOMER_KEYPAIR_PATHS;
  if (!kpPathsEnv) {
    console.error(chalk.red("Missing $CUSTOMER_KEYPAIR_PATHS (colon-separated)."));
    process.exit(1);
  }
  const customerPaths = kpPathsEnv.split(":").filter(Boolean);
  const customers: Keypair[] = customerPaths.map(loadKeypair);

  const priceUsd = parseFloat(process.env.POLIS_PRICE_USD || "0.10");
  const priceBase = usdToBase(priceUsd);

  // Mean inter-arrival in seconds, in real wall-clock time. We multiply by
  // (1 / speedup) so --speedup=10 means 10× faster (50 calls/day instead of 5).
  const meanInterArrivalSec = 86_400 / rate / speedup;
  const lambda = 1 / meanInterArrivalSec;

  console.log(chalk.cyan.bold("polis :: customer simulator"));
  console.log(chalk.gray(`  agent URL:           ${agentUrl}`));
  console.log(chalk.gray(`  agent wallet ATA:    ${walletAta.toBase58()}`));
  console.log(chalk.gray(`  customers:           ${customers.length}`));
  console.log(chalk.gray(`  rate:                ${rate} calls/day`));
  console.log(chalk.gray(`  speedup:             ${speedup}x`));
  console.log(chalk.gray(`  mean inter-arrival:  ${meanInterArrivalSec.toFixed(1)}s`));
  console.log(chalk.gray(`  price:               $${priceUsd}\n`));

  const csvPath = path.join(logsDir(), "customer-traffic.csv");
  if (!fs.existsSync(csvPath)) {
    fs.writeFileSync(
      csvPath,
      "iso,customer,payment_id,amount_usd,pay_sig,http_status,latency_ms,ok,error\n",
    );
  }

  // Graceful shutdown.
  let shuttingDown = false;
  const shutdown = () => {
    if (shuttingDown) return;
    shuttingDown = true;
    console.log(chalk.yellow("\n  ↩ shutting down simulator"));
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);

  let callCount = 0;
  let totalEarnings = 0;
  // Main loop.
  while (!shuttingDown) {
    const waitSec = exponentialSample(lambda);
    await sleep(waitSec * 1000);
    if (shuttingDown) break;

    const customer = randomCustomer(customers);
    const paymentId = randomPaymentId();
    const paragraph = randomParagraph();
    const t0 = new Date().toISOString();
    let payOk = true;
    let paySig = "";
    let httpStatus = 0;
    let latencyMs = 0;
    let errorMsg = "";

    try {
      paySig = await payForService(
        connection,
        customer,
        usdcMint,
        walletAta,
        priceBase,
        paymentId,
        codeHashBuf,
        walletProgramId,
      );
    } catch (e) {
      payOk = false;
      errorMsg = `pay: ${(e as Error).message}`;
    }

    if (payOk) {
      try {
        const res = await httpPost(agentUrl, {
          text: paragraph,
          payment_id: paymentId,
        });
        httpStatus = res.status;
        latencyMs = res.latencyMs;
        if (res.status !== 200) {
          errorMsg = `http: status=${res.status} body=${res.body.slice(0, 200)}`;
        } else {
          totalEarnings += priceUsd;
        }
      } catch (e) {
        errorMsg = `http: ${(e as Error).message}`;
      }
    }

    const record: CallRecord = {
      iso: t0,
      customer: customer.publicKey.toBase58(),
      payment_id: paymentId,
      amount_usd: priceUsd,
      pay_sig: paySig,
      http_status: httpStatus,
      latency_ms: latencyMs,
      ok: payOk && httpStatus === 200,
      error: errorMsg,
    };
    appendCsv(csvPath, record);
    callCount += 1;

    const colour = record.ok ? chalk.green : chalk.red;
    console.log(
      colour(
        `[${t0}] ${record.ok ? "✓" : "✗"} ${customer.publicKey.toBase58().slice(0, 8)}… ` +
          `$${priceUsd} ${record.ok ? `${latencyMs}ms` : `(${errorMsg.slice(0, 60)})`}`,
      ),
    );

    // Periodic summary.
    if (callCount % 10 === 0) {
      console.log(
        chalk.cyan(`  → ${callCount} calls, $${totalEarnings.toFixed(2)} earned (gross, agent-side)`),
      );
      void baseToUsd; // ensure helper imported
    }
  }
}

function appendCsv(filepath: string, r: CallRecord): void {
  const line = [
    r.iso,
    r.customer,
    r.payment_id,
    r.amount_usd.toString(),
    r.pay_sig,
    r.http_status.toString(),
    r.latency_ms.toString(),
    r.ok ? "true" : "false",
    `"${r.error.replace(/"/g, "''")}"`,
  ].join(",");
  fs.appendFileSync(filepath, line + "\n");
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

main().catch((e: Error) => {
  console.error(chalk.red.bold(`✗ simulator failed: ${e.message}`));
  console.error(e.stack);
  process.exit(1);
});
