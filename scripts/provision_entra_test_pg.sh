#!/usr/bin/env bash
# Provision an Azure Database for PostgreSQL Flexible Server suitable for
# running `tests/entra_live_test.rs`.
#
# What this creates:
#   * A resource group        (rg-duroxide-pg-entra-test-${SUFFIX})
#   * A Flexible Server       (pg-duroxide-entra-${SUFFIX}, Burstable B1ms,
#                              Entra auth enabled, password auth enabled for fallback)
#   * The default database    (postgres)
#   * Firewall rule           (your current public IP)
#   * Entra admin             (the currently `az login`'d user)
#
# The signed-in user becomes the Entra admin AND the test user — no
# `pgaadauth_create_principal` step is required because the admin role is the
# principal the test will authenticate as.
#
# Re-running is safe: `az group create` and `az postgres flexible-server create`
# are idempotent on identical params; firewall rule and admin assignment are
# upserts.
#
# When done, run `scripts/teardown_entra_test_pg.sh` (or `az group delete`).
#
# Required:
#   * az CLI logged in (`az login`)
#   * Subscription set (`az account set --subscription <id>`) OR
#     AZURE_SUBSCRIPTION_ID env var
#
# Optional env vars:
#   DUROXIDE_PG_ENTRA_TEST_PREFIX   Name suffix (default: derived from whoami)
#   DUROXIDE_PG_ENTRA_TEST_LOCATION Azure region   (default: eastus)
#   DUROXIDE_PG_ENTRA_TEST_RG       Resource group (default: rg-duroxide-pg-entra-test-${SUFFIX})
#   DUROXIDE_PG_ENTRA_TEST_SERVER   Server name    (default: pg-duroxide-entra-${SUFFIX})

set -euo pipefail

# ---------- helpers ----------

err()  { printf '\033[31m%s\033[0m\n' "$*" >&2; }
ok()   { printf '\033[32m%s\033[0m\n' "$*" >&2; }
info() { printf '\033[36m%s\033[0m\n' "$*" >&2; }

require() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "Required command not found on PATH: $1"
        exit 1
    fi
}

require az
require curl

# ---------- inputs ----------

DEFAULT_SUFFIX=$(whoami | sed 's#.*[^[:alnum:]]##' | tr '[:upper:]' '[:lower:]' | cut -c1-12)
SUFFIX="${DUROXIDE_PG_ENTRA_TEST_PREFIX:-$DEFAULT_SUFFIX}"
LOCATION="${DUROXIDE_PG_ENTRA_TEST_LOCATION:-eastus2}"
RG="${DUROXIDE_PG_ENTRA_TEST_RG:-rg-duroxide-pg-entra-test-${SUFFIX}}"
SERVER="${DUROXIDE_PG_ENTRA_TEST_SERVER:-pg-duroxide-entra-${SUFFIX}}"
DATABASE="postgres"

# Optional subscription override
if [ -n "${AZURE_SUBSCRIPTION_ID:-}" ]; then
    info "Setting subscription to AZURE_SUBSCRIPTION_ID=${AZURE_SUBSCRIPTION_ID}"
    az account set --subscription "$AZURE_SUBSCRIPTION_ID"
fi

# Validate az login
if ! az account show >/dev/null 2>&1; then
    err "Not logged in to az CLI. Run 'az login' first."
    exit 1
fi

SUB_ID=$(az account show --query id -o tsv)
SUB_NAME=$(az account show --query name -o tsv)
info "Subscription: $SUB_NAME ($SUB_ID)"
info "Resource group: $RG"
info "Server:         $SERVER"
info "Location:       $LOCATION"

# Identify the signed-in user — they will be the Entra admin AND test user
USER_OID=$(az ad signed-in-user show --query id -o tsv)
USER_UPN=$(az ad signed-in-user show --query userPrincipalName -o tsv)
info "Entra admin / test user: $USER_UPN ($USER_OID)"

# ---------- resource group ----------

info "Creating resource group (idempotent)..."
az group create --name "$RG" --location "$LOCATION" --only-show-errors --output none

# ---------- server ----------

# Generate a strong random password for the optional password-auth fallback.
# We need *some* admin password even with Entra-only auth enabled, because
# `az postgres flexible-server create` requires --admin-password unless you
# pass a SKU/option combination that defaults to Entra-only. Keeping
# password-auth enabled is convenient for debugging.
ADMIN_PW=$(openssl rand -base64 32 | tr -d '/+=' | head -c 24)A1!

if az postgres flexible-server show --resource-group "$RG" --name "$SERVER" >/dev/null 2>&1; then
    ok "Server $SERVER already exists; skipping create."
else
    info "Creating Flexible Server (this takes ~3-5 minutes)..."
    az postgres flexible-server create \
        --resource-group "$RG" \
        --name "$SERVER" \
        --location "$LOCATION" \
        --tier Burstable \
        --sku-name Standard_B1ms \
        --version 16 \
        --storage-size 32 \
        --admin-user pgadmin \
        --admin-password "$ADMIN_PW" \
        --active-directory-auth Enabled \
        --password-auth Enabled \
        --public-access None \
        --yes \
        --only-show-errors \
        --output none
    ok "Server created."
fi

# ---------- firewall: current public IP ----------

info "Detecting current public IP..."
MY_IP=$(curl -fsS https://api.ipify.org || true)
if [ -z "$MY_IP" ]; then
    err "Could not detect public IP. Add a firewall rule manually with 'az postgres flexible-server firewall-rule create'."
    exit 1
fi
info "Public IP: $MY_IP"

az postgres flexible-server firewall-rule create \
    --resource-group "$RG" \
    --name "$SERVER" \
    --rule-name "allow-${SUFFIX}-cur" \
    --start-ip-address "$MY_IP" \
    --end-ip-address "$MY_IP" \
    --only-show-errors \
    --output none
ok "Firewall rule allow-${SUFFIX}-cur upserted for $MY_IP."

# ---------- Entra admin ----------

info "Setting Entra admin to $USER_UPN..."
# This is upsert-friendly: re-running with the same OID is a no-op.
az postgres flexible-server ad-admin create \
    --resource-group "$RG" \
    --server-name "$SERVER" \
    --object-id "$USER_OID" \
    --display-name "$USER_UPN" \
    --type User \
    --only-show-errors \
    --output none
ok "Entra admin set."

# ---------- output ----------

HOST="${SERVER}.postgres.database.azure.com"

cat <<EOF

==========================================================================
Provisioning complete. Set these env vars to enable the live Entra test:

  export DUROXIDE_PG_ENTRA_LIVE_TEST=1
  export DUROXIDE_PG_ENTRA_TEST_HOST="${HOST}"
  export DUROXIDE_PG_ENTRA_TEST_DB="${DATABASE}"
  export DUROXIDE_PG_ENTRA_TEST_USER="${USER_UPN}"

Then run:

  cargo test --test entra_live_test -- --ignored --nocapture

To delete the resource group when done:

  scripts/teardown_entra_test_pg.sh
  # or:
  az group delete --name "${RG}" --yes --no-wait

PowerShell equivalents (paste these instead in a pwsh prompt):

  \$env:DUROXIDE_PG_ENTRA_LIVE_TEST = "1"
  \$env:DUROXIDE_PG_ENTRA_TEST_HOST = "${HOST}"
  \$env:DUROXIDE_PG_ENTRA_TEST_DB   = "${DATABASE}"
  \$env:DUROXIDE_PG_ENTRA_TEST_USER = "${USER_UPN}"
==========================================================================
EOF
