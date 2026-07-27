# Security Policy

## Supported versions

effiQueue is in **alpha (0.x)**. Only the latest `0.x` release receives
security fixes.

## Reporting a vulnerability

Please **do not open a public issue** for security problems.

Use GitHub's private vulnerability reporting: the repository **Security** tab →
**Report a vulnerability**. You'll get an acknowledgement as soon as possible
and updates as the issue is triaged and fixed.

## Operational notes (important)

effiQueue is a supervisor: it **spawns and kills worker processes** on the host
it runs on. Treat it accordingly:

- Run it under a **dedicated, unprivileged user**, not root.
- The `command` in the config is executed as-is — only put trusted commands
  there, and be aware that `shell = true` runs it through `sh -c` / `cmd /C`.
- The Prometheus `/metrics` endpoint has **no authentication**. Bind it to
  localhost or a private interface only.
- On Windows, graceful drain is best-effort (no `SIGTERM`); a hard kill after
  `drain_timeout` may drop an in-flight message.
