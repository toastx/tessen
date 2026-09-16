#!/usr/bin/env bash
# Creates/funds the devnet wallet and the two mock mints (mSPACEX, mUSDC).
# Idempotent: re-running tops up SOL and re-mints supply.
set -e
cd "$(dirname "$0")/.."
U="--url devnet"
K="keys/admin.json"
mkdir -p keys

[ -f $K ] || solana-keygen new --no-bip39-passphrase -s -o $K
ADMIN=$(solana address -k $K)
echo "admin: $ADMIN"

bal() { solana balance "$1" $U | cut -d' ' -f1; }
low() { awk "BEGIN{exit !($(bal $ADMIN) < $1)}"; }

# the public faucet rate-limits hard; fall back to the local default wallet
if low 1; then
  solana airdrop 2 $ADMIN $U || true
fi
if low 1 && [ -f ~/.config/solana/id.json ]; then
  solana transfer $ADMIN 1 $U --keypair ~/.config/solana/id.json --allow-unfunded-recipient
fi
echo "balance: $(bal $ADMIN) SOL"

mint() {
  local kp=keys/$1-mint.json
  [ -f $kp ] || solana-keygen new --no-bip39-passphrase -s -o $kp >/dev/null
  local addr=$(solana address -k $kp)
  if ! spl-token display $addr $U >/dev/null 2>&1; then
    spl-token create-token $kp --decimals 6 $U --fee-payer $K --mint-authority $K >/dev/null
    spl-token create-account $addr $U --fee-payer $K --owner $ADMIN >/dev/null
  fi
  local ata=$(spl-token address --token $addr --owner $ADMIN --verbose $U | awk '/Associated/{print $NF}')
  spl-token mint $addr 1000000 $ata $U --fee-payer $K --mint-authority $K >/dev/null
  echo "$addr"
}

SPACEX=$(mint mspacex)
USDC=$(mint musdc)

cat > devnet.json <<EOF
{
  "cluster": "devnet",
  "admin": "$ADMIN",
  "mSPACEX": "$SPACEX",
  "mUSDC": "$USDC",
  "realSpacexMainnet": "PreANxuXjsy2pvisWWMNB6YaJNzr7681wJJr2rHsfTh",
  "programId": "86kDc93MAkfLm3JNi43KYcFPiKZ765tYHPTekxmp1Ukb"
}
EOF
cat devnet.json
spl-token accounts $U --owner $ADMIN
