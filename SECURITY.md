# Security policy

## Supported versions

Security fixes go into the latest release. RxFerrix is pre-1.0: there are no maintenance branches for older minor versions, so the fix for a problem in 0.11 is the next release.

## Reporting a vulnerability

Please do not open a public issue for a security problem.

Report it privately through GitHub: on the repository's **Security** tab, choose **Report a vulnerability**. The report is visible only to the maintainers.

Include what you can of:

- the version (or commit) and how the server is run (flags, storage backend, behind a proxy or not);
- what an attacker can do, and what they need to start with (no account, a user account, an administrator's token);
- steps or a small program that shows it.

You will get an answer once the report has been read, and credit in the release notes unless you prefer otherwise.

## What counts

The server, the CLI, the client packages, the dashboard, and the files under `deploy/`. Findings that need an administrator's token are still worth reporting; say so in the report.

Known advisories in the dependencies are checked by the `audit` workflow (`cargo audit`), on every change to the lockfile and weekly.
