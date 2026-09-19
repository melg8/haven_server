//! Match lifecycle management.
//!
//! `MatchManager` owns the registry of live matches: creation, joining, GC of
//! stale/finished matches, and the per-match tokio task that runs the
//! fixed-timestep simulation loop and broadcasts to subscribers.

use crate::config::{BroadcastMode, Config};
use crate::game_state::{GameState, MatchStatus, Owner};
use crate::input;
use crate::network::{InputItem, Role, ServerMessage, BROADCAST_CHANNEL_SIZE, INPUT_QUEUE_SIZE};
use crate::simulation;
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};

/// Shared per-match state protected by an async mutex.
pub struct MatchInner {
    pub cfg: Config,
    pub state: GameState,
    pub white_token: Option<String>,
    pub black_token: Option<String>,
    pub created_at: Instant,
    pub finished_at: Option<Instant>,
    pub rng: StdRng,
}

/// Cheap handle to a match: clone it into handlers and tasks.
#[derive(Clone)]
pub struct MatchHandle {
    pub id: String,
    pub inner: Arc<Mutex<MatchInner>>,
    pub broadcast: broadcast::Sender<Arc<ServerMessage>>,
    pub input_tx: mpsc::Sender<InputItem>,
}

impl MatchHandle {
    /// Resolve a player token to a role. Unknown/absent tokens are spectators.
    pub async fn role_for_token(&self, token: &str) -> Role {
        let inner = self.inner.lock().await;
        if inner.white_token.as_deref() == Some(token) {
            Role::Player(Owner::White)
        } else if inner.black_token.as_deref() == Some(token) {
            Role::Player(Owner::Black)
        } else {
            Role::Spectator
        }
    }

    pub async fn current_state(&self) -> GameState {
        self.inner.lock().await.state.clone()
    }
}

/// REST create request: optional per-match config overrides.
#[derive(Debug, Default, Deserialize)]
pub struct CreateMatchRequest {
    pub board: Option<crate::config::BoardConfig>,
    pub simulation: Option<crate::config::SimulationConfig>,
}

/// REST create response.
#[derive(Debug, Serialize)]
pub struct CreateMatchResponse {
    pub match_id: String,
    pub config: Config,
}

/// REST join response.
#[derive(Debug, Serialize)]
pub struct JoinResponse {
    pub owner: Owner,
    pub player_token: String,
}

/// REST join request (optional token for rejoin).
#[derive(Debug, Default, Deserialize)]
pub struct JoinRequest {
    pub token: Option<String>,
}

/// Public match info for `GET /match/{id}`.
#[derive(Debug, Serialize)]
pub struct MatchInfo {
    pub match_id: String,
    pub status: MatchStatus,
    pub tick: u64,
    pub players: PlayersInfo,
    pub units_alive: UnitsAlive,
    pub winner: Option<Owner>,
    pub end_reason: Option<crate::game_state::EndReason>,
}

#[derive(Debug, Serialize)]
pub struct PlayersInfo {
    pub white: bool,
    pub black: bool,
}

#[derive(Debug, Serialize)]
pub struct UnitsAlive {
    pub white: usize,
    pub black: usize,
}

/// Registry of live matches plus the server-wide default config.
pub struct MatchManager {
    pub config: Config,
    matches: RwLock<HashMap<String, MatchHandle>>,
}

