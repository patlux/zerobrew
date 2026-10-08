# ImageOptim app-cask pilot

## Scope

The pilot adds one standalone macOS app cask without replacing Homebrew.
The initial artifact is ImageOptim 1.9.3 in a checksum-pinned tar.xz archive.
PKGs, DMGs, ZIP app archives, hooks, services, and user-data cleanup are outside this milestone.
The installer must reject unsupported required actions before it changes an app.
Zap metadata is information only. The pilot never removes preferences or caches.

## Acceptance ledger

| Requirement | Evidence | State |
| --- | --- | --- |
| Typed app metadata and fail-closed artifact validation | `zb_core::cask` tests and reduced ImageOptim API fixture | Implemented |
| SHA, safe identifiers, OS and architecture checks | `zb_core::cask` tests | Implemented |
| Complete bundle extraction with safe internal symlinks | Seven app I/O tests, plus real signed ImageOptim archive test | Implemented |
| Explicit app directory and foreign-app protection | Destination-scoped AppManager tests | I/O implemented; CLI in progress |
| Receipts, locks, repeat install, list/info/outdated | SQLite receipt, cross-process lock, fingerprint and numeric-version tests | I/O implemented; CLI in progress |
| Upgrade, uninstall and interrupted-operation recovery | Synthetic signed versions 1.0/2.0; abrupt child exit after publication for all three operations | I/O implemented |
| Preserve old app on failures and reject external changes | SHA, real rename permission failure, DB/journal triggers, external edit during recovery, sidecar and receipt corruption | I/O implemented |
| Fixture versions and fault-injection tests | 18 app I/O tests pass; one ignored worker is invoked by its parent test | Implemented |
| Signed real artifact and functional smoke test | Real archive passes the Rust staging path and signature verification. Repeat functional test through final CLI. | Partial |
| Pinned opt-in Nix integration and takeover/return procedure | Pending | Open |

## Initial verification, 2026-10-09

Host: mbpromax, arm64, macOS 27.0.
Baseline core tests: 49 passed through `mbx test -p zb_core --lib --locked`.
With metadata tests: 56 passed. Core Clippy passed with warnings denied.

The downloaded ImageOptim archive matches SHA-256:

```text
d4e453bbb55b6491d519a30e0e6de5beb7ce1a929e8f835767b638ef6ffd371d
```

The extracted app passes `codesign --verify --deep --strict`.
Its Developer ID belongs to Kornel Lesinski, team `59KZTZA4XR`.
The app contains arm64 and x86_64 executables and a stapled notarization ticket.

The vendor documents a blocking, hidden-UI command at https://imageoptim.com/command-line.html.
A local sandbox smoke test ran that executable against a generated PNG.
The PNG decreased from 21,222 bytes to 86 bytes. The process returned zero.
Pixel-equality verification remains required for the final end-to-end test.

The sandbox denied network access and writes outside the test directory.
It also denied access to the user preferences daemon.
Foundation ignored TMPDIR for worker files. A narrow worker-filename rule allowed those temporary files in the macOS user temp directory.
Positive and negative write probes checked that rule. No worker files remained afterward.
An earlier zero-exit run did not optimize the image because the sandbox denied worker temp files. That run is not success evidence.

The real Homebrew app, preferences and configuration remain unchanged.
Production takeover requires a separate explicit deployment instruction.

## Archive and bundle verification

The app staging path verifies SHA-256 before extraction.
It checks all members before it creates a private staging directory.
It rejects traversal, duplicate and case-colliding names, special files, hard links, and entries below symlinks.
It permits internal framework symlinks and checks their complete resolved chains.
A content/type/mode fingerprint detects external changes.
The macOS inspector verifies the existing signature and reads bundle metadata with plutil.
It never re-signs a downloaded app.

Seven focused I/O tests pass. A signed synthetic bundle passes inspection, then fails after executable tampering.
The explicit real-artifact test passes through the new Rust staging path.
It preserves the vendor signature, team identity, version, and framework symlinks.
I/O Clippy passes with warnings denied.

## Ownership and recovery verification

App state lives inside the explicit app destination, in a private `.zerobrew-apps` directory.
A destination-scoped lock prevents separate zerobrew roots from taking independent ownership.
The receipt stores cask metadata, bundle identity, version and a content/type/mode fingerprint.
Repeated installation verifies the fingerprint before it becomes a no-op.
Outdated checks reject same-version substitutions and upstream downgrades.
Upgrades require an explicit request and comparable numeric versions.

macOS exclusive rename and atomic exchange publish the bundle.
SQLite journals the pending operation before publication and commits the receipt afterward.
Staging and parent directories are synced. SQLite uses FULL synchronization and fullfsync.
An interrupted, uncommitted operation restores the prior app on next open.
Recovery stops without deleting either copy if an external program changed a transaction path.
User preferences and zap paths are never used by removal.

The lifecycle tests cover installation, repetition, upgrade, outdated, removal, foreign apps,
external edits, malformed receipts, symlinked state and SQLite sidecars, actual filesystem
rename failure, receipt/journal errors, recovery failure and cross-process contention.
A child test exits directly after publication without destructors; the parent verifies recovery
for install, upgrade and removal. This is process-exit evidence, not a power-loss simulation.
Parallel tests exposed a short inherited-lock window around subprocess creation.
An explicit lock guard now releases the lock after the SQLite connection closes.
The full focused suite and I/O Clippy pass after that change.

## Implementation order

1. Define the metadata contract and fixtures.
2. Add safe archive staging and bundle inspection.
3. Add durable ownership and lifecycle operations.
4. Verify synthetic failures and the real artifact.
5. Prepare opt-in Nix integration and the exact takeover/return procedure.

Each completed package receives a separate local commit.
No publication or system activation forms part of the current implementation work.
