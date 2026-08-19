# Security Policy

## Supported versions

This project doesn't maintain multiple release branches — there's one
version, and it's whatever's on `main`. Security fixes land there and
nowhere else, so keeping current means pulling the latest `main`.

## Reporting a vulnerability

Please **don't** open a public issue for a security problem — that gives
anyone running an older copy a working exploit before a fix exists.

Instead, use GitHub's private reporting:
**Security → Advisories → Report a vulnerability** on this repository.
If that's not available to you for any reason, open an issue asking for a
private contact channel rather than describing the problem itself.

Please include:
- What the issue is and why it's exploitable
- Steps to reproduce it, or a proof of concept
- What you think the impact is (what an attacker gains, and what they'd
  need already — local access, a tampered config, a MITM position — to
  get there)

There's no funded team behind this project, so response times are
best-effort, not contractual. A genuine report will still get a real look
and a fix, not silence.

## What's already been hardened

The [Security section of the README](README.md#security) documents what's
already been checked and closed: credential encryption at rest, JQL
injection, PATH-based binary hijacking, and a TOCTOU hole in how config
and state files are written. That list is the actual result of a real
review, not a claim — if you find something it missed, that's exactly what
this policy is for.
