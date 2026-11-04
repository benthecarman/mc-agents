# mc-agents

AI Minecraft bots that write their own skills, keep only what they save by
sleeping, and forget what they did not save when they die.

- **Body:** [azalea](https://github.com/azalea-rs/azalea) 0.15.1 (Minecraft Java 1.21.11).
- **Brain:** the [goose GDK](https://github.com/aaif-goose/goose) as a Rust library
  (`goose-agent`, `goose-providers`). We assemble our own agent state machine and
  run our own event loop around it. We do not use the goose CLI or desktop app.
- **Skills:** Rhai scripts that the bots write, test and register as new tools.
- **Memory:** one git repository per bot. Sleeping commits; dying throws away
  everything that was not committed.

## Setup

1. **Dev shell.** Run `direnv allow` (or `nix develop`). It gives you Rust
   nightly (pinned in `rust-toolchain.toml`) and JDK 21.
2. **Secrets and console.** Copy `.env.example` to `.env`. direnv loads it.
   - By default the bots use the local Maple proxy (`MAPLE_BASE_URL`,
     `MAPLE_MODEL`), which needs no key. For a hosted API, set `llm.kind`,
     `llm.base_url` and `llm.api_key_env` in `config.toml`, and put the key in
     `.env`. Keys never go in `config.toml`, and the bots never log them.
   - `MC_CONSOLE` gives the bots' launcher and the benchmark access to the
     server console, for gamerules, the day length and benchmark setup. The
     default is the pipe that `start.sh` creates.
3. **Server.** Start it with `mc-server/start.sh` (put `server.jar` for 1.21.11
   in `mc-server/` first). It is a local server with `online-mode=false`.
   You can type commands in its terminal as usual.
4. **Bots.** Start them with `cargo run --release`. Every bot in `config.toml`
   joins. When `MC_CONSOLE` is set, the launcher first sets
   `players_sleeping_percentage 0` (one sleeping bot skips the night) and
   `keep_inventory false`, then shortens the day to `server.day_minutes`.
   (1.21.11 uses snake_case gamerule names.)
5. **Join.** In Prism Launcher, make a 1.21.11 instance and launch it. Open
   Multiplayer → Direct Connection, and type `localhost`. An offline account
   works because the server has `online-mode=false`.

Logs are in color on the console and as plain text in `logs/all.log` and
`logs/<bot>.log`. Each bot's memory is in `memory/<bot>/`. Use
`git -C memory/Haley log -p` to read what Haley learned each night.

## The bots

`config.toml` has a bot for each person on the
[Spiral team](https://spiral.xyz/team/) except Ben, who joins as a player, with
a personality and a risk appetite drawn from their entry on that page, plus
Zeus, the god. Delete or add `[[bots]]` entries to change who joins. Bots
join 1.5 s apart, so all 21 take about half a minute. If a teammate joins
the server with the same name as a bot, one of the two gets kicked, so use
another name for yourself (for example `benthecarman`).

## Talking to the bots

- Say something with a bot's name in it (a whole word, so `Mat` and `Matt`
  are different bots): `Haley, make a wooden pickaxe`. Chat
  that names no bot is ignored by every bot. Use `all` to talk to every
  bot.
- `Haley, come here` works: bots have `where_is(player)` and
  `go_to_player(player)`. They can see any player in the area that any of
  the bots has loaded (about 150 blocks around each bot).
- A message that starts with a bot's name (`Zeus, ...` or `@Zeus ...`) is
  for that bot only, even if it names others.
- Test commands skip the model:
  - `!Haley mine log 3` and `!all look_around` run a tool directly.
  - `!Haley memory` shows saved and unsaved memory.
  - `!Haley dream` consolidates memory now, without a bed.
  - `!Haley forget` acts out a death for memory: unsaved skills are lost.
  - `!Haley reset` drops the current goal.

## The god bot

A bot with `role = "god"` in `config.toml` (Zeus by default) does not live in
the world. It is in creative mode and rules the world through the server
console, so it needs `MC_CONSOLE`. Call it by name:

- `Zeus, make it thunder` / `Zeus, make it night`
- `Zeus, teleport me to Jasper` / `Zeus, give Haley a bed`
- `@Zeus smite Jasper` (lightning, for those who deserve it)

Players listed in the god's `obeys` (in `config.toml`) are its masters: it
carries out whatever they command, smiting included, though it may grumble.
Everyone else gets what the god thinks is reasonable.

If you set `limits.god_whim_secs`, it also acts unasked that often (the
default, 0, means only when called). Its
powers are `set_weather`, `set_time`, `smite`, `teleport`, `give` and
`players`. Each one fills a fixed command template, and its arguments are
checked: players must be online, and items and coordinates must be valid.
The model never writes a console command. The god uses the same daily
model-call budget as the other bots.

## How it works

```
azalea events ─► runtime ─► BotEvent queue ─► agent loop
                                               ├─ reflexes (no model): flee, fight back, escape water, eat
                                               ├─ needs a thought? fast or strong model, one GDK turn
                                               │    tools: look_around, inventory, say,
                                               │           submit_plan / set_goal / keep_plan / learn_skill
                                               └─ executor runs the plan, step by step, on its own
```

- **Event loop** (`src/agent/mod.rs`). The bot thinks only when something
  happens: chat, a failed step, a finished plan, and, while it works on a
  goal, dusk, night or a full inventory. Reflexes (flee, fight back, escape
  water, eat) act without a model. Chat and routine events go to the fast
  model. Repeated failures and goal planning go to the strong model. By
  default a bot only does what players ask. Set `limits.idle_goal_secs` to
  let a bot with no goal pick one that fits its personality.
- **Chat.** A bot posts in chat only while a player is talking to it: for
  two minutes after a player names it, and once more to report how the
  player's task went. Everything else it would say (dusk decisions, dream
  summaries, death notices) goes to the console, marked `(not said)`. A
  bot that names another bot counts as talking to it, so bots can ask each
  other (and Zeus) for help. Each answers another bot at most once every
  45 seconds, so they can't loop.
- **Snapshots.** Every turn gets long-term memory, working memory, recent
  events, the world around the bot, the goal and the plan. It also gets a
  **stakes** line: unsaved skills and lessons, time until dark, distance to
  the nearest bed, and HP. With the bot's risk appetite, this line decides
  whether it races home at dusk or pushes on.
- **Plans** (`src/agent/plan.rs`, `executor.rs`). The model submits a list of
  tool calls. They are checked inside the turn: unknown tools or missing
  arguments go back to the model at once. The executor runs the steps. If a
  step fails, its error goes back to the model.
- **Skills** (`src/skills.rs`, `src/agent/learn.rs`). When a goal needs a
  reusable ability, the bot calls `learn_skill`. A writer turn writes a Rhai
  function and submits it with test arguments and a check (for example
  "gain 4 logs"). The code is compiled, run once in the sandbox, and checked.
  If it passes, it becomes a tool for plans and for other skills. It stays
  unsaved until the bot sleeps. Retries are capped by
  `limits.skill_write_retries`.
- **Sandbox.** Skills run only in Rhai, with only the bot API (the same tools
  the model has). They have no filesystem, shell, network, `eval` or
  `import`. Operations, call depth, string and array sizes, and run time
  (240 s) are limited.
- **Sleep and dreams** (`src/agent/dream.rs`, `src/memory.rs`). When a bot
  gets into a bed, it dreams. One model turn reads its saved and unsaved
  skills, rules, lessons, places, the day's events and other bots' deaths
  (nightmares). It merges duplicate skills, fixes or drops broken ones, and
  writes rules and a journal entry. The result is committed to
  `memory/<bot>`, with the journal as the commit body. The console shows
  the journal and a one-line summary.
- **Death.** Working memory is deleted (`git checkout && git clean`). Unsaved
  skills stop being tools. The console shows what it lost. Its death
  becomes a nightmare for the other bots' next dream.
- **Books** (`write_book`, `read_book`). A bot can write its rules and
  skills into a book and quill and put it in a chest. Any bot can read the
  books in nearby chests. Skills read from a book join working memory, so
  the reader must still sleep to keep them.
- **Hardcore** (`limits.hardcore = true`). Death also wipes long-term memory
  and starts a new generation. Only books survive. A later generation is
  told to look for its ancestors' books.
- **Budgets.** `limits.llm_calls_per_day` caps model calls per bot per
  in-game day. A quarter of the budget is kept for chat and emergencies.
  `limits.inferences_per_turn` and `limits.turn_timeout_secs` cap each turn.

## Demo script

Before the demo, set `server.day_minutes` to about 6 so that dusk comes
sooner. Start the server and `cargo run --release`, then join the game.

1. **A multi-step task.** Say `Haley, make a wooden pickaxe`. Watch the
   console: Haley sets a goal, plans (logs → planks → sticks → crafting table →
   pickaxe), runs the plan and replans when a step fails.
2. **Skill writing.** Say `Jasper, learn a skill to gather wood, then get 8
   logs`. The console shows the Rhai code, the test run and the check. In
   chat, Jasper reports: "I learned a new skill: gather_wood. I must sleep to
   remember it." `!Jasper memory` lists it as unsaved.
3. **Night, far from home.** Give Jasper a bed (`/give Jasper white_bed`), send
   Jasper away (`Jasper, go mine coal`), and type `/time set 12000`. At dusk
   the console shows whether a bot with a task races back or pushes on.
   Careful Haley (risk 0.2) goes to bed. Jasper (risk 0.95), who flaunts
   safety protocols, pushes on. Ask one of them why (`Jasper, why?`) to
   hear the reason in chat.
4. **The dream journal.** When a bot sleeps, the console prints its dream
   journal (tagged `dream`) and a "Dreamt: ..." summary. Show the commit:
   `git -C memory/Jasper log -1 -p`.
5. **Death with unsaved skills.** Teach a bot another skill, then kill it
   before it sleeps (`/kill Jasper`). The console shows "I died and forgot 1
   skill(s) (...)". `!Jasper memory` shows that the saved skills are still there and the
   new one is gone. The other bots dream about the death the next night.
6. **Inheritance (optional).** `/give Haley writable_book` and a chest. Say
   `!Haley place chest <x> <y> <z>` and `!Haley write_book all`. Then
   `!Jasper read_book`: Jasper gets Haley's skills and rules, unsaved.

## Benchmark and improver

`bench/scenarios.toml` defines timed scenarios: gather logs, place a crafting
table, craft a pickaxe, mine cobblestone, make tools from nothing, and sleep
at dusk. The benchmark bot plays them through the real agent loop. Each pass
scores 100, plus up to 50 for speed, minus 2 per model call.

```sh
# stop the demo bots first: they would answer the benchmark bot's chat
cargo run --release --bin bench                      # all scenarios
cargo run --release --bin bench -- --only wood,sleep
```

The report goes to `bench/results/latest.json`. The scenario coordinates are
for the demo world, so change the `tp` commands to suit your world. The
benchmark sets `difficulty peaceful` while it runs and `easy` after.

The **improver** is a second GDK agent that edits this crate to raise the
score:

```sh
cargo run --release --bin improver -- --generations 3
cargo run --release --bin improver -- --generations 5 --bench-args "--only wood,crafting_table"
```

- Its only tools are `read_file` and `write_file` (inside `src/`, but never
  `src/bin/`, where the benchmark and the improver live), `cargo_build`,
  `run_bench`, `git_diff`, `git_commit` and `git_revert`. It has no shell.
  Each tool runs one fixed command.
- It works on a new `improver/<date>` branch, and it refuses to start if
  `src/` has uncommitted changes.
- `git_commit` refuses unless the benchmark ran on exactly the current
  change and beat the best score. Changes that were not committed are
  reverted after each generation. So the branch holds only commits that
  raised the score. Review the branch, then merge it.
- Each generation's score goes to `bench/scores.csv`.

Model output varies, so a score can change between runs of the same code.
To make each comparison more reliable, run fewer scenarios per benchmark,
or run more generations.

## Guardrails

- API keys stay in `.env`, which is git-ignored. They are never logged.
- Generated skill code runs only in the Rhai sandbox. It never runs in a
  shell and is never compiled as Rust.
- The improver's file tools are limited to `src/` (and `src/bin/` is
  read-only). It has no general shell.
- There are caps on model calls per bot per in-game day, on model calls per
  turn, on turn time, and on skill-write retries.
- The god bot can run only its fixed command templates.
- Use a local test server only.

## Dependency note

`vendor/azalea-crypto` and `vendor/azalea-auth` are the published azalea
0.15.1 crates with their RustCrypto release-candidate pins moved to the final
`rand`/`rand_core` 0.10 releases and `rsa` 0.10.0-rc.20. Without this, Cargo
cannot build azalea and goose in one binary. The only code change is one
renamed trait import (`TryRngCore` → `TryRng`).

## Other binaries

- `cargo run --bin gdk-hello` runs one GDK turn with one custom tool and
  canned data. It needs no Minecraft server. Use it to check the model setup.
- `cargo run --bin mc-say -- "Haley, hello" "!Haley memory"` joins as `Tester`,
  says each line, and prints the chat that follows. It is for tests without a
  game client.
