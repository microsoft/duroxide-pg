# Feature Specification: Azure Entra Authentication for PostgreSQL Provider

**Branch**: feature/entra-auth-support  |  **Created**: 2026-04-29  |  **Status**: Draft
**Input Brief**: Allow `PostgresProvider` to connect to Azure Database for PostgreSQL using Microsoft Entra ID (Azure Active Directory) authentication.

## Overview

Today, the duroxide PostgreSQL provider connects to PostgreSQL using a static username and password. This works for self-hosted PostgreSQL and for Azure Database for PostgreSQL when password auth is enabled, but it does not work for Azure deployments that require Microsoft Entra ID (formerly Azure AD) authentication. Entra-authenticated Azure Postgres replaces the static password with a short-lived (roughly one hour) OAuth access token issued by Entra ID.

This feature adds first-class support for connecting the duroxide PostgreSQL provider to Azure Database for PostgreSQL using Entra ID. Operators running duroxide on Azure — under managed identities, workload identity in AKS, or service principals — will be able to use the provider without storing database passwords. The provider obtains and renews tokens automatically based on the ambient Azure identity available to the process, so callers do not write any token-handling code.

From a user perspective, configuring an Entra connection requires supplying the Azure Postgres connection target (host, database, principal name) and any Azure-specific options. Tokens are obtained at startup and renewed as needed for the lifetime of the provider, with no special action required from the caller. Connections to Azure Postgres always use TLS with full server-certificate verification, matching Azure's own security requirements.

## Objectives

- Enable the duroxide PostgreSQL provider to authenticate to Azure Database for PostgreSQL Flexible Server using Microsoft Entra ID.
- Eliminate the need to store database passwords or write token-refresh code when running on Azure.
- Support the common Azure identity sources operators rely on (managed identity, workload identity, environment-based service principal, and developer CLI sign-in for local development).
- Preserve all existing password-based behavior so current consumers are unaffected.
- Make token expiry transparent to operators — the provider must continue working correctly hours and days after startup without manual intervention.
- Enforce TLS with full server-certificate verification on Entra connections.

## User Scenarios & Testing

### User Story P1 – Connect to Azure Postgres using a managed identity

Narrative: A platform engineer deploys a duroxide-based service to Azure (App Service, Container Apps, or AKS) with a managed identity granted access to an Azure Database for PostgreSQL Flexible Server. They configure the duroxide provider with the server hostname, database name, and Entra principal name. At startup, the provider obtains an Entra token using the ambient managed identity, opens a TLS connection to Postgres, runs migrations, and starts servicing orchestrations — all without any database password.

Independent Test: Configure an Entra-authenticated provider against a test Azure Postgres Flexible Server with a managed identity that has been added as an Entra admin/role; observe that the provider initializes (migrations applied, pool ready) and that a simple orchestration round-trip succeeds.

Acceptance Scenarios:
1. Given a managed identity with database access, When the provider is constructed with valid host/db/user for an Entra connection, Then the provider initializes successfully and the first orchestration enqueue/fetch succeeds.
2. Given the Azure environment exposes managed-identity endpoints, When the pool opens additional connections under load, Then each new connection is authenticated successfully without caller intervention.
3. Given the provider has been running long enough that any initial token has expired, When the pool opens a brand-new physical connection, Then it authenticates successfully.

### User Story P2 – Local development against Azure Postgres using `az login`

Narrative: A developer wants to run the duroxide service against a shared Azure Postgres dev instance from their workstation. They sign in once with the Azure CLI and start the service. The provider picks up the developer's CLI credential automatically and connects without any password configuration.

Independent Test: With a logged-in `az` session that has Postgres access, constructing an Entra-authenticated provider on a workstation succeeds and orchestrations run end-to-end against the Azure server.

Acceptance Scenarios:
1. Given the developer is signed in via the Azure CLI and has Postgres access, When the developer starts the service locally, Then connections succeed using the CLI credential.
2. Given no Azure credential is available at runtime (no managed identity, no environment variables, no CLI session), When an Entra-authenticated provider is constructed, Then a clear error is returned indicating that no Entra credential could be obtained.

### User Story P3 – Custom schema with Entra auth

Narrative: A team running multi-tenant duroxide deployments on Azure uses custom schemas to isolate workflows. They need the same Entra authentication path to apply when selecting a non-default schema.

