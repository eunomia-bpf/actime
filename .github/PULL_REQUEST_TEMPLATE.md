<!-- Thanks for contributing to Actime. Fill in the sections below. Keep it
technical and concrete. See docs/DESIGN.md for the implementation contract and
docs/CONTRIBUTING.md (if present) for conventions. -->

## Summary

<!-- What does this change do, and why? One short paragraph. -->

## Motivation

<!-- The problem this solves. Link an issue if there is one (e.g. "Closes #12"). -->

## What plane(s) does this touch?

<!-- Check the ones that apply. -->

- [ ] policy (ActPlane integration, policy packs)
- [ ] observability (AgentSight integration, observation collection/export)
- [ ] backup (Akeep integration)
- [ ] actime-core (config, profiles, run store, reports, doctor)
- [ ] CLI surface (`docs/DESIGN.md` §10)
- [ ] packaging / docs / CI (this directory scope)

## Does this change the public contract?

The binding contract is `docs/DESIGN.md`. If your change alters any signature,
config field, CLI flag, profile, run-directory layout, or degradation behavior,
you **must** update `docs/DESIGN.md` in the same PR and call it out here.

- [ ] No — this is internal only.
- [ ] Yes — `docs/DESIGN.md` is updated in this PR.

If yes, summarize the contract change:

## How was it tested?

<!-- Commands you ran. New tests added. Note if the unprivileged smoke run still passes. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `actime doctor` is clean on my machine
- [ ] `actime run --policy off --no-backup -- /bin/echo hi` still runs end to end

## Checklist

- [ ] The change is consistent with `docs/DESIGN.md` (or updates it).
- [ ] No new dependency, telemetry, or network call was added silently.
- [ ] Fail-soft behavior is preserved: a missing plane degrades, it does not
      crash the run.
- [ ] Commit messages are clear and self-contained.
