# Security Policy

## Supported versions

ps5upload ships from `main` as a rolling release; only the **latest
release** receives fixes. Please reproduce any issue on the most recent
[release](https://github.com/phantomptr/ps5upload/releases) before
reporting.

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

Report privately via GitHub's **[Report a
vulnerability](https://github.com/phantomptr/ps5upload/security/advisories/new)**
button (Security tab → Report a vulnerability). This opens a private
advisory only you and the maintainer can see.

If you can't use that, reach the maintainer on
[Discord](https://discord.gg/fzK3xddtrM) and ask for a private channel —
don't post details publicly.

Please include:

- what the issue is and its impact,
- steps to reproduce (or a proof of concept),
- affected version / platform.

You'll get an acknowledgement as soon as practical. This is a free,
volunteer-maintained project, so timelines are best-effort — but security
reports are taken seriously and prioritized.

## Scope

ps5upload talks to a **jailbroken PS5 over a LAN**. The AVA1 protocol
(port 9120) uses the Noise XX handshake and encrypts and authenticates
every byte; the console only accepts devices that were paired by comparing
a six-digit code, or the engine that launched it. Weaknesses in pairing,
the handshake, or AVA1's message parsers are squarely in scope. The
local engine binds loopback (`127.0.0.1:19113`) and rejects non-loopback
callers except the PS5-facing `/pkg-host/` route. Reports most in scope:

- the desktop/Android client and the local engine service,
- the PS5 payload's network-facing handlers,
- the release/build pipeline and bundled dependencies.

Out of scope: attacks that require already-untrusted access to your LAN or
physical access to an unlocked machine, and the security of the
third-party jailbreak/loader the payload depends on.