Independent Test: Constructing an Entra-authenticated provider with a non-default schema name initializes successfully against an Entra-enabled Azure Postgres and migrations are applied to that schema.

Acceptance Scenarios:
1. Given valid Entra options and an existing schema name, When the schema-aware Entra constructor is called, Then migrations run inside that schema and orchestrations operate on it.

### Edge Cases

- **No credential available**: `DefaultAzureCredential` cannot find any source — surfaces a clear, actionable error at construction time.
- **Token fetch fails transiently**: Network/IMDS hiccup during pool's `before_connect` hook — surfaces as a retryable connection error consistent with the existing provider error classification.
- **Token-issuing principal lacks DB role**: Token acquired but Postgres rejects login — surfaces as a permanent auth error with enough context for the operator to diagnose.
- **TLS verification fails**: Hostname mismatch or untrusted CA — surfaces as a permanent error; the provider must NOT silently fall back to a weaker TLS mode.
- **Long-running pool**: A connection has been idle for hours — when the pool opens a new physical connection (after eviction or under load), it always uses a fresh token rather than a cached, expired one.
- **Entra auth used against non-Azure Postgres**: Caller misconfigures host — connection fails because Postgres rejects the bearer token; this surfaces clearly without crashing the provider.
- **Coexistence**: Existing password-based `new` / `new_with_schema` continue to work unchanged in the same process.

## Requirements

### Functional Requirements

- FR-001: The provider MUST offer a way to construct an Entra-authenticated connection to Azure Database for PostgreSQL using a connection target (host, port, database, principal) and Entra-specific options, without a password. (Stories: P1, P2)
- FR-002: The provider MUST support Entra-authenticated connections that target a non-default PostgreSQL schema, equivalent to the schema support already offered by the password-based path. (Stories: P3)
- FR-003: The provider MUST acquire Entra access tokens automatically using the ambient Azure identity (managed identity, workload identity, environment-based service principal, or developer CLI sign-in), without requiring caller code to handle tokens. (Stories: P1, P2)
- FR-004: The provider MUST ensure that pooled connections always authenticate with a non-expired token, including connections opened long after the provider was first constructed. (Stories: P1)
- FR-005: The Entra connection path MUST require TLS with full server-certificate verification and MUST NOT silently downgrade to a weaker mode. (Stories: P1)
- FR-006: The existing password-based construction paths (with and without custom schema) MUST remain functionally unchanged. (Stories: P1, P2, P3)
- FR-007: When no Entra credential is available at provider construction, the returned error MUST clearly identify the failure as Entra credential resolution, distinct from a generic database error. (Stories: P2)
- FR-008: When Entra token acquisition fails after the provider is running (e.g., transient network failure during pool growth), the resulting connection error MUST be surfaced as a transient/retryable failure, consistent with how the provider already classifies retryable connection errors. (Stories: P1)
- FR-009: The Entra connection path MUST allow callers to override the Azure token audience/scope to support sovereign clouds and other non-default Azure environments. (Stories: P1)
- FR-010: The Entra connection path MUST allow callers to tune the connection pool (max size, acquire timeout) consistent with the existing password path. (Stories: P1)
- FR-011: Documentation (README and rustdoc) MUST cover Entra usage, the required Azure setup (admin/role grants), and the recommended identity sources for production and local development. (Stories: P1, P2)

### Key Entities

- **Entra Auth Options**: Caller-supplied configuration for Entra-authenticated connections — token audience override, pool tuning, and any other Azure-specific knobs. Defaults match Azure Database for PostgreSQL Flexible Server's documented requirements so that the common case requires no tuning.

### Cross-Cutting / Non-Functional

- Adding Entra support MUST NOT break the public API of existing constructors or the duroxide `Provider`/`ProviderAdmin` trait surface.
- The new Azure identity dependency is added unconditionally; build-time and binary-size impact for downstream consumers must be acceptable.

## Success Criteria

- SC-001: A duroxide service running with a managed identity that has been granted Postgres access can run end-to-end orchestrations against Azure Database for PostgreSQL Flexible Server with no password configured. (FR-001, FR-003, FR-004, FR-005)
- SC-002: A developer who has signed in via the Azure CLI can start the same service locally against the same Azure Postgres without changing code or supplying a password. (FR-001, FR-003)
- SC-003: When the connection pool grows hours after startup, every new physical connection authenticates successfully (no expired-token authentication failures observed). (FR-004)
- SC-004: All pre-existing tests using the password-based constructors continue to pass without modification. (FR-006)
- SC-005: When Entra credential resolution fails, the error returned to the caller names the cause (e.g., "no Azure credential available") rather than a generic database error. (FR-007)
- SC-006: Connection attempts on the Entra path that would require a weaker TLS mode are rejected; no path silently downgrades. (FR-005)
- SC-007: README and rustdoc include a runnable example for Entra usage and document required Azure role/admin setup. (FR-011)

