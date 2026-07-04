# ADR 0004: Installer binary integrity — HTTPS + GitHub trust, signing deferred

- **Status:** Accepted (signature verification deferred — see "Revisiting")
- **Date:** 2026-05
- **Deciders:** Camera Node maintainers

## Context

A Camera Node binary reaches an operator's machine one of two ways:

1. **Pre-built download.** The installer (`backend/scripts/install.sh` in the
   Command Center repo, served at `https://<cc>/install.sh` and run via
   `curl -fsSL … | bash`) fetches the release asset from
   `https://github.com/SourceBox-LLC/Sentinel-CameraNode/releases/...`,
   extracts it, `chmod +x`, and the operator runs it — often as a systemd
   service with camera + LAN access.
2. **Source build.** On platforms with no pre-built asset (Pi / aarch64
   today), the installer `git clone`s the repo over HTTPS and `cargo build`s.

**Integrity model today:** transport is HTTPS end-to-end (GitHub API, release
asset, `git clone`, and the rustup bootstrap all use TLS), and trust rests on
(a) GitHub's release infrastructure and our release account, and (b) the
CC-served `install.sh` itself. There is **no offline signature verification**
of the downloaded binary.

**Residual risk:** an attacker who compromises the GitHub release / our
release account (or who can tamper with the CC-served `install.sh`) could ship
a malicious binary that runs with the node's privileges. HTTPS does not defend
against this — it protects the channel, not the artifact's provenance.

A checksum published alongside the binary does **not** close this gap: it
travels over the same channel from the same release, so an attacker who can
replace the binary can replace its checksum too. Only an **offline signature**
(a key not present in the release pipeline's blast radius) provides real
artifact provenance.

## Decision

**Ship with the HTTPS + GitHub-trust model for now; defer offline signature
verification.**

Rationale:

- At current scale (pre-PMF, payment processor in test mode, no paying
  customers, a tiny release/download surface) the likelihood of a compromised
  release is low and the blast radius is small.
- Real verification needs a **maintainer-held signing keypair** and the
  private key stored as a **CI secret** — neither of which can be created or
  held by anything other than the maintainer. It then requires changes across
  two repos (release signing + installer verification). That is disproportionate
  to the current risk and falls in the production-hardening bucket deliberately
  deferred until there's a real user base (see the project's pre-PMF stance).

## Consequences

**Positive:**
- No signing-key generation or secret management to operate today.
- The release pipeline and installer stay simple; source-build users are
  covered by git-over-HTTPS provenance already.

**Negative:**
- No protection against a compromised GitHub release or release account — the
  documented residual risk above. Accepted at current scale.

**Neutral:**
- Source builds inherit their provenance from the cloned git repo; only the
  pre-built-download path is affected by the missing signature.

## Revisiting this decision

Implement signature verification **before any of**:

- Leaving payment test mode / onboarding the first paying customers.
- Listing the HACS Home-Assistant component or the binary in any
  broad-distribution channel (HACS default store, a package manager).
- Any sign of release-account / supply-chain risk.

**Implementation plan when revisited** (ready to execute):

1. Maintainer generates a signing keypair (e.g. `minisign` or `cosign`) and
   stores the **private** key as a GitHub Actions secret in the
   `Sentinel-CameraNode` repo.
2. `release.yml`: after building each binary, sign it and upload the detached
   signature (`.minisig` / `.sig`) as a release asset.
3. `install.sh` (Command Center repo): embed the **public** key; after the
   download, fetch the signature and verify it before `chmod +x` / extract;
   abort loudly on verification failure. Source-build path unchanged.

## References

- `backend/scripts/install.sh` (Command Center repo) — the installer; downloads
  + extracts the release binary or builds from source.
- `release.yml` — the Camera Node release workflow that would gain the signing step.
- ADR 0002 (`0002-machine-id-encryption-key.md`) — at-rest secret handling, the
  other half of the node's security posture.
