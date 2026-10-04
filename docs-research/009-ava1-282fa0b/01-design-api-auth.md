# Design 1 — authenticate the engine API and web UI

## Goal
A request to `/api/*` (and the SSE/stream endpoints) is served only with a valid bearer token,
in addition to today's loopback/`PS5UPLOAD_ALLOW_IP` guard. Removes the "IP allowlist is
spoofable; anyone on those hosts has full control" exposure the Dockerfiles document.

## Non-goals
Per-user accounts, roles, OAuth. One shared secret per engine is the right size for a
single-owner homelab tool. TLS is out of scope (LAN; the token is bearer over HTTP, same trust
model as the console's own pairing over the LAN) — note it in docs; a reverse proxy can add TLS.

## Design
- **Secret:** `PS5UPLOAD_API_TOKEN` (env). If unset, the engine **generates** one at first start,
  stores it `0600` at `<data dir>/.ps5upload/api_token`, and prints it once in the startup line
  with instructions. Docker users mount `/data` already, so it persists.
- **Transport:** `Authorization: Bearer <token>`. Also accept `?token=` **only** for the two
  browser-navigated endpoints that cannot set headers (`/api/game/inspect/image?...`, artwork
  `<img src>`), and for those compare the token and then redirect to a cookie-less path? Simpler:
  the web UI already fetches artwork via `fetch` (browserInvoke.ts:580, 712); keep headers and
  drop the query form entirely. Only the top-level UI (`GET /`) and static assets are public.
- **Loopback exemption, opt-in only:** `PS5UPLOAD_API_TOKEN_LOOPBACK_EXEMPT=1` keeps the desktop
  app working with no token when it talks to its own local engine over 127.0.0.1/::1. Default
  **off** in the Docker images, **on** for the bundled desktop engine (the desktop app sets it when
  it spawns the engine). This keeps the upgrade painless for desktop users and strict for servers.
- **Constant-time compare**, 401 with a one-line hint (never echo the token), rate-limit failed
  attempts per peer IP (e.g. 10/minute) to blunt guessing; log each refusal once per peer per minute.
- **DNS-rebinding:** the App/API agent is adding origin checks; the token makes rebinding harmless
  too (an attacker page cannot read the token).

## Entry points (engine, `engine/crates/ps5upload-engine/src/lib.rs`)
- `loopback_guard` (≈717) + `LoopbackGuardConfig` (≈617-700): add `token: Option<Arc<str>>` and
  `loopback_exempt: bool`; after `loopback_allows` passes, require the header unless
  (`loopback_exempt && peer is loopback`). Public paths: `/`, `/assets/*`, `/api/ps5/readiness`? —
  **no**: readiness reveals state; keep it behind the token. Only `/` and static assets public.
- Startup (`main`/`run`, where `PS5UPLOAD_ALLOW_IP` is parsed ≈9166-9213): read/generate the
  token; print `api token: <first 6>… (full value in <path>)`; wire into the config.
- `--healthcheck` (Dockerfile `HEALTHCHECK` calls `GET /api/jobs`): the self-probe runs inside the
  container as the same process family — have it read the token file and send the header.

## Entry points (client)
- `client/src/lib/browserInvoke.ts`: one place builds every `fetch` (lines 122-174, 565, 580,
  712, 894). Add `authHeaders()` that returns `{ authorization: \`Bearer ${token}\` }` when a
  token is configured, and spread it into each call. SSE (`accept: text/event-stream`, line 174)
  is a `fetch` too, so it gets the header.
- Token storage: Settings → Engine URL already exists (`client/src/state/engine.ts`,
  `getEngineUrl`). Add "Engine token" next to it, stored like the URL (the desktop app stores
  saved servers' encrypted credentials already — reuse that store, do not put it in plain
  localStorage on the web build; for the web build, session storage with a clear warning is the
  pragmatic floor).
- Desktop app: when it spawns the bundled engine, pass `PS5UPLOAD_API_TOKEN_LOOPBACK_EXEMPT=1`
  (no UX change for desktop users).

## Docs / images
- `engine/Dockerfile`, `Dockerfile.webui`, `compose.yaml`: add `PS5UPLOAD_API_TOKEN`
  (recommended) and the generated-token behaviour; rewrite the SECURITY paragraphs (the API is no
  longer unauthenticated). Keep the allowlist as defence in depth.
- FAQ/README: "Engine token" section; how to read it from the container (`docker exec cat
  /data/.ps5upload/api_token`).

## Tests
- Unit: guard refuses without header (401), accepts with header, loopback-exempt matrix,
  constant-time compare, rate limit after N failures.
- Integration (`ps5upload-engine` tests): healthcheck passes with the token file present;
  SSE endpoint requires the token; artwork fetch with header works.
- Client vitest: `browserInvoke` adds the header when a token is set, omits when not.
- Docker: the two "Docker-specific wording tests" (currently excluded on Linux) updated.

## Acceptance
With no token configured, a fresh server start prints a generated token and every `/api/*`
request from a non-loopback peer without it gets 401; the desktop app keeps working unchanged;
the web UI works after pasting the token once. Dockerfiles no longer say "UNAUTHENTICATED".

## Rollout
Ship in the cutover release (same release the ports change: users are already reading the
upgrade notes). Desktop: zero friction (exempt). Docker/web: one new env var or one printed token.
