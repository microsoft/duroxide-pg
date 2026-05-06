# Entra-auth Live Test in CI

This repo runs `tests/entra_live_test.rs` on every PR against a long-lived
Azure Database for PostgreSQL Flexible Server, authenticating via Microsoft
Entra ID. There are **no secrets** stored in GitHub — auth uses [GitHub
Actions OIDC federated workload identity][gh-oidc].

## What gets created

One-time provisioning (see `scripts/provision_entra_ci_pg.sh`) creates:

| Resource | Name | Notes |
|---|---|---|
| Resource group | `rg-duroxide-pg-entra-ci` | In subscription `3a95a41f-f77e-4053-8f64-3c3d25111bdd`, region `eastus2` |
| PG Flex Server | `pg-duroxide-entra-ci` | Burstable B1ms, **Entra-only auth** (password auth disabled), PG 16 |
| Firewall rule | `allow-public` | `0.0.0.0`–`255.255.255.255`. Safe because Entra is the auth gate and the server has no production data. |
| AAD application | `duroxide-pg-entra-ci` | Workload-identity-federated; **no client secret** |
| Federated creds | `github-pr`, `github-main` | Subjects: `repo:microsoft/duroxide-pg:pull_request`, `repo:microsoft/duroxide-pg:ref:refs/heads/main` |
| Entra admin | the SP above | Service principal is the test user |

Approximate cost: ~$15-20/month for the Burstable B1ms server.

## How auth works at runtime

```
GitHub Actions runner                     Azure
─────────────────────                     ──────
PR triggers workflow                       
         │                                  
         ▼                                  
azure/login@v2                              
   issues OIDC token w/ subject ───────►   AAD app verifies federated
   "repo:microsoft/duroxide-pg:             credential matches subject;
    pull_request"                           returns AAD access token
         │                                  
         ▼                                  
sets env: AZURE_FEDERATED_TOKEN_FILE,       
          AZURE_CLIENT_ID,                  
          AZURE_TENANT_ID                   
         │                                  
         ▼                                  
cargo test → PostgresProvider::new_with_entra(...)
   default chain tries WorkloadIdentityCredential first.
   Those env vars are exactly what it requires, so it succeeds.
         │                                  
         ▼                                  
   token used as PG password (via Pool::set_connect_options)
                          ─────────────►   PG Flex server validates token
                                            against the Entra admin
                                            (the SP). Auth succeeds.
```

PRs from **external forks** cannot read repo variables, so the workflow's
preflight job emits a warning and the live test is skipped. This is by
design — we don't want fork PRs running CI against our Azure resources.

## Setup procedure

You need: `az` CLI, `jq`, optionally `gh` CLI. Bash environment (Git Bash or
WSL on Windows).

```bash
# 1. Log into Azure as someone with Contributor + Application Developer in
#    the target subscription/tenant.
az login
az account set --subscription 3a95a41f-f77e-4053-8f64-3c3d25111bdd

# 2. Run the provisioning script. Idempotent — safe to re-run.
scripts/provision_entra_ci_pg.sh

#    With --auto-set-vars, the script also sets the GitHub repo variables
#    via the gh CLI (requires `gh auth login` with admin access to the repo):
scripts/provision_entra_ci_pg.sh --auto-set-vars
```

The script prints the exact `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`,
`AZURE_SUBSCRIPTION_ID`, `ENTRA_TEST_HOST`, `ENTRA_TEST_DB`,
`ENTRA_TEST_USER` values needed. Without `--auto-set-vars`, set them via:

- GitHub UI: Repository → Settings → Secrets and variables → Actions →
  **Variables** tab → New repository variable
- Or: `gh -R microsoft/duroxide-pg variable set NAME --body VALUE`

> **These are repository variables, not secrets.** The values are non-secret
> identifiers (a tenant id, an app id, a hostname). The actual access token
> is minted at runtime via federated credential exchange.

## How CI invokes the test

The workflow [`.github/workflows/entra-live-test.yml`](../.github/workflows/entra-live-test.yml)
runs on PR + nightly + manual dispatch:

1. **`preflight` job** — verifies all six repo variables are populated. If
   any are missing (e.g., on a fork PR), emits a warning and short-circuits
   the workflow.
2. **`entra-live` job** —
   1. `azure/login@v2` exchanges GitHub's OIDC token for an AAD access token
      using the federated credential.
   2. The login action sets `AZURE_FEDERATED_TOKEN_FILE`,
      `AZURE_CLIENT_ID`, and `AZURE_TENANT_ID`. These are the env vars our
      `WorkloadIdentityCredential` (first in the default chain in
      `src/entra.rs`) reads.
   3. `cargo test --test entra_live_test -- --ignored --nocapture` runs the
      live test, which connects to the PG server using a fresh Entra token
      and exercises schema migrations + a basic query.

## Updating the federated credentials

To allow another branch / environment to use the same AAD app, add another
federated credential. Examples of valid GitHub OIDC subjects:

- `repo:OWNER/REPO:pull_request`
- `repo:OWNER/REPO:ref:refs/heads/BRANCH`
- `repo:OWNER/REPO:environment:ENVNAME`

Add via:

```bash
az ad app federated-credential create \
  --id "$APP_ID" \
  --parameters '{
    "name": "github-release",
    "issuer": "https://token.actions.githubusercontent.com",
    "subject": "repo:microsoft/duroxide-pg:ref:refs/tags/v*",
    "audiences": ["api://AzureADTokenExchange"]
  }'
```

## Teardown

```bash
scripts/teardown_entra_ci_pg.sh
```

This deletes the resource group (server + firewall + Entra admin in one
shot) and the AAD application (federated creds + SP go with it).

## Troubleshooting

- **Workflow logs `Skipping live Entra test — missing repo vars`** — set
  the six repository variables (see Setup).
- **`AADSTS70021` from `azure/login@v2`** — the federated credential's
  `subject` doesn't match the workflow's actual OIDC subject. The
  `pull_request` subject only matches PR runs from the same repo, not
  forks. For fork support, use a separate workflow on `pull_request_target`
  (with extra security review) — generally not recommended.
- **`FATAL: 28000 / no pg_hba.conf entry`** — the Entra principal isn't
  configured as a Postgres role on the server. The provisioning script sets
  the SP as the **Entra admin**, which auto-grants login. If you create a
  different SP as a non-admin role, you must run
  `pgaadauth_create_principal('<sp-name>', false, false)` as the admin.
- **Test passes locally but fails in CI** — confirm `azure/login@v2`'s
  `AZURE_TENANT_ID` matches the tenant where the AAD app lives. Mismatched
  tenant IDs produce a confusing 401 from Entra.
- **The PG server is unreachable from a runner IP** — the `allow-public`
  firewall rule allows all IPs. If you tightened it, you'll need the
  current GitHub Actions IP ranges (`https://api.github.com/meta` →
  `actions[]`) and a script to keep the firewall rules in sync.

## Why federated identity, not a client secret?

- **No secret rotation.** No `AZURE_CLIENT_SECRET` to expire and brick CI.
- **No secret to leak.** A leaked client id is harmless without the secret.
- **Tighter scope.** The token lifetime is tied to the workflow run, not a
  long-lived password.
- **Exercises the same code path as production.** Most production users
  authenticate via Workload Identity (AKS) or Managed Identity, both of
  which use the same `WorkloadIdentityCredential` / `ManagedIdentityCredential`
  chain. Running the live test through federated identity validates the
  primary credential path, not a developer-only fallback.

[gh-oidc]: https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/about-security-hardening-with-openid-connect
