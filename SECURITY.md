# Security policy

LowQuery stores database connection profiles and talks to production databases. Thank you for helping keep that safe.

## Supported versions

Security fixes go into the latest release. Please update before reporting and mention the exact version you tested.

| Version | Supported |
| --- | --- |
| Latest release | Yes |
| Older releases | No |

## Reporting a vulnerability

**Do not open a public issue for a vulnerability.** Report it privately through GitHub:

1. Open the [Security tab](https://github.com/LowBloat/LowQuery/security) of the repository.
2. Choose **Report a vulnerability** and describe the problem, the affected version, operating system and database engine, and steps to reproduce.

Do not include real passwords, host names you consider private or production data. Use a disposable test database.

We aim to acknowledge reports within 7 days and to agree on a disclosure date once a fix is ready. We credit reporters in the release notes unless you prefer otherwise.

## How LowQuery handles secrets

- Passwords are stored only in the operating system's credential store (Windows Credential Manager, macOS Keychain, Linux Secret Service), never in `connections.json`, logs or the interface state.
- The interface never receives a saved password back from the native process.
- Connections are opened by the native process; the interface runs under a restrictive Content Security Policy and cannot reach the network directly.
- Updates are signed with a private key held by the maintainers and the release workflow, never in the repository; the app verifies the signature against the public key embedded at build time before installing anything.
- PostgreSQL `prefer` and `require` encrypt the connection without validating the server certificate, like libpq. Use `verify-full` when the server certificate must be checked.

## Scope

In scope: the desktop application in this repository, how it stores profiles and credentials, how it connects to databases, and the release workflow.

Out of scope: vulnerabilities in the database servers themselves; attacks that require an already compromised operating system account; SQL you run yourself against a server you are allowed to change.
