#!/usr/bin/env bash
# polis :: localnet setup (reproducible)
# Deploys the 5 programs to a running solana-test-validator, creates a mock
# USDC mint, funds the demo keypairs, and writes ../.anchor-config.json.
# Prereqs: a validator running with --enable-rpc-transaction-history at :8899,
# and target/deploy/*.so already built (anchor build).
set -euo pipefail

export PATH="$HOME/.cargo/bin:$HOME/.local/share/solana/install/active_release/bin:$HOME/.nvm/versions/node/v24.10.0/bin:$PATH"
cd "$(dirname "$0")/.."
ROOT=$(pwd)
K=/tmp/polis-keys
ADMIN=~/.config/solana/id.json

solana config set --url http://127.0.0.1:8899 >/dev/null
solana airdrop 100 >/dev/null 2>&1 || true
solana airdrop 100 >/dev/null 2>&1 || true

echo "→ deploy programs"
anchor deploy --provider.cluster localnet >/dev/null 2>&1
echo "  done"

echo "→ create mock USDC mint (6 decimals, authority=admin)"
MINT=$(spl-token create-token --decimals 6 2>/dev/null | grep -oE 'Creating token [1-9A-HJ-NP-Za-km-z]+' | awk '{print $3}')
echo "  USDC mint: $MINT"

echo "→ fund keypairs (SOL + USDC)"
for entry in operator:100 lp1:600 lp2:400 voucher1:50 customer1:50 customer2:50 dispenser:10000; do
  name=${entry%%:*}; amt=${entry##*:}
  owner=$(solana address -k $K/$name.json)
  solana airdrop 10 "$owner" >/dev/null 2>&1 || true
  ata=$(spl-token create-account "$MINT" --owner "$owner" --fee-payer "$ADMIN" 2>/dev/null | grep -oE '[1-9A-HJ-NP-Za-km-z]{32,44}' | head -1)
  spl-token mint "$MINT" "$amt" "$ata" >/dev/null 2>&1
  echo "  $name +\$$amt"
done

echo "→ write .anchor-config.json"
cat > "$ROOT/.anchor-config.json" <<EOF
{
  "cluster": "localnet",
  "rpcUrl": "http://127.0.0.1:8899",
  "usdcMint": "$MINT",
  "adminPubkey": "$(solana address -k $ADMIN)",
  "programs": {
    "identity": "wSWCbEtpjD9fpVR5XSrK9YhEtJKNwPefkwHTN2v3SbG",
    "wallet": "7Jz35XjSeWfA28t8oxzbTyfUTuvgiM4YYnkygpktALBY",
    "x402": "3qWKUNmhHw7qraor4oYuyzchYwqQFiYDUbxMKoK5NKt8",
    "credit": "7jCmeDoXpMFRTsWYuCeKvx1TiHF9pjT4iyTKLQaPaHDQ",
    "reputation": "AskjrF52LFEJaQc7d5Z46AMEDnKRoURMFBqmcacvAEoP"
  }
}
EOF

# Keep agent/.env's USDC mint in sync.
if [ -f "$ROOT/agent/.env" ]; then
  sed -i '' "s/^SOLANA_USDC_MINT=.*/SOLANA_USDC_MINT=$MINT/" "$ROOT/agent/.env"
fi

echo "→ done. USDC mint = $MINT"
