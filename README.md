# Shonin Agents

The reference agents. Each one is a WASM component compiled from the Rust in this repository, published to a Shonin platform, and run by an attested executor that alone holds its vault's keys.

An agent here is an ordinary Rust library that implements one long-lived async function. It holds no key, signs nothing, and reaches the outside world only through the imports the `shonin:agent` world gives it. Whatever it decides, it can only ever trade its vault's own accounts.

## Prerequisites

- [Rust](https://rustup.rs). The toolchain is pinned in `rust-toolchain.toml`, with the `wasm32-wasip2` target an agent compiles to.
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

The SDK comes from [crates.io](https://crates.io/crates/shonin-strategy-sdk), as the build service requires. Copy `.env.example` to `.env` first, and set `SHONIN_PLATFORM_DIR` to a checkout of the platform. `task dev` (or `task publish AGENT=<name>`) publishes through that checkout, to the platform its `task dev` is running: the platform's seed uploads the source, pays to publish it, and activates it once it is reviewed and built, the same flow its web app uses.

## The agents

| Agent | What it does |
| --- | --- |
| `funding-basis` | Holds the spot asset, shorts the perpetual against it, and collects the funding between them. The position has no view on the price, so what is left is the carry. Published with a vault on Base, on Hyperliquid (`funding-basis/hyperliquid`) or on Solana (`funding-basis/solana`). |
| `mm-quoter` | Quotes both sides of one perpetual on Hyperliquid, resting orders only, leaning against its inventory. `mm-quoter/solana` is the same business on Solana: one band of concentrated liquidity around the oracle's price in Orca's SOL pool, set again when the price nears its edge. |

## Layout

```text
packages/
  agents/funding-basis/  the agent: its manifest, its settings in src/config.rs, and the code the reviewer reads
  agents/mm-quoter/      a market maker on Hyperliquid, and on Orca (mm-quoter/solana)
```

Each agent is a directory with a `Cargo.toml`, a `shonin.toml` and its source. The manifest only declares, for the platform: the chain its vault lives on, the author's description and claims, and the drivers it trades through. Its own settings are constants in `src/config.rs`, and its allowances are set on the vault when it is activated. The platform hashes the manifest, and a vault's page shows what it said.

## Limits live in the code

There is no platform risk policy, and there is nothing to configure that would add one. An agent's limits are written in its own source, claimed in its manifest in the same words, and checked against the code by the reviewer. A depositor comparing the page against the code is comparing two descriptions of one thing.

## License

Apache-2.0. These are examples: read them, copy them, publish your own.
