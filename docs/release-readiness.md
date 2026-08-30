# Release readiness and bug-hunt stopping rule

Voxkey will never be provably free of defects. The practical goal is to make
the residual risk explicit and bounded. Commit count is not a bug count: the
tree was already 250 commits past `v0.5.0` when this gate was introduced. Those
commits include new functionality, tests, defensive limits, observability,
refactors, documentation, and overlapping fixes from independent audits. This
tree must therefore be treated as the next release line, not as a small patch
to the already-released 0.5 code.

## Stabilization policy

1. Freeze features. Only fixes, tests, packaging, and release documentation go
   into the candidate.
2. Run `./scripts/verify all`. Formatting, Clippy with warnings denied, all Rust
   tests, and the isolated Python integration suite must pass with no retry.
3. Exercise every previously flaky boundary at least 20 times, either inside a
   deterministic stress test or through repeated invocations. A failure that
   cannot be explained and made deterministic is a release blocker, not “just
   a flake.”
4. Complete the manual graphical-session gate below in a disposable GNOME VM.
5. Publish a prerelease candidate and soak it for at least seven normal-use
   days. Record failures and total dictations, rather than only counting fixes.
6. Time-box at most two final audits. New low-severity findings enter the normal
   backlog; they do not restart an open-ended bug hunt.

The release may proceed when the automated gate passes, the manual gate is
signed off, the soak has no unresolved blocker, and there are no open critical
or high-severity defects. A known low-severity issue may be accepted only when
its scope and workaround are documented.

## Severity

- Critical: keyboard/input lockout, data or credential exposure/loss, arbitrary
  deletion, or a security boundary failure.
- High: the primary dictate/transcribe/insert path fails or hangs under a
  supported configuration, including a reproducible race.
- Medium: a secondary feature fails without corrupting state or trapping input.
- Low: cosmetic, diagnostic, or unusually narrow behavior with a safe
  workaround.

## Disposable GNOME/Wayland gate

Do not run this gate in the primary graphical session. Use a disposable Fedora
44 GNOME Wayland VM matching the release target, with SSH or console recovery
available.

- Install the candidate RPM into a clean user account; verify first-run portal
  permission, shortcut binding, settings launch, and a successful dictation.
- Exercise fast press/release/repress, held-key repeats, cancellation,
  transcription failure, unsupported characters, screen lock/unlock, GNOME
  Shell restart where supported, daemon stop, and logout/login.
- After every case, verify ordinary letters, Shift on both sides, Ctrl, Alt,
  Super, input-source switching, copy/paste, and typing in at least GTK, Qt,
  browser, terminal, and lock-screen fields.
- Force-close the portal and kill the daemon during injection. Confirm the EIS
  device disappears, no modifier remains logically pressed, and the daemon
  fails closed instead of silently recreating input authority.
- Verify install, upgrade from `v0.5.0`, disable/mask, uninstall, and reinstall.
- Save the OS/GNOME/portal versions, test results, and operator/date with the
  release notes. Any keyboard-state anomaly blocks release and requires a fresh
  VM snapshot before retesting.

The four Python tests skipped because a mock cannot provide a real compositor
or EIS endpoint are covered by this manual gate until a disposable nested-GNOME
runner can automate them. A tag must not be created merely because the offline
suite is green.

## When bug hunting stops

Open-ended “find more bugs” work stops once the gate above is green. From that
point, investigation starts from a concrete failure, changed risk surface, or
scheduled audit budget. This does not claim zero bugs; it prevents an
unbounded search from continually changing a candidate faster than it can be
validated.
