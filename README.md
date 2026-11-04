# mc-agents

AI Minecraft bots that write their own skills, keep only what they save by
sleeping, and forget what they did not save when they die.

- **Body:** [azalea](https://github.com/azalea-rs/azalea) 0.15.1 (Minecraft Java 1.21.11).
- **Brain:** the [goose GDK](https://github.com/aaif-goose/goose) as a Rust library
  (`goose-agent`, `goose-providers`). We assemble our own agent state machine and
  run our own event loop around it. We do not use the goose CLI or desktop app.

## Setup

1. Enter the dev shell: `direnv allow` (or `nix develop`). It provides Rust
   nightly (pinned in `rust-toolchain.toml`) and JDK 21.
2. Copy `.env.example` to `.env` and fill it in. By default the bots use the
   local Maple proxy (`MAPLE_BASE_URL`, `MAPLE_MODEL`) and need no key. To use a
   hosted API, set `llm.kind`, `llm.base_url` and `llm.api_key_env` in
   `config.toml` and put the key in `.env`. Keys never go in `config.toml`.

## Check the brain (milestone 1)

```sh
cargo run --bin gdk-hello            # fast model
cargo run --bin gdk-hello -- --strong
```

This runs one GDK agent turn in-process with one custom Rust tool
(`look_around`, canned data). No Minecraft server is needed.

## Dependency note

`vendor/azalea-crypto` and `vendor/azalea-auth` are the published azalea
0.15.1 crates with their RustCrypto release-candidate pins moved to the final
`rand`/`rand_core` 0.10 releases and `rsa` 0.10.0-rc.20. Without this, Cargo
cannot build azalea and goose in one binary. The only code change is one
renamed trait import (`TryRngCore` → `TryRng`).
