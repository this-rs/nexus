# Security Policy

This directory is a **fork of Obscura**, which runs real, untrusted
JavaScript from arbitrary web pages through V8. Obscura's full security policy
applies to the vendored engine (see the upstream
[SECURITY.md](https://github.com/h4ckf0r0day/obscura/blob/main/SECURITY.md)).

## Reporting a vulnerability

**Please do not report security issues through public GitHub issues, pull
requests, or discussions.**

Report privately to this repository's maintainers (GitHub Security →
**Report a vulnerability**, or email the maintainers with "security" in the
subject). Include:

- Type of issue (SSRF, hang/crash, memory safety, cross-session data exposure…).
- The nexus commit (the Obscura tree is vendored under `obscura/`).
- Reproduction steps: page, tool call, or flags.
- Proof-of-concept and impact.

## SSRF protection (on by default)

- Navigation and fetches to **private/internal IPs** (RFC 1918, loopback,
  link-local, cloud metadata) are **blocked by default**.
- To reach a local dev server, set `OBSCURA_ALLOW_PRIVATE_NETWORK=1` in the
  server environment (or pass the engine's equivalent flag).
- **Never** enable this in deployments that serve untrusted prompts/URLs.
- DNS-resolution-time checks apply: a public hostname resolving to a private IP
  is rejected too.

## Operational notes

- The MCP server executes page JavaScript (`browser_evaluate`) and interacts
  with forms. Page content is **untrusted input to the agent**, not a trust
  boundary — the browser is not a sandbox.
- The MCP server has **no authentication** (stdio to a local process). Mount it
  for local agents only; protect any HTTP transport with your own auth.
- Do not log proxy credentials passed via `--proxy`.
