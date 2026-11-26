# Changelog

Each release's section becomes its release notes. Add what a change means for someone using Paja under Unreleased;
when tagging `vX.Y.Z`, rename Unreleased to `X.Y.Z` (the release workflow reads that section) and start a new one.

## Unreleased

- Paja Desk starts and stops runs: add a repository by its checkout folder, check it with `paja doctor`, run one item
  or keep running, and stop after the step or at once. `paja run --stop-on-stdin` is how it stops them.
- Paja Desk follows a running item live: its round, current step and model, how long the step has run, what each
  finished step spent and the pull request. `paja work show` includes the same `run` for an active item, and a run
  that stopped shows as interrupted.
- Prebuilt `paja` binaries for Windows, macOS (Apple silicon and Intel) and Linux, attached to each GitHub release with
  a `SHA256SUMS` file.
- Paja Desk installers for Windows, macOS and Linux in each release, each with the matching `paja`; on Windows it goes
  on your `PATH`. Desk shows the `paja` it found and warns when its version differs.
- `paja --version` names the commit the binary was built from; with `--json` it prints `{"version", "commit"}`.
