# gitdesk

A desktop Git client built step by step in the
[A Git client in Rust](https://hackingwithrust.com/projects/gitdesk/) track.

A two-crate Cargo workspace:

- `crates/gitdesk-core` — every piece of Git logic: opening repositories,
  reading objects, refs, the index. Headless, so it can be checked against
  real `git` output on every step.
- `crates/gitdesk` — the desktop shell. The window arrives later in the
  track; until then the crate is a placeholder that keeps the workspace
  shape.
