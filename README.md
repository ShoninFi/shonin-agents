# Shonin Agents

The reference strategies. Each one is a WASM component compiled from the Rust in this repository, published to a Shonin platform, and run by an attested executor that alone holds its vault's keys.

A strategy here is an ordinary Rust library that implements one long-lived async function. It holds no key, signs nothing, and reaches the outside world only through the imports the `shonin:strategy` world gives it. Whatever it decides, it can only ever trade its vault's own accounts.

## Prerequisites

- [Rust](https://rustup.rs). The toolchain is pinned in `rust-toolchain.toml`, with the `wasm32-wasip2` target a strategy compiles to.
- [Task](https://taskfile.dev)
- For the commit guard: [gitleaks](https://github.com/gitleaks/gitleaks), [jq](https://jqlang.org) and the [Claude Code](https://claude.com/claude-code) CLI.

## Quickstart

```bash
task setup    # toolchain, commit guard, and the SDK pointed at a local platform
task build    # build every agent as a component
task check    # compile natively and as a component
task lint     # rustfmt --check + clippy -D warnings
task test     # native tests
task dev      # publish funding-basis to a platform running locally
```

Copy `.env.example` to `.env` first, and set `SHONIN_PLATFORM_DIR` to a checkout of the platform. `task patch` writes a gitignored `.cargo/config.toml` that resolves the SDK from there while the SDK and the strategies are moving together; without it, the SDK resolves from the registry by the version in `Cargo.toml`, which is what the build service does. `task dev` (or `task publish AGENT=<name>`) publishes through that checkout, to the platform its `task dev` is running: the platform's seed creates the vault and submits the source, the same flow its web app uses.

## The agents

| Agent | What it does |
| --- | --- |
| `funding-basis` | Holds the spot asset, shorts the perpetual against it, and collects the funding between them. The position has no view on the price, so what is left is the carry. |

## Layout

```text
packages/
  shared/                helpers every agent shares: reading its config, sizing a position
  agents/funding-basis/  the agent: its manifest, and the code the reviewer reads
```

Each agent is a directory with a `Cargo.toml`, a `shonin.toml` and its source. The manifest is what the author declares: what the strategy is, the limits it sets itself, the venues and accounts it trades, and the compute it asks for. The platform hashes it, and a vault's page shows what it said.

## Limits live in the code

There is no platform risk policy, and there is nothing to configure that would add one. A strategy's limits are written in its own source, listed in its manifest in the same words, and read by the reviewer. A depositor comparing the page against the code is comparing two descriptions of one thing.

## License

Apache-2.0. These are examples: read them, copy them, publish your own.
