# Security Policy

## Reporting a vulnerability

Please report security issues **privately**, not in a public issue.

Use GitHub's private vulnerability reporting: go to the repository's
**Security** tab → **Report a vulnerability**. This opens a private advisory
visible only to the maintainers.

clipd is a solo-maintained project, so responses are best-effort — but security
reports are triaged ahead of everything else. Please include what you'd expect:
affected version/commit, Windows build, reproduction steps, and impact.

## Threat model (what "a vulnerability" means here)

clipd is a **local, user-mode** application. By design it has no network
surface, no telemetry, no auto-update, no process injection, no API hooking, and
no kernel driver; its only privileged action is an optional `HKCU\...\Run`
registry value for start-with-Windows. This deliberately small surface is a
feature (see [`clipper-devpack/devpack/06-SAFETY-AND-VMS.md`](clipper-devpack/devpack/06-SAFETY-AND-VMS.md)).

**In scope** — things worth reporting:

- A crafted `config.toml` (or a config value like a filename template or output
  path) that causes path traversal, arbitrary file write outside the intended
  folder, or code execution.
- The optional post-save hook or any external-process invocation being coerced
  into running unintended commands.
- Memory-safety defects reachable from untrusted input.
- Any way clipd escalates privileges or persists beyond the documented Run key.

**Out of scope** — working as designed, not vulnerabilities:

- clipd records the screen and system/mic audio — that is its entire purpose.
- DRM-protected content captures as black frames (by design).
- The app requires a hardware video encoder and fails loudly without one.
- Selling the compiled binary while the source stays GPL-3.0 — that's the
  intended licensing model, not a flaw.

## Supported versions

The project is pre-1.0. Only the latest `main` (and the most recent tagged
release, once releases exist) receive fixes.
