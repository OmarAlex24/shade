# Security policy

## Reporting a vulnerability

Use GitHub private vulnerability reporting: open the repository's **Security**
tab and choose **Report a vulnerability**. The report stays private to you and
the maintainers until a fix is published.

**Do not open a public issue, pull request or discussion for a security bug.**

Please include:

- The affected version or commit, plus your macOS and hardware details.
- What an attacker gains, and which trust boundary is crossed.
- Exact reproduction steps: the commands or SDK calls, and a minimal fixture
  repository if the problem depends on repository or dependency metadata.
- Observed behaviour versus expected behaviour, and any relevant JSON responses
  or `shade doctor --diagnostics` output.
- Redact any real credential. A synthetic token is enough to demonstrate a
  secret-handling flaw.

You should get an acknowledgement within **7 days**. Please give the maintainers
a chance to ship a fix before disclosing publicly.

## Supported versions

Only the latest `main` and the latest `0.x` release are supported. Older
releases receive no fixes; upgrade before reporting.

## Threat model

[docs/SECURITY.md](docs/SECURITY.md) is the precise threat model: Shade assumes
the repository and its dependency metadata can be malicious, and describes the
Git, filesystem, dependency and secret defences in detail.

Two boundaries are documented there and are **not** vulnerabilities:

- **The daemon is single-user.** It runs as your login user under a per-user
  LaunchAgent, with a `0600` Unix socket and user-only directories. It does not
  isolate multiple people or mutually distrusting accounts.
- **It is not a same-UID process sandbox.** A linked worktree necessarily
  exposes a writable common Git object database, so a local process running as
  you can bypass Git attributes with plumbing such as
  `git hash-object -w --no-filters`. The secret clean filter is an
  accidental-commit guardrail, not a boundary against a malicious local actor.

A report that only demonstrates one of those two documented limits will be
closed as working as designed. A report showing Shade consuming or publishing a
contaminated tree, leaking secret values into a response, event or log, or
executing dependency code that should never run, is in scope.
