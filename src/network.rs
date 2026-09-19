//! Wire protocol and WebSocket handling.
//!
//! Client → server messages are validated in `input.rs`; server → client
//! messages fall into three groups: per-connection frames (`welcome`,
//! `state_snapshot`), per-tick broadcasts (`state_snapshot` / `state_delta`),
//! and fire-and-forget notifications (`event`, `error`, `pong`).
//!
//! Every client gets a full `state_snapshot` on connect and on reconnect —
//! that is the reconnection story: clients retry with backoff and rebuild
//! their world from the next snapshot.

use crate::game_state::{ArtilleryType, GameState, MatchStatus, Owner, StateDelta, Tile, UnitId};
use crate::simulation::GameEvent;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize, Serializer};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

/// Who a WebSocket connection is. Players are authenticated by the
/// `player_token` issued on join (`?token=...` query parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Player(Owner),
    Spectator,
}

impl Role {
    pub fn owner(self) -> Option<Owner> {
        match self {
            Role::Player(o) => Some(o),
            Role::Spectator => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Wire messages
// ---------------------------------------------------------------------------

/// Messages the client sends.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Select {
        unit_id: UnitId,
    },
    Move {
        unit_id: UnitId,
        target: Tile,
    },
    Upgrade {
        unit_id: UnitId,
        artillery_type: ArtilleryType,
    },
    Ping,
}

/// Messages the server sends.
#[derive(Debug, Clone)]
pub enum ServerMessage {
    Welcome {
        match_id: String,
        you: Option<Owner>,
        status: MatchStatus,
        tick: u64,
    },
    StateSnapshot {
        state: GameState,
    },
    StateDelta {
        delta: StateDelta,
    },
    Event {
        event: GameEvent,
    },
    /// `to` routes the error to a single player; clients must ignore errors
    /// addressed to someone else. `to: null` is a spectator/global error.
    Error {
        to: Option<Owner>,
        code: String,
        detail: String,
    },
    Pong {
        tick: u64,
    },
}

impl Serialize for ServerMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        match self {
            ServerMessage::Welcome {
                match_id,
                you,
                status,
                tick,
            } => {
                map.serialize_entry("type", "welcome")?;
                map.serialize_entry("match_id", match_id)?;
                map.serialize_entry("you", you)?;
                map.serialize_entry("status", status)?;
                map.serialize_entry("tick", tick)?;
            }
            ServerMessage::StateSnapshot { state } => {
                map.serialize_entry("type", "state_snapshot")?;
                map.serialize_entry("state", state)?;
            }
            ServerMessage::StateDelta { delta } => {
                map.serialize_entry("type", "state_delta")?;
                map.serialize_entry("delta", delta)?;
            }
            ServerMessage::Event { event } => {
                map.serialize_entry("type", "event")?;
                map.serialize_entry("event", event)?;
            }
            ServerMessage::Error { to, code, detail } => {
                map.serialize_entry("type", "error")?;
                map.serialize_entry("to", to)?;
                map.serialize_entry("code", code)?;
                map.serialize_entry("detail", detail)?;
            }
            ServerMessage::Pong { tick } => {
                map.serialize_entry("type", "pong")?;
                map.serialize_entry("tick", tick)?;
            }
        }
        map.end()
    }
}

/// An item on the match's input queue.
#[derive(Debug)]
pub struct InputItem {
    pub from: Role,
    pub msg: ClientMessage,
}

/// Serialize a server message to a WebSocket text frame.
pub fn to_ws_text(msg: &ServerMessage) -> Message {
    let json = serde_json::to_string(msg).expect("server message is always serializable");
    Message::Text(json.into())
}

/// Parse one WebSocket text frame into a client message.
pub fn from_ws_text(raw: &str) -> Result<ClientMessage, String> {
    serde_json::from_str(raw).map_err(|e| format!("bad message: {e}"))
}

// ---------------------------------------------------------------------------
// Connection pump
// ---------------------------------------------------------------------------

/// Capacity of the per-connection direct channel (acks, errors, pong).
const DIRECT_CHANNEL_SIZE: usize = 32;
/// Capacity of the per-match broadcast channel.
pub const BROADCAST_CHANNEL_SIZE: usize = 512;
/// Capacity of the per-match input queue.
pub const INPUT_QUEUE_SIZE: usize = 256;

/// Run one connected client: forward broadcasts + direct messages to the
/// socket, and socket messages into the match input queue. Returns when the
/// client disconnects.
pub async fn run_connection(
    socket: WebSocket,
    role: Role,
    match_id: String,
    broadcast_rx: broadcast::Receiver<std::sync::Arc<ServerMessage>>,
    input_tx: mpsc::Sender<InputItem>,
    current_state: GameState,
) {
    let (mut sink, mut stream) = socket.split();
    let (direct_tx, mut direct_rx) = mpsc::channel::<ServerMessage>(DIRECT_CHANNEL_SIZE);

    // Initial handshake: welcome + full snapshot (this is also the reconnect path).
    let _ = sink
        .send(to_ws_text(&ServerMessage::Welcome {
            match_id: match_id.clone(),
            you: role.owner(),
            status: current_state.status,
            tick: current_state.tick,
        }))
        .await;
    let _ = sink
        .send(to_ws_text(&ServerMessage::StateSnapshot {
            state: current_state,
        }))
        .await;

    // Outbound task: broadcast + direct frames → socket.
    let outbound = tokio::spawn(async move {
        let mut broadcast_rx = broadcast_rx;
        loop {
            tokio::select! {
                next = broadcast_rx.recv() => {
                    match next {
                        Ok(msg) => {
                            if sink.send(to_ws_text(&msg)).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Client fell behind: it is stale, so force a resync
                            // by... the next snapshot tick will heal deltas, but
                            // to stay honest we just log; snapshot mode resends
                            // full state every tick anyway.
                            tracing::warn!("client lagged behind by {n} messages");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                Some(msg) = direct_rx.recv() => {
                    if sink.send(to_ws_text(&msg)).await.is_err() {
                        break;
                    }
                }
                else => break,
            }
        }
    });

    // Inbound loop: socket frames → parse → input queue.
    while let Some(Ok(frame)) = stream.next().await {
        let raw = match frame {
            Message::Text(t) => t,
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
        };
        match from_ws_text(raw.as_str()) {
            Ok(ClientMessage::Ping) => {
                // Answered locally: no need to bother the simulation.
                let _ = direct_tx.send(ServerMessage::Pong { tick: u64::MAX }).await;
            }
            Ok(msg) => {
                if input_tx.send(InputItem { from: role, msg }).await.is_err() {
                    break; // match is gone
                }
            }
            Err(detail) => {
                let _ = direct_tx
                    .send(ServerMessage::Error {
                        to: role.owner(),
                        code: "bad_request".into(),
                        detail,
                    })
                    .await;
            }
        }
    }

    outbound.abort();
}
