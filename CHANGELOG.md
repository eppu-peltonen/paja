# Changelog

Each release's section becomes its release notes. Add what a change means for someone using Paja under Unreleased;
when tagging `vX.Y.Z`, rename Unreleased to `X.Y.Z` (the release workflow reads that section) and start a new one.

## Unreleased

- Prebuilt `paja` binaries for Windows, macOS (Apple silicon and Intel) and Linux, attached to each GitHub release with
  a `SHA256SUMS` file.
- Paja Desk installers for Windows, macOS and Linux in each release, each with the matching `paja`; on Windows it goes
  on your `PATH`. Desk shows the `paja` it found and warns when its version differs.
- `paja --version` names the commit the binary was built from; with `--json` it prints `{"version", "commit"}`.
