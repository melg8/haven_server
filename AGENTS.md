# AGENTS.md — haven_server

## What this project is

`haven_server` is the **authoritative backend for RTS Chess** — a hybrid of classic
chess and real-time strategy. The server is the single source of truth: clients send
inputs over WebSocket, the server simulates the game on a fixed tick loop and
broadcasts state snapshots/deltas back to clients.

- **Language:** Rust (stable)
- **Framework:** Axum (HTTP + WebSocket), Tokio async runtime, Serde serialization
- **State:** In-memory, server-authoritative, no external database
- **Client:** any WebSocket client; a built-in browser client is served at `/`

## Execution environment (how to set up from scratch)

The reference environment is a Linux x86_64 box with git and network access.

1. **Install Rust** (if not present):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
   source "$HOME/.cargo/env"
   ```
   Verified toolchain: `rustc 1.98.1` / `cargo 1.98.1`. Any recent stable works.

2. **Clone & build:**
   ```bash
   git clone https://github.com/melg8/haven_server.git
   cd haven_server
   cargo build            # debug build
   cargo build --release  # optimized single binary
   ```

3. **Run:**
   ```bash
   cargo run
   # or, with a custom config file:
   HAVEN_CONFIG=/path/to/config.toml cargo run --release
   ```
   The server listens on `0.0.0.0:8080` by default (see `config.toml`).

4. **Test:**
   ```bash
   cargo test          # unit + integration tests
   cargo clippy        # lints (CI hygiene)
   cargo fmt --check   # formatting
   ```

## Project layout

```
haven_server/
├── AGENTS.md            # this file — read it first
├── Cargo.toml
├── config.toml          # all gameplay tuning lives here (no recompile needed)
├── src/
│   ├── main.rs          # bootstrap: tracing, config, router, graceful shutdown
│   ├── config.rs        # serde config structs, loaded from config.toml
│   ├── game_state.rs    # GameState, Unit, Board, Tile, snapshots & deltas
│   ├── simulation.rs    # fixed-tick simulation: movement, melee, artillery, deaths, win check
│   ├── input.rs         # client command validation (ownership, range, legality)
│   ├── network.rs       # ClientMessage / ServerMessage / GameEvent wire types + WS handler
│   ├── match_manager.rs # match creation, join, lifecycle, per-match tick task
│   └── api.rs           # REST handlers
├── static/
│   └── index.html       # embedded browser client (served at /)
└── tests/
    └── rest_api.rs      # end-to-end REST tests via tower::ServiceExt::oneshot
```

> Note: the module for match lifecycle is `match_manager.rs`, not `match.rs`,
> because `match` is a reserved keyword in Rust.

## Git rules for agents

- **Branch:** all work happens on `main`.
- **Commit identity:** commits are made as
  `melg8 <melg8@users.noreply.github.com>` — configure once per clone:
  ```bash
  git config user.name "melg8"
  git config user.email "melg8@users.noreply.github.com"
  ```
- **Push:** push significant, self-contained changes as soon as they are ready
  (`git push origin main`). Do not push broken states: `cargo build && cargo test`
  must pass first.
- **Secrets:** the GitHub token, if used for pushing, lives ONLY in the git remote
  URL or a credential helper. **Never commit tokens** to any tracked file.
- **Commit style:** short imperative subject lines
  (`feat: …`, `fix: …`, `test: …`, `docs: …`, `chore: …`).

## Gameplay model (quick reference)

- 8x8 board; white pawns start on rank 2 (`y=1`), black on rank 7 (`y=6`).
- Tick loop runs at `simulation.tick_rate` Hz (default 10).
- **Movement is probabilistic:** a queued pawn move resolves each tick with
  per-tick probability `1 - (1 - move_probability_per_second)^(1/tick_rate)`
  (30%/s at 10 Hz ≈ 3.5%/tick). This creates the "has it arrived yet?" tension.
- Pawns step 1 tile in any of the 8 directions (`pawn_move_range`).
- Adjacent enemy units (Chebyshev distance 1) **lock in melee** automatically;
  both deal `melee_damage` every `melee_cooldown_secs` until one dies.
- Pawns can be **upgraded** to artillery (stationary). Artillery fires
  orthogonally up to `artillery_range`, first unit/wall in line blocks LOS.
- Match ends when one side has no units left, or on timeout (higher alive count
  wins, equal counts draw).

## API summary

REST:
- `POST /match` — create match, optional per-match config overrides in body. Returns `{ match_id, config }`.
- `GET  /match/{id}` — public match info (status, players, tick, unit counts).
- `POST /match/{id}/join` — join; body may carry `{"token": "..."}` to rejoin. Returns `{ owner, player_token }`. Auto-starts the match when both sides are taken.

WebSocket:
- `GET /ws/{match_id}?token=<player_token>` — connect as player (or spectator without token).
- Client → server: `{"type":"select","unit_id":1}`, `{"type":"move","unit_id":1,"target":{"x":3,"y":2}}`, `{"type":"upgrade","unit_id":1,"artillery_type":"cannon"}`, `{"type":"ping"}`.
- Server → client: `welcome`, `state_snapshot`, `state_delta`, `event`, `error`, `pong`.
- On connect and on reconnect the client always receives a full `state_snapshot`.

## Known simplifications / roadmap

- In-memory state only (lost on restart) — Redis persistence is future work.
- Walls exist in the data model but no walls are generated in MVP.
- Single artillery type (`cannon`); per-type stats table is future work.
- Token-in-query WS auth; header-based auth is future work.
- Bevy/WASM rich client is planned separately (`frontend plan`); the embedded
  browser client at `/` covers the full MVP interaction set today.
