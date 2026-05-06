#!/usr/bin/env bash
# One-time provisioning for the Entra-auth live test in GitHub Actions CI.
#
# What this creates (idempotent — safe to re-run):
#   * Resource group     rg-duroxide-pg-entra-ci
#   * Flexible Server    pg-duroxide-entra-ci          (Burstable B1ms,
#                                                       Entra-only auth)
#   * Database           postgres                      (default)
#   * Firewall rule      allow-public                  (0.0.0.0-255.255.255.255;
#                                                       Entra is the auth gate,
#                                                       no password auth on
#                                                       the server)
#   * AAD application    duroxide-pg-entra-ci          (federated workload identity)
#   * Service principal  for the application
#   * Federated creds    on the AAD app:
#                          - repo:OWNER/REPO:pull_request
#                          - repo:OWNER/REPO:ref:refs/heads/main
#   * Entra admin        the application's service principal
#
# The script also prints the exact `gh variable set` commands (or the values
# to paste into the GitHub repo settings UI) so the workflow can authenticate.
#
# Required:
#   * az CLI logged in (`az login`)
#   * gh CLI (optional; only used when --auto-set-vars is passed)
#
# Optional env vars:
#   GH_REPO              Repo slug, default "microsoft/duroxide-pg"
#   AZURE_SUBSCRIPTION   Subscription ID; default below
#   LOCATION             Azure region; default eastus2
#   RG                   Resource group name; default rg-duroxide-pg-entra-ci
#   SERVER               Server name; default pg-duroxide-entra-ci
#   APP_NAME             AAD app display name; default duroxide-pg-entra-ci
#   SERVICE_MANAGEMENT_REFERENCE
#                        Service Tree GUID for the owning service.
#                        REQUIRED in the Microsoft tenant (tenant policy
#                        rejects AAD app creation without it). Find your
#                        team's GUID at https://servicetree.msftcloudes.com
#                        — pick the Service node and copy the "Service Id".
#
# Flags:
#   --auto-set-vars      Run `gh variable set` for the required GitHub vars
#                        (requires `gh auth login` with admin:org / repo).

set -euo pipefail

err()  { printf '\033[31m%s\033[0m\n' "$*" >&2; }
ok()   { printf '\033[32m%s\033[0m\n' "$*" >&2; }
info() { printf '\033[36m%s\033[0m\n' "$*" >&2; }

require() { command -v "$1" >/dev/null 2>&1 || { err "Missing on PATH: $1"; exit 1; }; }
require az
require jq

AUTO_SET_VARS=0
for arg in "$@"; do
    case "$arg" in
        --auto-set-vars) AUTO_SET_VARS=1 ;;
        -h|--help)
            sed -n '1,40p' "$0"; exit 0 ;;
        *) err "Unknown arg: $arg"; exit 1 ;;
    esac
done

GH_REPO="${GH_REPO:-microsoft/duroxide-pg}"
AZURE_SUBSCRIPTION="${AZURE_SUBSCRIPTION:-3a95a41f-f77e-4053-8f64-3c3d25111bdd}"
LOCATION="${LOCATION:-eastus2}"
RG="${RG:-rg-duroxide-pg-entra-ci}"
SERVER="${SERVER:-pg-duroxide-entra-ci}"
APP_NAME="${APP_NAME:-duroxide-pg-entra-ci}"
SMR="${SERVICE_MANAGEMENT_REFERENCE:-}"
DATABASE="postgres"

if ! az account show >/dev/null 2>&1; then
    err "Not logged in to az CLI. Run 'az login' first."
    exit 1
fi

az account set --subscription "$AZURE_SUBSCRIPTION"
TENANT_ID=$(az account show --query tenantId -o tsv)
info "Subscription: $AZURE_SUBSCRIPTION"
info "Tenant:       $TENANT_ID"
info "Repo:         $GH_REPO"
info "RG/Server:    $RG / $SERVER"
info "App:          $APP_NAME"

# ---------- resource group ----------

az group create --name "$RG" --location "$LOCATION" --only-show-errors --output none
ok "RG ready."

# ---------- AAD application + service principal ----------

APP_ID=$(az ad app list --display-name "$APP_NAME" --query "[0].appId" -o tsv 2>/dev/null || true)
if [ -z "$APP_ID" ]; then
    info "Creating AAD application '$APP_NAME'..."
    if [ -z "$SMR" ]; then
        err "SERVICE_MANAGEMENT_REFERENCE is required to create AAD apps in the Microsoft tenant."
        err "Find your team's Service Tree GUID at https://servicetree.msftcloudes.com"
        err "and re-run with: SERVICE_MANAGEMENT_REFERENCE=<guid> $0"
        exit 1
    fi
    APP_ID=$(az ad app create --display-name "$APP_NAME" \
        --service-management-reference "$SMR" \
        --query appId -o tsv)
fi
ok "AAD app: $APP_ID"

SP_OBJECT_ID=$(az ad sp list --filter "appId eq '$APP_ID'" --query "[0].id" -o tsv 2>/dev/null || true)
if [ -z "$SP_OBJECT_ID" ]; then
    info "Creating service principal..."
    SP_OBJECT_ID=$(az ad sp create --id "$APP_ID" --query id -o tsv)
