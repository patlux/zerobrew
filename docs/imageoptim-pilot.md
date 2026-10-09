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
| Explicit app directory and foreign-app protection | Destination-scoped AppManager tests and four offline CLI acceptance tests | Implemented |
| Receipts, locks, repeat install, list/info/outdated | SQLite receipt, cross-process lock, fingerprint, numeric-version and full CLI lifecycle tests | Implemented |
| Upgrade, uninstall and interrupted-operation recovery | Synthetic signed versions 1.0/2.0; abrupt child exit after publication for all three operations | I/O implemented |
| Preserve old app on failures and reject external changes | SHA, real rename permission failure, DB/journal triggers, external edit during recovery, sidecar and receipt corruption | I/O implemented |
| Fixture versions and fault-injection tests | 19 app I/O tests pass; one ignored worker is invoked by its parent test | Implemented |
| Signed real artifact and functional smoke test | Final CLI download/install/repeat/info/outdated/uninstall; sandboxed ImageOptim: 6,174 → 84 bytes with exact decoded RGBA equality | Verified |
| Pinned opt-in Nix integration and takeover/return procedure | Immutable runtime commit; disabled mbpromax module; package/activation builds; enabled/disabled evaluations; Nix-binary smoke and isolated app/Caskroom backup restore | Prepared and verified; production handoff not executed |

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

## CLI and final real-artifact evidence

App mode is explicit: pass `--appdir` with an existing absolute destination.
Only `cask:<token>` names are accepted. Install, upgrade and uninstall operate on
exactly one app per transaction. Formula roots/prefixes, reset, migration, source
builds and `--no-link` are rejected in app mode.

Optional `--app-manifest` selects raw, pinned Homebrew JSON instead of the live API.
Optional `--app-archive` supplies a local artifact and requires a manifest.
Both paths use the same metadata, checksum, archive and signature validation.
This supports offline tests and a reproducible Nix activation without live metadata drift.
Without these options the CLI uses the Homebrew Cask API and the existing checksum-verifying downloader.

Four network-free CLI tests cover the complete two-version lifecycle, invalid actions
before state creation, missing/corrupt archives, invalid signatures, foreign bundles
and externally changed apps. All 64 CLI unit tests pass.
The workspace suite passes: 56 core, 64 CLI, four app CLI and 349 I/O tests.
The ignored subprocess worker is executed explicitly by its parent lifecycle test.
Network/parity integration tests remain opt-in.

The workspace run used Apple tools first in PATH and `RUBYOPT=--disable-gems`.
The original environment loaded a stale native JSON gem linked to a removed Ruby.
A separate test-only commit inspects each Mach-O architecture explicitly, because
this host's otool prints only the host slice by default. Both slices are still required.
No formula runtime behavior changed.

The reproducible smoke runner is:
`/Users/patwoz/dev/Personal/thirdparty/zerobrew/scripts/imageoptim-smoke.py`.
It downloads the pinned archive through zb, verifies the vendor team, repeats the
install, runs ImageOptim on a generated test-image copy inside a write/network sandbox,
compares decoded RGBA pixels with ImageMagick, rechecks the receipt and uninstalls.
It fingerprints the real `/Applications/ImageOptim.app` before and after, without
launching or modifying it. The source image, vendor bundle and production bundle
remained unchanged. ImageOptim returned zero and reduced 6,174 bytes to 84 bytes;
the 47 × 43 image's decoded pixels match exactly.
Evidence from the successful download run is `/private/tmp/zerobrew-final-appvze6n/report.json`.
The final ownership-guard build also passed using the local archive:
`/private/tmp/zerobrew-final-qqipbipn/report.json`.
A separate live Homebrew API install/outdated/uninstall passed in
`/private/tmp/zerobrew-live-mj3ic0rk`.
The CLI failure test now also exercises the actual downloader with network access
denied by a child-only sandbox. It preserves the prior app and receipt.

For `/Applications`, AppManager checks both standard Homebrew Caskroom locations.
A retained receipt blocks install, verification, removal and recovery, even if the
Homebrew app was manually removed. A fixture tests real and dangling receipt paths.
Isolated app directories remain available without taking ownership of the real app.

## Nix pilot completion

The Nix package pins runtime commit `de8a4e35e8c8cfda0d8b29754a86b726af7e13a8`.
It builds successfully with Rust 1.98.1 and passes 56 pure core tests in the sandbox.
The opt-in module is disabled in the persistent mbpromax configuration.
Homebrew still owns ImageOptim 1.9.3. No production activation occurred.
Both enabled and disabled host configurations were evaluated, and the enabled
activation script plus pinned vendor archive were built without executing them.
Seven targeted Nix assertions and the final complete `make nix-check` pass.

The Nix binary and Nix-fetched archive passed the same isolated functional test:
`/private/tmp/zerobrew-final-bjyfm7mh/report.json`.
App/Caskroom backup and restoration into a second disposable directory preserve
file contents, modes and symlink targets; the restored app signature is valid:
`/private/tmp/zerobrew-rollback-xl888e6k/report.json`.
This is a verified data restore, not a production Homebrew ownership round-trip.

The exact, separately authorized takeover and rollback steps are documented in
`/Users/patwoz/dev/Personal/infra/nix/docs/private/zerobrew-imageoptim-pilot.md`.
The later apply command on mbpromax is
`make -C /Users/patwoz/dev/Personal/infra switch`, after the backed-up handoff.
A production first/second switch and its target-app smoke remain intentionally pending.

## Implementation order

1. Define the metadata contract and fixtures.
2. Add safe archive staging and bundle inspection.
3. Add durable ownership and lifecycle operations.
4. Verify synthetic failures and the real artifact.
5. Prepare opt-in Nix integration and the exact takeover/return procedure.

Each completed package receives a separate local commit.
No publication or system activation forms part of the current implementation work.
