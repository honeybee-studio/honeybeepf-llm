#!/usr/bin/env bash
set -euo pipefail

# =========================
# OCI Bastion Port Forward Script
# =========================

CONFIG_FILE="$HOME/.oci/config"
SSH_PUB_KEY="$HOME/.ssh/id_rsa.pub"
SSH_PRIVATE_KEY="$HOME/.ssh/id_rsa"
SESSION_TTL=3600
K8S_API_PORT=6443

# --- Helpers ---
error() {
  echo "❌ $1" >&2
  exit 1
}

check_cmd() {
  command -v "$1" >/dev/null 2>&1 || error "Required command '$1' not found"
}

# --- Preconditions ---
check_cmd oci
check_cmd grep

[[ -f "$CONFIG_FILE" ]] || error "OCI config not found at $CONFIG_FILE"
[[ -f "$SSH_PUB_KEY" ]] || error "SSH public key not found at $SSH_PUB_KEY"

# --- Get Compartment ID ---
COMPARTMENT_ID=$(grep '^tenancy=' "$CONFIG_FILE" | cut -d= -f2)

[[ -n "$COMPARTMENT_ID" ]] || error "Failed to read compartment ID"

echo "📦 Compartment ID: $COMPARTMENT_ID"
echo

# --- List Bastions ---
echo "🔐 Available Bastions:"
oci bastion bastion list \
  --compartment-id "$COMPARTMENT_ID" \
  --query 'data[].{name:name,id:id}' \
  --output table

echo
read -rp "Enter Bastion ID: " BASTION_ID
[[ -n "$BASTION_ID" ]] || error "Bastion ID is required"

echo
echo "🔎 Verifying Bastion..."
oci bastion bastion get --bastion-id "$BASTION_ID" >/dev/null

# --- List Clusters ---
echo
echo "☸️  Active OKE Clusters:"
oci ce cluster list \
  --compartment-id "$COMPARTMENT_ID" \
  --all \
  --query 'data[?"lifecycle-state"==`ACTIVE`].{name:name,id:id}' \
  --output table

echo
read -rp "Enter Cluster ID: " CLUSTER_ID
[[ -n "$CLUSTER_ID" ]] || error "Cluster ID is required"

# --- Get Private Endpoint IP ---
echo
echo "🌐 Fetching Kubernetes API private IP..."
oci ce cluster get \
  --cluster-id "$CLUSTER_ID" \
  --query 'data.endpoints' \
  --raw-output

echo
read -rp "Enter K8S_PRIVATE_IP without PORT: (example: 10.0.0.92)" K8S_PRIVATE_IP
[[ -n "$K8S_PRIVATE_IP" ]] || error "K8S_PRIVATE_IP is required"
echo "➡️  Kubernetes Private IP: $K8S_PRIVATE_IP"

# --- Create Bastion Session ---
echo
echo "🚀 Creating Bastion Port Forwarding Session..."
SESSION=$(oci bastion session create-port-forwarding \
  --bastion-id "$BASTION_ID" \
  --target-private-ip "$K8S_PRIVATE_IP" \
  --target-port "$K8S_API_PORT" \
  --session-ttl "$SESSION_TTL" \
  --ssh-public-key-file "$SSH_PUB_KEY" \
  --query 'data.id' \
  --raw-output)

echo
echo "✅ Bastion session created successfully"
echo "🆔 Session ID: $SESSION"
echo
echo "⏳ Session TTL: ${SESSION_TTL}s"

echo "🔌 Connecting SSH tunnel (leave this open)"
# Poll until the SSH command becomes available
SSH_CMD=""
MAX_ATTEMPTS=30
SLEEP_SECONDS=2
for i in $(seq 1 "$MAX_ATTEMPTS"); do
  SSH_CMD=$(oci bastion session get --session-id "$SESSION" \
    --query 'data."ssh-metadata".command' --raw-output 2>/dev/null || true)
  if [[ -n "$SSH_CMD" ]]; then
    break
  fi
  STATE=$(oci bastion session get --session-id "$SESSION" \
    --query 'data."lifecycle-state"' --raw-output 2>/dev/null || true)
  echo "⏳ Waiting for session to be ready (state: ${STATE:-unknown})..."
  sleep "$SLEEP_SECONDS"
done

[[ -n "$SSH_CMD" ]] || error "Failed to retrieve SSH command from bastion session"

# Substitute placeholders safely
SSH_CMD=${SSH_CMD//<privateKey>/$SSH_PRIVATE_KEY}
SSH_CMD=${SSH_CMD//<localPort>/$K8S_API_PORT}

echo "run this command: "
echo "$SSH_CMD"