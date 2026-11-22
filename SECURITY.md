# Security

## Reporting a vulnerability

Please report security issues privately through GitHub:
[report a vulnerability](https://github.com/hamin/lotel/security/advisories/new).
Don't open a public issue for them.

Say what you found, how to reproduce it, and which version you ran
(`lotel --version`). You'll get a reply within a few days, and a fix or a
plan once the problem is confirmed. Reporters are credited in the release
notes unless they'd rather not be.

## What counts

lotel stores telemetry that can be sensitive, so these matter most:

- data that should be redacted showing up in the UI or the API (prompts, tool
  content, emails, home paths; see [docs/privacy.md](docs/privacy.md))
- getting past the ingest token on the OTLP endpoint
- data outliving its retention limit
- anything that lets a request read or write outside the data directory

The UI has no login by design (see
[docs/configuration.md](docs/configuration.md)), so reading telemetry through
a UI port you exposed is not a vulnerability.

## Supported versions

Fixes land in the latest release. lotel is pre-1.0, so please upgrade rather
than expect backports.