impl MatchManager {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            matches: RwLock::new(HashMap::new()),
        })
    }

    pub async fn get(&self, id: &str) -> Option<MatchHandle> {
        self.matches.read().await.get(id).cloned()
    }

    pub async fn live_count(&self) -> usize {
        self.matches.read().await.len()
    }

    /// Create a match and spawn its tick task.
    pub async fn create(&self, req: CreateMatchRequest) -> Result<CreateMatchResponse, String> {
        {
            let matches = self.matches.read().await;
            if matches.len() >= self.config.server.max_matches {
                return Err(format!(
                    "server is at max_matches ({}); try again later",
                    self.config.server.max_matches
                ));
            }
        }

        let mut cfg = self.config.clone();
        if let Some(board) = req.board {
            cfg.board = board;
        }
        if let Some(simulation) = req.simulation {
            cfg.simulation = simulation;
        }
        cfg.validate()?;

        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let state = GameState::new(&cfg.board, cfg.simulation.unit_hp);
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CHANNEL_SIZE);
        let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE_SIZE);

        let handle = MatchHandle {
            id: id.clone(),
            inner: Arc::new(Mutex::new(MatchInner {
                cfg: cfg.clone(),
                state,
                white_token: None,
                black_token: None,
                created_at: Instant::now(),
                finished_at: None,
                rng: StdRng::from_os_rng(),
            })),
            broadcast: broadcast_tx,
            input_tx,
        };

        self.matches
            .write()
            .await
            .insert(id.clone(), handle.clone());
        tokio::spawn(run_match_task(handle.clone(), input_rx));

        tracing::info!(match_id = %id, "match created");
        Ok(CreateMatchResponse {
            match_id: id,
            config: cfg,
        })
    }

    /// Join a match as a player. A second join starts the match.
    /// Passing an existing `player_token` rejoins the same side (reconnect).
    pub async fn join(&self, id: &str, req: JoinRequest) -> Result<JoinResponse, String> {
        let handle = self.get(id).await.ok_or("match not found")?;
        let mut inner = handle.inner.lock().await;

        // Rejoin with a known token.
        if let Some(token) = req.token.as_deref() {
            if inner.white_token.as_deref() == Some(token) {
                return Ok(JoinResponse {
                    owner: Owner::White,
                    player_token: token.to_string(),
                });
            }
            if inner.black_token.as_deref() == Some(token) {
                return Ok(JoinResponse {
                    owner: Owner::Black,
                    player_token: token.to_string(),
                });
            }
        }

        let owner = if inner.white_token.is_none() {
            Owner::White
        } else if inner.black_token.is_none() {
            Owner::Black
        } else {
            return Err("match is full".into());
        };

        let token = uuid::Uuid::new_v4().to_string();
        match owner {
            Owner::White => inner.white_token = Some(token.clone()),
            Owner::Black => inner.black_token = Some(token.clone()),
        }

        // Auto-start once both sides are taken.
        if inner.white_token.is_some() && inner.black_token.is_some() {
            inner.state.status = MatchStatus::Active;
            drop(inner);

            let _ = handle.broadcast.send(Arc::new(ServerMessage::Event {
                event: simulation::GameEvent::MatchStarted,
            }));
            let state = handle.current_state().await;
            let _ = handle
                .broadcast
                .send(Arc::new(ServerMessage::StateSnapshot { state }));
            tracing::info!(match_id = %id, "match started");
        }

        tracing::info!(match_id = %id, %owner, "player joined");
        Ok(JoinResponse {
            owner,
            player_token: token,
        })
    }

    /// Public info snapshot for REST.
    pub async fn info(&self, id: &str) -> Option<MatchInfo> {
        let handle = self.get(id).await?;
        let inner = handle.inner.lock().await;
        Some(MatchInfo {
            match_id: handle.id.clone(),
            status: inner.state.status,
            tick: inner.state.tick,
            players: PlayersInfo {
                white: inner.white_token.is_some(),
                black: inner.black_token.is_some(),
            },
            units_alive: UnitsAlive {
                white: inner.state.alive_count(Owner::White),
                black: inner.state.alive_count(Owner::Black),
            },
            winner: inner.state.winner,
            end_reason: inner.state.end_reason.clone(),
        })
    }

    /// Remove expired matches: unstarted lobbies and old finished games.
    pub async fn collect_garbage(&self) {
        let now = Instant::now();
        let lobby_ttl = Duration::from_secs_f64(self.config.lifecycle.lobby_ttl_secs);
        let finished_ttl = Duration::from_secs_f64(self.config.lifecycle.finished_ttl_secs);

        let mut expired: Vec<String> = Vec::new();
        {
            let matches = self.matches.read().await;
            for (id, handle) in matches.iter() {
                // Peek cheaply: try_lock to avoid blocking on live games.
                if let Ok(inner) = handle.inner.try_lock() {
                    match inner.state.status {
                        MatchStatus::Lobby => {
                            if now.duration_since(inner.created_at) > lobby_ttl {
                                expired.push(id.clone());
                            }
                        }
                        MatchStatus::Finished => {
                            if let Some(t) = inner.finished_at {
                                if now.duration_since(t) > finished_ttl {
                                    expired.push(id.clone());
                                }
                            }
                        }
                        MatchStatus::Active => {}
                    }
                }
            }
        }
        if expired.is_empty() {
            return;
        }
        let mut matches = self.matches.write().await;
        for id in &expired {
            matches.remove(id);
            tracing::info!(match_id = %id, "match garbage collected");
        }
    }
}

/// Spawn the periodic GC task.
pub fn spawn_gc_task(manager: Arc<MatchManager>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            manager.collect_garbage().await;
        }
    });
}