fi
ok "SP object id: $SP_OBJECT_ID"

# ---------- federated credentials ----------

add_federated() {
    local name="$1" subject="$2"
    local existing
    existing=$(az ad app federated-credential list --id "$APP_ID" \
        --query "[?name=='$name'].name" -o tsv 2>/dev/null || true)
    if [ -n "$existing" ]; then
        ok "Federated credential '$name' already exists."
        return
    fi
    info "Creating federated credential '$name' (subject: $subject)..."
    local body
    body=$(jq -nc --arg n "$name" --arg s "$subject" '{
        name: $n,
        issuer: "https://token.actions.githubusercontent.com",
        subject: $s,
        audiences: ["api://AzureADTokenExchange"]
    }')
    az ad app federated-credential create --id "$APP_ID" --parameters "$body" \
        --only-show-errors --output none
    ok "Federated credential '$name' created."
}

add_federated "github-pr"   "repo:${GH_REPO}:pull_request"
add_federated "github-main" "repo:${GH_REPO}:ref:refs/heads/main"

# ---------- Postgres Flexible Server ----------

if az postgres flexible-server show --resource-group "$RG" --name "$SERVER" >/dev/null 2>&1; then
    ok "Server $SERVER already exists; skipping create."
else
    info "Creating Flexible Server (~3-5 min)..."
    # Throwaway admin password: needed for create even with password-auth disabled.
    ADMIN_PW="$(openssl rand -base64 32 | tr -d '/+=' | head -c 24)A1!"
    az postgres flexible-server create \
        --resource-group "$RG" \
        --name "$SERVER" \
        --location "$LOCATION" \
        --tier Burstable --sku-name Standard_B1ms \
        --version 16 --storage-size 32 \
        --admin-user pgadmin --admin-password "$ADMIN_PW" \
        --active-directory-auth Enabled --password-auth Disabled \
        --public-access None --yes \
        --only-show-errors --output none
    ok "Server created."
fi

# ---------- firewall (allow public; Entra is the auth gate) ----------

az postgres flexible-server firewall-rule create \
    --resource-group "$RG" --name "$SERVER" \
    --rule-name "allow-public" \
    --start-ip-address "0.0.0.0" --end-ip-address "255.255.255.255" \
    --only-show-errors --output none
ok "Firewall rule 'allow-public' upserted (Entra is the auth gate)."

# ---------- Entra admin = the SP ----------

info "Setting Entra admin to the service principal..."
az postgres flexible-server ad-admin create \
    --resource-group "$RG" --server-name "$SERVER" \
    --object-id "$SP_OBJECT_ID" \
    --display-name "$APP_NAME" \
    --type ServicePrincipal \
    --only-show-errors --output none
ok "Entra admin set."

HOST="${SERVER}.postgres.database.azure.com"

# ---------- emit GitHub variable instructions ----------

cat <<EOF

==========================================================================
Provisioning complete.

The GitHub Actions workflow needs these *repository variables* (NOT secrets;
no client secret is involved — federated identity issues a token at runtime):

  AZURE_CLIENT_ID            $APP_ID
  AZURE_TENANT_ID            $TENANT_ID
  AZURE_SUBSCRIPTION_ID      $AZURE_SUBSCRIPTION
  ENTRA_TEST_HOST            $HOST
  ENTRA_TEST_DB              $DATABASE
  ENTRA_TEST_USER            $APP_NAME

Set them via the GitHub UI:
  https://github.com/${GH_REPO}/settings/variables/actions

Or via gh CLI:
  gh -R ${GH_REPO} variable set AZURE_CLIENT_ID --body "$APP_ID"
  gh -R ${GH_REPO} variable set AZURE_TENANT_ID --body "$TENANT_ID"
  gh -R ${GH_REPO} variable set AZURE_SUBSCRIPTION_ID --body "$AZURE_SUBSCRIPTION"
  gh -R ${GH_REPO} variable set ENTRA_TEST_HOST --body "$HOST"
  gh -R ${GH_REPO} variable set ENTRA_TEST_DB --body "$DATABASE"
  gh -R ${GH_REPO} variable set ENTRA_TEST_USER --body "$APP_NAME"

To delete everything later:
  scripts/teardown_entra_ci_pg.sh
==========================================================================
EOF

if [ "$AUTO_SET_VARS" -eq 1 ]; then
    require gh
    info "Setting GitHub repo variables via gh..."
    gh -R "$GH_REPO" variable set AZURE_CLIENT_ID       --body "$APP_ID"
    gh -R "$GH_REPO" variable set AZURE_TENANT_ID       --body "$TENANT_ID"
    gh -R "$GH_REPO" variable set AZURE_SUBSCRIPTION_ID --body "$AZURE_SUBSCRIPTION"
    gh -R "$GH_REPO" variable set ENTRA_TEST_HOST       --body "$HOST"
    gh -R "$GH_REPO" variable set ENTRA_TEST_DB         --body "$DATABASE"
    gh -R "$GH_REPO" variable set ENTRA_TEST_USER       --body "$APP_NAME"
    ok "GitHub repo variables set."
fi
