# OpenAI Account Slots Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add an opt-in one-account-one-container execution slot for OpenAI accounts, with a dedicated proxy exit and fail-closed routing.

**Architecture:** `gateway-host` reconciles Docker resources through a port, while the OpenAI Provider chooses direct or slot transport after selecting an account. A small Rust sidecar performs authenticated HTTP/SSE forwarding through the account proxy; PostgreSQL stores desired slot identity and configuration while runtime health remains reconstructable.

**Tech Stack:** Rust 2024, Tokio, Axum, SQLx/PostgreSQL, Bollard Docker API, Reqwest/Rustls, Vue 3/TypeScript, Docker Compose

---

### Task 1: Persist account slot intent

**Files:**
- Create: `backend/migrations/0017_openai_account_slots.sql`
- Modify: `backend/crates/gateway-core/src/account/model.rs`
- Modify: `backend/crates/gateway-core/src/account/ports.rs`
- Modify: `backend/crates/gateway-store/src/postgres/provider_accounts/rows.rs`
- Modify: `backend/crates/gateway-store/src/postgres/provider_accounts/repository.rs`
- Modify: `backend/crates/gateway-store/src/postgres/provider_accounts/admin_adapter.rs`
- Test: `backend/crates/gateway-store/tests/postgres/provider_accounts/mod.rs`
- Test: `backend/crates/gateway-store/tests/postgres/schema_integrity.rs`

1. Add failing PostgreSQL tests for default-disabled slots, OpenAI-only enablement, proxy requirement, stable instance identity, generation updates and account-delete cascade.
2. Run `cargo +1.97.0 test --manifest-path backend/Cargo.toml --test main postgres::provider_accounts --locked` and confirm the new tests fail.
3. Add the frozen numbered migration and typed Core/Store models.
4. Implement repository reads and CAS updates without exposing proxy credentials in debug output.
5. Re-run the focused tests and `git diff --check`.
6. Commit as `feat(slots): persist OpenAI account slot intent`.

### Task 2: Add deployment configuration and slot ports

**Files:**
- Modify: `backend/crates/gateway-host/src/config.rs`
- Create: `backend/crates/gateway-host/src/slots/mod.rs`
- Create: `backend/crates/gateway-host/src/slots/model.rs`
- Create: `backend/crates/gateway-host/src/slots/ports.rs`
- Modify: `backend/crates/gateway-host/src/lib.rs`
- Modify: `deploy/config.example.yaml`
- Test: `backend/crates/gateway-host/tests/config.rs`

1. Add failing config tests proving slots are disabled by default and validating image, socket, timeouts and resource limits when enabled.
2. Define desired-resource, observed-health and Docker Engine port types; redact all secret-bearing fields from `Debug`.
3. Parse `host.openai_slots` while preserving existing configuration compatibility.
4. Run gateway-host tests and format checks.
5. Commit as `feat(slots): define host slot configuration`.

### Task 3: Implement the sidecar protocol and binary

**Files:**
- Create: `backend/apps/slot-sidecar/Cargo.toml`
- Create: `backend/apps/slot-sidecar/src/main.rs`
- Create: `backend/apps/slot-sidecar/src/config.rs`
- Create: `backend/apps/slot-sidecar/src/server.rs`
- Create: `backend/apps/slot-sidecar/src/forward.rs`
- Create: `backend/apps/slot-sidecar/tests/main.rs`
- Modify: `backend/Cargo.toml`

1. Write tests for bearer authentication, health readiness, proxy-required startup, header filtering, streaming backpressure, body limits and cancellation.
2. Add the workspace binary and a narrow internal protocol: `GET /readyz` and `POST /internal/v1/forward`.
3. Build a fresh Reqwest/Rustls client for the configured SOCKS5 or HTTP proxy, never allowing a direct connector.
4. Preserve allowed upstream status/headers/body while removing hop-by-hop and internal authentication headers.
5. Run the sidecar tests, strict Clippy and Rustfmt.
6. Commit as `feat(slots): add authenticated OpenAI slot sidecar`.

### Task 4: Reconcile Docker slot resources

**Files:**
- Modify: `backend/crates/gateway-host/Cargo.toml`
- Create: `backend/crates/gateway-host/src/slots/docker.rs`
- Create: `backend/crates/gateway-host/src/slots/reconciler.rs`
- Create: `backend/crates/gateway-host/src/slots/registry.rs`
- Test: `backend/crates/gateway-host/tests/slots.rs`