/// The per-match simulation loop.
async fn run_match_task(handle: MatchHandle, mut input_rx: mpsc::Receiver<InputItem>) {
    let tick_period = {
        let inner = handle.inner.lock().await;
        Duration::from_secs_f64(1.0 / inner.cfg.simulation.tick_rate as f64)
    };

    let mut interval = tokio::time::interval(tick_period);
    // Do not burst-catch up after long locks; skip missed ticks instead.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        let mut inner = handle.inner.lock().await;
        match inner.state.status {
            MatchStatus::Active => {
                // Re-borrow the guard contents once so disjoint field borrows
                // (&mut state, &cfg, &mut rng) are visible to the compiler.
                let inner: &mut MatchInner = &mut inner;

                // 1. Drain queued client inputs.
                while let Ok(item) = input_rx.try_recv() {
                    handle_input(&handle, inner, item);
                }

                // 2..7. Advance the world one tick.
                let events =
                    simulation::tick(&mut inner.state, &inner.cfg.simulation, &mut inner.rng);

                // 8. Broadcast state + events.
                match inner.cfg.simulation.broadcast_mode {
                    BroadcastMode::Snapshot => {
                        let msg = Arc::new(ServerMessage::StateSnapshot {
                            state: inner.state.clone(),
                        });
                        let _ = handle.broadcast.send(msg);
                    }
                    BroadcastMode::Delta => {
                        // Delta mode: reuse the snapshot machinery but strip
                        // unchanged units. For MVP correctness we ship a delta
                        // built from the events that touched units plus status.
                        let touched = unit_ids_touched(&events);
                        let delta = crate::game_state::StateDelta {
                            tick: inner.state.tick,
                            status: inner.state.status,
                            winner: inner.state.winner,
                            end_reason: inner.state.end_reason.clone(),
                            updated: inner
                                .state
                                .units
                                .values()
                                .filter(|u| touched.contains(&u.id))
                                .cloned()
                                .collect(),
                            removed: events
                                .iter()
                                .filter_map(|e| match e {
                                    simulation::GameEvent::UnitDied { unit, .. } => Some(*unit),
                                    _ => None,
                                })
                                .collect(),
                        };
                        let msg = Arc::new(ServerMessage::StateDelta { delta });
                        let _ = handle.broadcast.send(msg);
                    }
                }
                for ev in events {
                    let msg = Arc::new(ServerMessage::Event { event: ev });
                    let _ = handle.broadcast.send(msg);
                }

                // End of match: final full snapshot, then stop the loop.
                if inner.state.status == MatchStatus::Finished {
                    let msg = Arc::new(ServerMessage::StateSnapshot {
                        state: inner.state.clone(),
                    });
                    let _ = handle.broadcast.send(msg);
                    inner.finished_at = Some(Instant::now());
                    tracing::info!(match_id = %handle.id, "match finished");
                    break; // inner (the guard) is released here
                }
            }
            MatchStatus::Lobby => {
                // Keep the input queue drained; nothing is valid before start.
                while let Ok(_item) = input_rx.try_recv() {
                    let _ = handle.broadcast.send(Arc::new(ServerMessage::Error {
                        to: None,
                        code: "match_not_active".into(),
                        detail: "match has not started yet".into(),
                    }));
                }
                if inner.created_at.elapsed()
                    > Duration::from_secs_f64(inner.cfg.lifecycle.lobby_ttl_secs)
                {
                    tracing::info!(match_id = %handle.id, "lobby expired");
                    break;
                }
            }
            MatchStatus::Finished => {
                // Task loop breaks right after finishing; this arm is a safety net.
                break;
            }
        }
    }
}

/// Validate + apply one client input while the match lock is held.
fn handle_input(handle: &MatchHandle, inner: &mut MatchInner, item: InputItem) {
    let role = item.from;
    let owner = match role.owner() {
        Some(o) => o,
        None => {
            let _ = handle.broadcast.send(Arc::new(ServerMessage::Error {
                to: None,
                code: "not_a_player".into(),
                detail: "spectators cannot send commands".into(),
            }));
            return;
        }
    };
    let mut events = Vec::new();
    match input::handle_message(
        &mut inner.state,
        owner,
        item.msg,
        &inner.cfg.simulation,
        &mut events,
    ) {
        Ok(()) => {
            for ev in events {
                let _ = handle
                    .broadcast
                    .send(Arc::new(ServerMessage::Event { event: ev }));
            }
        }
        Err(e) => {
            let _ = handle.broadcast.send(Arc::new(ServerMessage::Error {
                to: Some(owner),
                code: e.code().into(),
                detail: e.to_string(),
            }));
        }
    }
}

fn unit_ids_touched(
    events: &[simulation::GameEvent],
) -> std::collections::HashSet<crate::game_state::UnitId> {
    use simulation::GameEvent::*;
    let mut set = std::collections::HashSet::new();
    for e in events {
        match e {
            MoveQueued { unit, .. } | MoveResolved { unit, .. } | MoveBlocked { unit, .. } => {
                set.insert(*unit);
            }
            MeleeLocked { a, b } => {
                set.insert(*a);
                set.insert(*b);
            }
            MeleeHit {
                attacker, defender, ..
            }
            | ArtilleryFired {
                unit: attacker,
                target: defender,
                ..
            } => {
                set.insert(*attacker);
                set.insert(*defender);
            }
            UnitUpgraded { unit, .. } | UnitDied { unit, .. } => {
                set.insert(*unit);
            }
            MatchStarted | MatchOver { .. } => {}
        }
    }
    set
}
