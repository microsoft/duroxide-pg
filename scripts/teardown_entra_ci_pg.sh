#!/usr/bin/env bash
# Tear down the resources created by `provision_entra_ci_pg.sh`.
#
# Deletes:
#   * The resource group (which removes the PG Flexible Server, firewall rule,
#     and Entra-admin assignment in one shot).
#   * The AAD application (and its federated credentials + service principal).
#
# Optional env vars: same as the provision script (RG, APP_NAME, AZURE_SUBSCRIPTION).

set -euo pipefail

err()  { printf '\033[31m%s\033[0m\n' "$*" >&2; }
ok()   { printf '\033[32m%s\033[0m\n' "$*" >&2; }
info() { printf '\033[36m%s\033[0m\n' "$*" >&2; }

command -v az >/dev/null 2>&1 || { err "az CLI missing"; exit 1; }

AZURE_SUBSCRIPTION="${AZURE_SUBSCRIPTION:-3a95a41f-f77e-4053-8f64-3c3d25111bdd}"
RG="${RG:-rg-duroxide-pg-entra-ci}"
APP_NAME="${APP_NAME:-duroxide-pg-entra-ci}"

if ! az account show >/dev/null 2>&1; then
    err "Not logged in to az CLI. Run 'az login' first."
    exit 1
fi
az account set --subscription "$AZURE_SUBSCRIPTION"

if az group show --name "$RG" >/dev/null 2>&1; then
    info "Deleting resource group $RG (background)..."
    az group delete --name "$RG" --yes --no-wait --only-show-errors --output none
    ok "RG delete initiated."
else
    info "Resource group $RG does not exist."
fi

APP_ID=$(az ad app list --display-name "$APP_NAME" --query "[0].appId" -o tsv 2>/dev/null || true)
if [ -n "$APP_ID" ]; then
    info "Deleting AAD application '$APP_NAME' ($APP_ID)..."
    az ad app delete --id "$APP_ID" --only-show-errors
    ok "AAD app deleted."
else
    info "AAD app '$APP_NAME' not found."
fi
