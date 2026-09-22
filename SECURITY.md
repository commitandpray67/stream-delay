# Security policy

stream-delay handles stream keys and exposes a local control API, so we take security reports seriously.

## Reporting a vulnerability

Please **do not open a public issue**. Report it privately through GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
("Security" tab → "Report a vulnerability").

Include what you found, how to reproduce it, and the impact you expect. We aim to acknowledge reports within 7 days.

## Scope

Areas of particular interest:

- The local HTTP/WebSocket API (authentication, DNS rebinding, cross-origin access from web pages).
- Handling of stream keys and other secrets (storage, logging, display).
- Parsing of untrusted network input (RTMP chunks, AMF0, FLV tags).

## Supported versions

Until v1.0, only the latest release receives fixes.
