# Shonin Agents

The reference agents: WASM components compiled from this repository and published to a Shonin platform. An **agent** is the program; a **strategy** is what it is trying to do, and it is the word the platform's contracts use. See `README.md` for the layout and the quickstart.

## Rules

- **This repository is public.** Never mention other repositories, local paths or private projects in anything committed.
- **An agent's limits live in its own source**, as constants in `src/config.rs`, and are claimed in its manifest in the same words. There is no platform policy to defer to, and nothing here should read as if there were.
- **The SDK comes from crates.io.** `Cargo.toml` declares `shonin-strategy-sdk` by version and `Cargo.lock` takes it from crates.io, the only form the build service accepts. Never patch it to a path: a change the agents need in the SDK is published as a new SDK version first.
- **Dependencies live in the workspace.** Declare every version in `[workspace.dependencies]`; crates use `{ workspace = true }`.
- **An agent is reproducible or it is nothing.** `Cargo.lock` is committed and every build is `--locked`, so the bundle the build service signs is the one anybody else gets from the same source. Nothing here may read a clock, the network or a random number while it compiles: the reviewer reports that against the agent's reproducibility rating.
- **Nothing here holds a key.** An agent signs nothing and reads no secret it did not declare. If something looks like it needs a key, the design is wrong.

## Commit guard

Every commit runs `.githooks/commit-msg` (enabled by `task setup`). It rejects a commit whose message or staged changes contain a secret, a term from the local, gitignored `.private/terms.txt`, an absolute home path, or content a reviewer model judges to break the local policy in `.private/review.md`.

Never bypass it: no `--no-verify` or `-n`, no changes to `core.hooksPath`, no `SHONIN_SKIP_REVIEW`. When it blocks a commit, fix the commit. Never commit anything under `.private/`.

## Done means

`task check`, `task lint` and `task test` all pass, and `task build` produces a component.