## Assumptions

- The default Azure token audience for Azure Database for PostgreSQL Flexible Server matches Microsoft's documented value for the OSS RDBMS service. Sovereign clouds use the analogous value and are reachable via the audience override (FR-009). Concrete strings are determined during planning.
- A Rust crate providing a `DefaultAzureCredential` equivalent is available, compatible with the project's tokio runtime, and stable enough to depend on. The exact crate and version are selected during planning.
- The Postgres client library used by the project (sqlx) supports a per-connection password-injection hook (or equivalent) suitable for supplying a fresh token each time a physical connection is opened. If the pinned version does not, an equivalent integration approach is acceptable.
- Azure Database for PostgreSQL accepts the Entra access token as the Postgres password field — no protocol-level OAuth handshake is required.
- The provider does not need to support pre-acquired/static tokens supplied by the caller in this initial release; if needed, that can be a follow-up addition.
- Integration tests against a real Azure Postgres Flexible Server are out of scope for the automated test suite; manual verification (or a separately gated, opt-in integration test) covers SC-001 and SC-002.

## Scope

In Scope:
- New construction paths for connecting the duroxide PostgreSQL provider to Azure Database for PostgreSQL using Microsoft Entra ID, both with and without a custom schema.
- Automatic token acquisition and renewal using the ambient Azure identity available to the process.
- TLS with full server-certificate verification on all Entra connections.
- An options type carrying Azure-specific configuration (audience override, pool tuning).
- Rustdoc and README updates documenting Entra usage and the required Azure setup.
- Unit tests covering option construction, error surface for missing credentials, and the TLS-enforcement contract.

Out of Scope:
- Live integration tests against a real Azure Postgres Flexible Server in CI (manual verification only for the initial release).
- Support for Azure Database for MySQL or other non-Postgres Azure database services.
- Caller-supplied static tokens or fully custom credential factories beyond what the options type exposes (may be revisited).
- Changes to the duroxide `Provider`/`ProviderAdmin` trait surface or to existing migrations.

## Dependencies

- A Rust Azure identity crate providing `DefaultAzureCredential` semantics (managed identity, environment-based service principal, Azure CLI fallback). Specific crate selected during planning.
- The existing Postgres client library used by the project (sqlx 0.8); no upgrade required.
- An Azure Database for PostgreSQL Flexible Server with Entra ID authentication enabled and the calling principal granted access.

## Risks & Mitigations

- **Risk**: The new Azure identity dependency adds noticeable build time / binary size to all consumers, including those not on Azure. **Mitigation**: Document the trade-off in README; revisit feature-gating in a follow-up if consumer feedback warrants it.
- **Risk**: API churn in the chosen Azure identity crate breaks the integration on minor version bumps. **Mitigation**: Pin a specific minor version; cover the integration with a unit test exercising the credential abstraction we depend on.
- **Risk**: Token refresh under sustained load causes rate limiting against the Azure identity endpoint. **Mitigation**: Rely on the Azure identity library's built-in token caching so per-connection acquisitions reuse a non-expired token; document this behavior.
- **Risk**: Operators misconfigure roles and see opaque "password authentication failed" errors. **Mitigation**: Document required Azure admin/role setup explicitly in README, including sample commands.
- **Risk**: TLS verification breaks for customers behind corporate proxies or with custom CAs. **Mitigation**: Document the requirement clearly; do not silently downgrade — operators with custom CAs must configure their system trust store.
- **Risk**: Tests cannot run end-to-end without an Azure account. **Mitigation**: Cover behavior with unit tests using a mock credential; provide a clearly-labelled opt-in integration test for those with an Azure environment.

## References

- Issue: none
- Research: none
- Microsoft documentation (external): "Use Microsoft Entra ID for authentication with Azure Database for PostgreSQL — Flexible Server".
- Existing code: current PostgreSQL provider source for the password-based construction paths and rustdoc.
