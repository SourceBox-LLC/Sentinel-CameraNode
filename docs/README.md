# Camera Node docs

Supplementary documentation for Sentinel Camera Node. The top-level `README.md` is the user-facing install + operation guide; `AGENTS.md` is the developer / LLM-facing architecture reference. The docs in this tree cover the things that don't fit cleanly in either.

## Runbooks (`docs/runbooks/`)

Step-by-step procedures for when something's gone wrong. Each runbook names the symptom, lists what access you need, walks through the diagnostic steps in order, and ends with a rollback + escalation path.

- [video-not-showing.md](runbooks/video-not-showing.md) — camera registered but the tile is black / buffering / stuck on "stream not started yet"
- [local-mode-setup.md](runbooks/local-mode-setup.md) — installing standalone on a LAN with no Command Center pairing: how to find the node URL, the threat model, recording-toggle behaviour per mode, and the upgrade path to Connected

## Architecture Decision Records (`docs/adr/`)

One decision per file, numbered in order. ADRs capture the *why* behind a non-obvious choice so future maintainers don't re-litigate it. Format per Michael Nygard's template (Context / Decision / Consequences).

- [0001-pi-software-encoding.md](adr/0001-pi-software-encoding.md) — why the Raspberry Pi path uses libx264 and not the hardware `h264_v4l2m2m` encoder
- [0002-machine-id-encryption-key.md](adr/0002-machine-id-encryption-key.md) — why the at-rest encryption key derives from the OS machine identifier, not a user passphrase or the OS keychain
- [0003-sqlite-recording-store.md](adr/0003-sqlite-recording-store.md) — why recordings and snapshots live in SQLite BLOB columns rather than flat files or an embedded KV store
- [0004-installer-binary-integrity.md](adr/0004-installer-binary-integrity.md) — why the installer trusts HTTPS + GitHub for now and defers offline binary-signature verification (with the ready-to-execute plan for when it's revisited)

## Writing new docs

- **Runbook** — when you catch yourself pasting the same sequence of commands into more than one support thread. Cheap to write, saves time forever.
- **ADR** — when you make a decision that was hard to make, or that someone else will almost certainly re-argue. Write it *while the tradeoffs are fresh*, not six months later.
- **README / AGENTS** — these two are the primary docs and get updated in-place with every feature. Don't fork them into `docs/`.