1. Add fake-engine tests for create/start/stop/recreate, stable volumes, per-slot networks, labels, health publication and exponential backoff.
2. Implement the Bollard adapter with exact owned-resource labels and no broad deletion.
3. Implement reconciliation as a Host daemon task with cancellation-safe operations.
4. Mount a root-readable per-slot authentication file and stable identity files; never use environment variables for secrets.
5. Run gateway-host tests and strict Clippy.
6. Commit as `feat(slots): reconcile account containers`.

### Task 5: Route selected OpenAI accounts through their slot

**Files:**
- Modify: `backend/crates/providers/openai/src/lib.rs`
- Modify: `backend/crates/providers/openai/src/provider/mod.rs`
- Modify: `backend/crates/providers/openai/src/provider/execution.rs`
- Create: `backend/crates/providers/openai/src/transport/slot.rs`
- Modify: `backend/crates/providers/openai/src/transport/mod.rs`
- Test: `backend/crates/providers/openai/tests/provider/contract.rs`
- Test: `backend/crates/providers/openai/tests/transport/slot.rs`

1. Add provider contract tests showing direct accounts are unchanged, Ready slot accounts use only the sidecar, and unhealthy slot accounts fail closed before upstream send.
2. Add a slot registry/transport port and inject it during Provider initialization.
3. Forward normalized HTTP/SSE requests with request IDs and cancellation, preserving existing error diagnostics and retry safety.
4. Ensure quota, catalog and credential maintenance continue to use their current paths until separately migrated; only inference uses slots in this release.
5. Run OpenAI Provider tests and strict Clippy.
6. Commit as `feat(openai): route inference through account slots`.

### Task 6: Expose Admin API and runtime status

**Files:**
- Modify: `backend/crates/gateway-admin/src/model/accounts.rs`
- Modify: `backend/crates/gateway-admin/src/ports/mod.rs`
- Modify: `backend/crates/gateway-admin/src/use_case/accounts.rs`
- Modify: `backend/crates/gateway-api/src/admin/accounts.rs`
- Test: `backend/crates/gateway-admin/tests/use_case/accounts.rs`
- Test: `backend/crates/gateway-api/tests/admin/accounts.rs`

1. Add failing tests for enabling, disabling and reading slot state, including OpenAI-only and proxy-required validation.
2. Add an authenticated Admin mutation for slot intent and include sanitized slot status in account responses.
3. Publish a runtime refresh after committed slot changes.
4. Verify unauthorized callers cannot view internal endpoints or secret material.
5. Run focused Admin/API tests.
6. Commit as `feat(admin): manage OpenAI account slots`.

### Task 7: Add account UI controls

**Files:**
- Modify: `frontend/src/api/accounts.ts`
- Modify: `frontend/src/types/accounts.ts`
- Modify: `frontend/src/views/accounts/components/AccountEditModal.vue`
- Create: `frontend/src/views/accounts/components/AccountSlotStatus.vue`
- Modify: `frontend/src/views/accounts/constants.ts`
- Modify: `frontend/src/views/accounts/index.vue`

1. Extend account types and API calls with the sanitized slot contract.
2. Add an OpenAI-only switch to the existing edit modal; disable it without an assigned proxy and show a short nearby reason.
3. Add a compact status cell using existing semantic tokens and Base components.
4. Verify loading, global-disabled, starting, ready and degraded states without exposing proxy URLs or runtime secrets.
5. Run `pnpm --dir frontend format:check` and `pnpm --dir frontend build`.
6. Validate light/dark and narrow layouts in a browser, then commit as `feat(accounts): manage independent account slots`.

### Task 8: Package and verify Docker deployment

**Files:**
- Modify: `deploy/Dockerfile`
- Modify: `deploy/compose.yaml`
- Modify: `deploy/README.md`
- Modify: `README.md`
- Create: `deploy/slot-sidecar.Dockerfile`
- Create: `scripts/verify-account-slots.sh`

1. Build the gateway and sidecar images from the same source revision.
2. Add the Docker Socket and slot state mounts only when the feature is enabled; document the root-equivalent security boundary.
3. Add an integration script that creates two slots with two capture proxies and proves distinct container, network, volume, identity and outbound paths.
4. Run Rustfmt, strict Clippy, backend tests, frontend checks, Compose validation and the container integration test.
5. Review logs and Docker metadata for leaked credentials or proxy URLs.
6. Commit as `docs(slots): document isolated account slots`.

