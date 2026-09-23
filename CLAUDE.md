# Shonin Bots

The reference strategies: WASM components compiled from this repository and published to a Shonin platform. See `README.md` for the layout and the quickstart.

## Rules

- **This repository is public.** Never mention other repositories, local paths or private projects in anything committed.
- **A strategy's limits live in its own source**, and are listed in its manifest in the same words. There is no platform policy to defer to, and nothing here should read as if there were.
- **The SDK is a version, not a path.** `Cargo.toml` declares `shonin-strategy-sdk` by version; a local checkout is patched in through the gitignored `.cargo/config.toml` that `task patch` writes. Never commit a path dependency.
- **Dependencies live in the workspace.** Declare every version in `[workspace.dependencies]`; crates use `{ workspace = true }`.
- **A strategy is reproducible or it is nothing.** `Cargo.lock` is committed and every build is `--locked`: the build service compiles a submission twice and compares the bytes, and a project that does not build the same way twice is refused.
- **Nothing here holds a key.** A strategy signs nothing and reads no secret it did not declare. If something looks like it needs a key, the design is wrong.

## Commit guard

Every commit runs `.githooks/commit-msg` (enabled by `task setup`). It rejects a commit whose message or staged changes contain a secret, a term from the local, gitignored `.private/terms.txt`, an absolute home path, or content a reviewer model judges to break the local policy in `.private/review.md`.

Never bypass it: no `--no-verify` or `-n`, no changes to `core.hooksPath`, no `SHONIN_SKIP_REVIEW`. When it blocks a commit, fix the commit. Never commit anything under `.private/`.

## Done means

`task check`, `task lint` and `task test` all pass, and `task build` produces a component.
