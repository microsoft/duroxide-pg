#!/usr/bin/env bash
# Tear down the resource group created by `provision_entra_test_pg.sh`.
#
# Uses the same naming convention as the provision script. Override
# DUROXIDE_PG_ENTRA_TEST_RG if you customized it during provisioning.

set -euo pipefail

err()  { printf '\033[31m%s\033[0m\n' "$*" >&2; }
ok()   { printf '\033[32m%s\033[0m\n' "$*" >&2; }
info() { printf '\033[36m%s\033[0m\n' "$*" >&2; }

if ! command -v az >/dev/null 2>&1; then
    err "az CLI not found on PATH."
    exit 1
fi

if [ -n "${AZURE_SUBSCRIPTION_ID:-}" ]; then
    az account set --subscription "$AZURE_SUBSCRIPTION_ID"
fi

if ! az account show >/dev/null 2>&1; then
    err "Not logged in to az CLI. Run 'az login' first."
    exit 1
fi

DEFAULT_SUFFIX=$(whoami | sed 's#.*[^[:alnum:]]##' | tr '[:upper:]' '[:lower:]' | cut -c1-12)
SUFFIX="${DUROXIDE_PG_ENTRA_TEST_PREFIX:-$DEFAULT_SUFFIX}"
RG="${DUROXIDE_PG_ENTRA_TEST_RG:-rg-duroxide-pg-entra-test-${SUFFIX}}"

if ! az group show --name "$RG" >/dev/null 2>&1; then
    info "Resource group $RG does not exist. Nothing to tear down."
    exit 0
fi

info "Deleting resource group $RG (background, no-wait)..."
az group delete --name "$RG" --yes --no-wait --only-show-errors --output none
ok "Delete initiated. Verify with: az group show --name $RG"
