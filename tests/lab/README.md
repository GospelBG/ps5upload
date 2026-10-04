# Lab

Hand tools for poking a live PS5. These are deliberately manual — one
frame, one question, no workflow around it. When something on hardware
behaves oddly and you want to know exactly what the payload answered,
this is where you come.

For automated end-to-end checks use [`../smoke-hardware.mjs`](../README.md)
instead; for the full gate see [`../../TESTING.md`](../../TESTING.md).

## Pointing them at your console

Every script reads `PS5_ADDR` (or takes the address as its first
argument), so nothing here is tied to one network:

```sh
export PS5_ADDR=192.168.1.50           # console address
./status-runtime.sh
./hello-runtime.sh
```

The helper's only port is 9120 (AVA1); the console's ELF loader is
typically 9021. The committed defaults are generic — keep your own
addresses in a local env file rather than editing these scripts.

## What's here

**Runtime state** — `hello-runtime.sh`, `status-runtime.sh`,
`check-runtime-port.sh`, `smoke-runtime.sh`

**Transactions** — `begin-tx.sh`, `query-tx.sh`, `abort-tx.sh`,
`exercise-tx-stub.sh`, `full-cycle.sh`

**Payload lifecycle** — `send-payload.sh`, `shutdown-runtime.sh`,
`takeover-runtime.sh`, `verify-takeover.sh`,
`reload-and-verify-takeover.sh`, `reload-and-verify-replay.sh`

**Diagnostics** — `capture-runtime-trace.sh`, the legacy-protocol probes,
`elev_probe`

**Install/launch probes** — `test_install_launch.py`,
`test_launch_only.py`

The legacy-protocol probes are a baseline kept until the cutover
release is out, then removed; they speak the old protocol and do not
work against a current helper. For current checks use the engine's HTTP
API or `../smoke-hardware.mjs`.

## Don't delete these because nothing calls them

`scripts:audit` marks lab utilities as intentionally manual. A script
with no caller is the normal state here, not dead code.
