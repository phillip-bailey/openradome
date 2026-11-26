# Security policy

OpenRadome runs as a service on people's home networks: `install.sh` installs it as root, it listens on
network ports (Beast, raw and SBS, and optionally the dashboard, the HTTP API and the MCP server with its
token), and it reads radio and network data that anyone can send. Security problems are taken seriously.

## Reporting a vulnerability

**Please do not report security problems in public issues, discussions or pull requests.**

Report them privately through GitHub: open the repository's **Security** tab and choose
**Report a vulnerability** (GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)).
Only the maintainers can see the report.

Please include:

- the OpenRadome version (`openradome --version`) and the platform (for example Raspberry Pi OS, 32-bit, Pi 4);
- the relevant part of the configuration (leave out the MCP token, your position and anything else private);
- what an attacker can do, and from where (on the radio, on the local network, from the internet);
- the steps to reproduce it, or a proof of concept (a crafted message, capture file or request).

## What happens next

OpenRadome is maintained by one person in their spare time, so these are best-effort targets:

- a first reply within **7 days**;
- an assessment, and a plan for a fix, within **30 days**;
- a fixed release, then a published GitHub security advisory (with a CVE where it applies) describing the problem
  and the versions affected.

You will be credited in the advisory and the release notes, unless you would rather stay anonymous. Please give
us a reasonable time to release a fix before you disclose the problem publicly; we will keep you informed.

## Supported versions

Security fixes go into the **latest release** only. Please upgrade before reporting, and stay on the latest
release: `sudo ./install.sh` (or the one-line installer) upgrades in place and keeps your configuration.

## Scope

In scope: OpenRadome itself: the decoder and its network ports, the dashboard, the HTTP API, the MCP server
(token checks, TLS), `install.sh`, and the release archives and checksums.

Out of scope: the separate programs and services OpenRadome works with (feed clients, MLAT clients, tar1090,
flight-tracking networks), problems that need someone who already has root or the MCP token, and attacks that
only work when the dashboard or MCP port is deliberately exposed to the internet without TLS (the README
advises against it). Reports about those are still welcome as ordinary issues, without exploit details.

## Hardening tips

- Keep the data ports on `127.0.0.1` (`net-bind-address`) unless other machines need them.
- Keep the dashboard and the MCP server on your local network; never forward their ports from the internet.
- Use HTTPS for MCP (`mcp-tls-cert`, `mcp-tls-key`), keep `/etc/openradome/mcp-token` readable by root and the
  service only, and rotate the token if it may have leaked (see the README).
