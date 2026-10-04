#!/usr/bin/env bash
#
# The whole owner-to-agent flow against a live network, in one command that cleans up after itself.
#
#   RILL_E2E_OWNER=0x… RILL_E2E_AGENT=0x… scripts/e2e.sh [testnet|mainnet] [swap|stake|deepbook|workflow|recovery]
#
# Builds the server and the signer, starts a throwaway server on a free loopback port with its own
# stores, runs the scenarios in `bins/rill/tests/e2e_live.rs` against it one after another (they
# share the owner's key, so never in parallel), and stops the server whatever happened. Name a
# scenario to run only that one.
# The owner and the agent are both keys in the local Sui keystore, so no step waits on a wallet
# popup; the test revokes the wallet it funded even when a step fails.
#
# RILL_E2E_WALLET_BIN runs the agent side on another signer binary, such as the release the plugin
# launcher downloaded, instead of the one just built.
#
# Mainnet spends real SUI and needs RILL_E2E_ALLOW_MAINNET=1: the swap turns 0.005 SUI into USDC, the
# stake turns 1 SUI into haSUI held by the agent, and the DeepBook ask rests above the market until
# the owner cancels it and withdraws its 1.1 SUI. Every wallet's remainder comes back on revoke.
# Receipts land in the run directory printed at the end, one e2e-<scenario>-receipts.json each.
set -euo pipefail

network="${1:-testnet}"
scenario="${2:-}"
case "$network" in
  testnet | mainnet) ;;
  *) echo "usage: scripts/e2e.sh [testnet|mainnet] [swap|stake|deepbook|workflow|recovery]" >&2; exit 2 ;;
esac
case "$scenario" in
  "" | swap | stake | deepbook | workflow | recovery) ;;
  *) echo "unknown scenario: $scenario (swap, stake, deepbook, workflow or recovery)" >&2; exit 2 ;;
esac
# Keep the default smoke allocation unchanged: the combined run and recovery are explicit modes.
extra_args=(--skip recovery_)
if [ -z "$scenario" ]; then
  extra_args+=(--skip workflow_)
elif [ "$scenario" = recovery ]; then
  extra_args=(--skip workflow_)
  : "${RILL_E2E_RECOVERY_RECEIPT:?set RILL_E2E_RECOVERY_RECEIPT to the existing public workflow receipt}"
fi
: "${RILL_E2E_OWNER:?set RILL_E2E_OWNER to the owner address in the local Sui keystore}"
: "${RILL_E2E_AGENT:?set RILL_E2E_AGENT to the agent address in the local Sui keystore}"
if [ "$network" = mainnet ] && [ "${RILL_E2E_ALLOW_MAINNET:-}" != 1 ]; then
  echo "mainnet spends real SUI: set RILL_E2E_ALLOW_MAINNET=1 to mean it" >&2
  exit 2
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
run_dir="$(mktemp -d "${TMPDIR:-/tmp}/rill-e2e.XXXXXX")"
chmod 700 "$run_dir"

cargo build --locked -q -p rill-server -p rill --bins

port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
api="http://127.0.0.1:$port"
secret="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"

env -i PATH="$PATH" HOME="$HOME" \
  SUI_NETWORK="$network" PORT="$port" BIND_ADDRESS=127.0.0.1 \
  PUBLIC_BASE_URL="$api" RILL_CONSENT_URL="$api/authorize" \
  RILL_OAUTH_SECRET="$secret" \
  SKILLS_STORE_PATH="$run_dir/skills.json" OAUTH_STORE_PATH="$run_dir/oauth.json" \
  GRANTS_STORE_PATH="$run_dir/grants.json" \
  ./target/debug/rill-server >"$run_dir/server.log" 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
  curl -fsS "$api/health" >/dev/null 2>&1 && break
  if ! kill -0 "$server" 2>/dev/null; then
    echo "the server exited; its log:" >&2
    cat "$run_dir/server.log" >&2
    exit 1
  fi
  sleep 0.2
done

status=0
RILL_E2E_NETWORK="$network" RILL_E2E_API="$api" RILL_E2E_DIR="$run_dir" \
  RILL_E2E_WALLET_BIN="${RILL_E2E_WALLET_BIN:-$root/target/debug/rill-wallet}" \
  cargo test --locked -q -p rill --test e2e_live -- --ignored --nocapture --test-threads=1 \
  ${scenario:+"${scenario}_"} "${extra_args[@]}" || status=$?

echo
echo "run directory: $run_dir (server.log, e2e-*-receipts.json)"
exit "$status"
