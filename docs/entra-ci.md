# Entra-auth Live Test in CI

This repo runs `tests/entra_live_test.rs` on every PR against a long-lived
Azure Database for PostgreSQL Flexible Server, authenticating via Microsoft
Entra ID. There are **no secrets** stored in GitHub — auth uses [GitHub
Actions OIDC federated workload identity][gh-oidc].

## What gets created

One-time provisioning (see `scripts/provision_entra_ci_pg.sh`) creates:

| Resource | Name | Notes |
|---|---|---|
| Resource group | `rg-duroxide-pg-entra-ci` | In subscription `3a95a41f-f77e-4053-8f64-3c3d25111bdd`, region `westus3` (eastus2 is provisioning-restricted in this subscription) |
| PG Flex Server | `pg-duroxide-entra-ci` | Burstable B1ms, **Entra-only auth** (password auth disabled), PG 16 |
| Firewall rule | `allow-public` | `0.0.0.0`–`255.255.255.255`. Safe because Entra is the auth gate and the server has no production data. |
| AAD application | `duroxide-pg-entra-ci` | Workload-identity-federated; **no client secret** |
| Federated creds | `github-pr-id`, `github-main-id` | Subjects: `repository_owner_id:6154722:repository_id:1157352866:pull_request`, `...:ref:refs/heads/main`. The `microsoft` org customizes the OIDC subject claim to the ID-based form — see [OIDC subject format](#oidc-subject-format). |
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
   "repository_owner_id:6154722:            credential matches subject;
    repository_id:1157352866:               returns AAD access token
    pull_request"                          
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
#    SERVICE_MANAGEMENT_REFERENCE is REQUIRED in the Microsoft tenant —
#    it's the Service Tree GUID for the owning service. Find your team's
#    GUID at https://servicetree.msftcloudes.com (open the Service node
#    and copy "Service Id").
SERVICE_MANAGEMENT_REFERENCE=<your-service-tree-guid> \
  scripts/provision_entra_ci_pg.sh

#    With --auto-set-vars, the script also sets the GitHub repo variables
#    via the gh CLI (requires `gh auth login` with admin access to the repo):
SERVICE_MANAGEMENT_REFERENCE=<your-service-tree-guid> \
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

## OIDC subject format

GitHub issues **exactly one** subject per OIDC token. Its format depends on the
repository's [subject claim customization][gh-sub]:

| `use_default` | Subject format |
|---|---|
| `true` (GitHub default) | `repo:OWNER/REPO:<context>` |
| `false` (this repo) | the customized claim keys, e.g. `repository_owner_id:<id>:repository_id:<id>:<context>` |

The `microsoft` org customizes it to the ID-based form, which resists
repo-rename attacks. Check the current setting with:

```bash
gh api repos/microsoft/duroxide-pg/actions/oidc/customization/sub
# {"use_default":false,"include_claim_keys":["repository_owner_id","repository_id","context"], ...}
```

**Only register credentials in the format this repo actually emits.** A
credential in the other format can never match, so it is permanent dead weight
on the app — and tenant security sweeps delete such credentials, which looks
exactly like CI breaking for no reason. `scripts/provision_entra_ci_pg.sh`
detects the format and registers only the matching pair; it warns about any
leftovers in the other format.

> Because both credentials depend on this customization, resetting it to
> `use_default: true` (or transferring the repo) breaks `main` and PR auth
> simultaneously.

## Updating the federated credentials

To allow another branch / environment to use the same AAD app, add another
federated credential. Build the subject from this repo's ID-based prefix
`repository_owner_id:6154722:repository_id:1157352866:` plus the context:

- `...:pull_request`
- `...:ref:refs/heads/BRANCH`
- `...:environment:ENVNAME`

Add via:

```bash
az ad app federated-credential create \
  --id "$APP_ID" \
  --parameters '{
    "name": "github-release-id",
    "issuer": "https://token.actions.githubusercontent.com",
    "subject": "repository_owner_id:6154722:repository_id:1157352866:ref:refs/tags/v1.0.0",
    "audiences": ["api://AzureADTokenExchange"]
  }'
```

> Subjects are matched **exactly** — wildcards are not supported. Re-running
> `scripts/provision_entra_ci_pg.sh` is the safer way to restore the standard
> pair, since it derives the subjects from the live GitHub API.

## Teardown

```bash
scripts/teardown_entra_ci_pg.sh
```

This deletes the resource group (server + firewall + Entra admin in one
shot) and the AAD application (federated creds + SP go with it).

## Troubleshooting

- **Workflow logs `Skipping live Entra test — missing repo vars`** — set
  the six repository variables (see Setup).
- **`AADSTS700213` from `azure/login@v2`** — "No matching federated identity
  record found for presented assertion subject". The app has no credential for
  the subject GitHub sent. The error message quotes the exact subject received;
  compare it against the app's credentials and re-run
  `scripts/provision_entra_ci_pg.sh` to restore the standard pair:
  ```bash
  az ad app federated-credential list --id "$APP_ID" \
    --query '[].{name:name,subject:subject}' -o table
  ```
  If `main` works but PRs fail (or vice versa), only one of the two credentials
  is missing — this is what a deleted credential looks like, so check the
  [Entra audit log](#auditing-credential-changes) before assuming misconfiguration.
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

## Auditing credential changes

Federated credentials can be removed out-of-band (tenant security sweeps have
done exactly this), and the only symptom is a failing `azure/login@v2` step.
The Entra audit log records every change, including the before/after subject:

```bash
APP_OBJECT_ID=$(az ad app show --id "$APP_ID" --query id -o tsv)
az rest --method GET --url \
  "https://graph.microsoft.com/v1.0/auditLogs/directoryAudits?\$filter=targetResources/any(t:t/id eq '$APP_OBJECT_ID')&\$top=50"
```

Look for `Update application` entries whose `modifiedProperties` include
`FederatedIdentityCredentials`; `oldValue` and `newValue` show exactly which
credentials existed before and after.

[gh-sub]: https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/about-security-hardening-with-openid-connect#customizing-the-subject-claims-for-an-organization-or-repository
[gh-oidc]: https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/about-security-hardening-with-openid-connect
