# Security Policy

## Supported versions

Actus follows semantic versioning. Security fixes are published against the
**latest released `1.x` minor**; please track the most recent release.

| Version | Supported          |
| ------- | ------------------ |
| 1.x     | :white_check_mark: |
| < 1.0   | :x:                |

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues,
discussions, or pull requests.**

Instead, use one of the following private channels:

- **GitHub** — open a private advisory via the repository's
  **Security → Report a vulnerability** tab
  ([GitHub private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)).
- **Email** — security@proximify.com.

Please include enough detail to reproduce: affected version, a minimal proof of
concept, the impact you observed, and any suggested mitigation.

## What to expect

- We aim to acknowledge a report within **3 business days**.
- We will keep you informed as we investigate, and coordinate a disclosure
  timeline with you once a fix is ready.
- Because Actus is built directly on [hyper] and [tokio], some reports may turn
  out to belong upstream — we will help route those appropriately.

[hyper]: https://hyper.rs/
[tokio]: https://tokio.rs/
