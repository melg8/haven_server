# haven_server

Authoritative **RTS Chess** game server: chess meets real-time strategy.
Built with **Rust + Axum + Tokio + WebSocket**. The server is the single source
of truth — clients send inputs, the server simulates and broadcasts state.

## Features

- **Fixed-tick simulation** (10 Hz, configurable) with probabilistic movement —
  queued moves resolve on a per-tick dice roll, creating the signature
  "has it arrived yet?" tension.
- **Melee combat** — adjacent enemies lock in place and trade damage on a cooldown.
- **Artillery upgrades** — promote pawns to stationary artillery that fires
  straight-line shots with line-of-sight checks.
- **Match lifecycle** — create / join / auto-start / win-condition & timeout handling.
- **REST + WebSocket API** — REST for setup, WebSocket for real-time play.
- **Tunable via `config.toml`** — balance the game without recompiling.
- **Built-in browser client** served at `/` — two browser tabs = two players.

## Quick start

```bash
cargo run            # serves REST + WS + web client on 0.0.0.0:8080
```

Open http://localhost:8080 in two tabs, create a match in the first tab,
join from the second — the match starts automatically.

## API

| Method | Path | Description |
|--------|------|-------------|
| POST | `/match` | Create a match (optional config overrides). Returns `{ match_id, config }` |
| GET | `/match/{id}` | Public match info (status, players, tick, unit counts) |
| POST | `/match/{id}/join` | Join as player, returns `{ owner, player_token }`; auto-starts when full |
| GET | `/ws/{match_id}?token=…` | WebSocket: `select` / `move` / `upgrade` / `ping` in, `state_snapshot` / `state_delta` / `event` / `error` / `pong` out |
| GET | `/` | Embedded browser client |

See [AGENTS.md](AGENTS.md) for the full gameplay model, message formats and
agent working conventions.

## Example session

```bash
curl -s -X POST localhost:8080/match
# {"match_id":"9b6f…","config":{…}}

curl -s -X POST localhost:8080/match/9b6f…/join
# {"owner":"white","player_token":"…"}

# then connect a WebSocket client to /ws/9b6f…?token=…
```

## License

MIT — see [LICENSE](LICENSE).
