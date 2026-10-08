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
| Complete bundle extraction with safe internal symlinks | Pending | Open |
| Explicit app directory and foreign-app protection | Pending | Open |
| Receipts, locks, repeat install, list/info/outdated | Pending | Open |
| Upgrade, uninstall and interrupted-operation recovery | Pending | Open |
| Preserve old app on failures and reject external changes | Pending | Open |
| Fixture versions and fault-injection tests | Pending | Open |
| Signed real artifact and functional smoke test | Pre-implementation gate below. Repeat through final installer. | Partial |
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

## Implementation order

1. Define the metadata contract and fixtures.
2. Add safe archive staging and bundle inspection.
3. Add durable ownership and lifecycle operations.
4. Verify synthetic failures and the real artifact.
5. Prepare opt-in Nix integration and the exact takeover/return procedure.

Each completed package receives a separate local commit.
No publication or system activation forms part of the current implementation work.
