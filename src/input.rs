//! Client command validation.
//!
//! Commands arrive over WebSocket and are applied to the *queued* layer of the
//! game state (never straight into the world): moves are enqueued and resolve
//! later on the simulation's probabilistic dice. Everything is validated here —
//! ownership, unit kind, match status, range and legality.

use crate::game_state::{
    ArtilleryType, GameState, MatchStatus, Owner, Tile, UnitId, UnitKind, UnitState,
};
use crate::network::ClientMessage;
use crate::simulation::GameEvent;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    MatchNotActive,
    NotAPlayer,
    UnknownUnit(UnitId),
    NotOwned(UnitId),
    NotMovable(UnitId),
    IllegalTarget { to: Tile, reason: String },
    NotUpgradable(UnitId),
    UnknownArtilleryType,
}

impl CommandError {
    /// Stable machine-readable code sent to clients in `error` messages.
    pub fn code(&self) -> &'static str {
        match self {
            CommandError::MatchNotActive => "match_not_active",
            CommandError::NotAPlayer => "not_a_player",
            CommandError::UnknownUnit(_) => "unknown_unit",
            CommandError::NotOwned(_) => "not_owned",
            CommandError::NotMovable(_) => "not_movable",
            CommandError::IllegalTarget { .. } => "illegal_target",
            CommandError::NotUpgradable(_) => "not_upgradable",
            CommandError::UnknownArtilleryType => "unknown_artillery_type",
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::MatchNotActive => write!(f, "match is not active"),
            CommandError::NotAPlayer => write!(f, "spectators cannot send commands"),
            CommandError::UnknownUnit(id) => write!(f, "unit {id} does not exist"),
            CommandError::NotOwned(id) => write!(f, "unit {id} is not yours"),
            CommandError::NotMovable(id) => {
                write!(f, "unit {id} cannot move (locked, artillery or dead)")
            }
            CommandError::IllegalTarget { to, reason } => {
                write!(f, "illegal target {}-{}, {reason}", to.x, to.y)
            }
            CommandError::NotUpgradable(id) => {
                write!(
                    f,
                    "unit {id} cannot be upgraded right now (must be an idle pawn)"
                )
            }
            CommandError::UnknownArtilleryType => write!(f, "unknown artillery type"),
        }
    }
}

/// Validate and apply a client message to the queued layer of `state`.
/// On success, produced bookkeeping events (e.g. `MoveQueued`) are pushed.
pub fn handle_message(
    state: &mut GameState,
    owner: Owner,
    msg: ClientMessage,
    cfg: &crate::config::SimulationConfig,
    events: &mut Vec<GameEvent>,
) -> Result<(), CommandError> {
    match msg {
        ClientMessage::Select { unit_id } => handle_select(state, owner, unit_id),
        ClientMessage::Move { unit_id, target } => {
            handle_move(state, owner, unit_id, target, cfg, events)
        }
        ClientMessage::Upgrade {
            unit_id,
            artillery_type,
        } => handle_upgrade(state, owner, unit_id, artillery_type, cfg, events),
        // Answered at the WebSocket layer; reaching here means a transport
        // quirk — treat as a no-op.
        ClientMessage::Ping => Ok(()),
    }
}

fn owned_unit(state: &GameState, owner: Owner, unit_id: UnitId) -> Result<(), CommandError> {
    match state.units.get(&unit_id) {
        None => Err(CommandError::UnknownUnit(unit_id)),
        Some(u) if u.owner != owner => Err(CommandError::NotOwned(unit_id)),
        Some(_) => Ok(()),
    }
}

fn handle_select(state: &mut GameState, owner: Owner, unit_id: UnitId) -> Result<(), CommandError> {
    owned_unit(state, owner, unit_id)?;
    // Selection is client-local presentation; the server only validates that
    // the client owns what it says it selected. No state change.
    Ok(())
}

fn handle_move(
    state: &mut GameState,
    owner: Owner,
    unit_id: UnitId,
    target: Tile,
    cfg: &crate::config::SimulationConfig,
    events: &mut Vec<GameEvent>,
) -> Result<(), CommandError> {
    if state.status != MatchStatus::Active {
        return Err(CommandError::MatchNotActive);
    }
    owned_unit(state, owner, unit_id)?;

    let unit = state.units.get(&unit_id).expect("checked above");
    if unit.kind != UnitKind::Pawn {
        return Err(CommandError::NotMovable(unit_id)); // artillery is fixed
    }
    if unit.state == UnitState::Locked {
        return Err(CommandError::NotMovable(unit_id)); // grappled: no moving out
    }

    if !state.is_inside(target) {
        return Err(CommandError::IllegalTarget {
            to: target,
            reason: "outside the board".into(),
        });
    }
    if target.chebyshev(unit.pos) > cfg.pawn_move_range {
        return Err(CommandError::IllegalTarget {
            to: target,
            reason: format!("move range is {} tile(s)", cfg.pawn_move_range),
        });
    }
    if state.walls.contains(&target) {
        return Err(CommandError::IllegalTarget {
            to: target,
            reason: "wall".into(),
        });
    }
    if let Some(other) = state.unit_at(target) {
        let _ = other;
        return Err(CommandError::IllegalTarget {
            to: target,
            reason: "tile occupied".into(),
        });
    }

    // Legality passed — queue the move instead of applying it. The simulation
    // rolls the dice on each tick until it resolves (or goes stale).
    let from = unit.pos;
    let unit = state.units.get_mut(&unit_id).expect("checked above");
    unit.pending_move = Some(target);
    unit.state = UnitState::Moving;
    events.push(GameEvent::MoveQueued {
        unit: unit_id,
        by: owner,
        from,
        to: target,
    });
    Ok(())
}

fn handle_upgrade(
    state: &mut GameState,
    owner: Owner,
    unit_id: UnitId,
    artillery_type: ArtilleryType,
    cfg: &crate::config::SimulationConfig,
    events: &mut Vec<GameEvent>,
) -> Result<(), CommandError> {
    if state.status != MatchStatus::Active {
        return Err(CommandError::MatchNotActive);
    }
    owned_unit(state, owner, unit_id)?;

    let unit = state.units.get(&unit_id).expect("checked above");
    if unit.kind != UnitKind::Pawn {
        return Err(CommandError::NotUpgradable(unit_id));
    }
    if unit.state != UnitState::Idle || unit.pending_move.is_some() {
        return Err(CommandError::NotUpgradable(unit_id));
    }

    let _ = artillery_type; // MVP has a single cannon variant; kept for the wire protocol
    let at = unit.pos;
    let unit = state.units.get_mut(&unit_id).expect("checked above");
    unit.kind = UnitKind::Artillery;
    unit.state = UnitState::Idle;
    unit.pending_move = None;
    unit.fire_cooldown = cfg.artillery_cooldown_ticks();
    events.push(GameEvent::UnitUpgraded {
        unit: unit_id,
        at,
        kind: UnitKind::Artillery,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BoardConfig;
    use crate::config::SimulationConfig;

    fn state() -> GameState {
        GameState::new(&BoardConfig::default(), 2)
    }

    fn white_pawn_at(state: &GameState, tile: Tile) -> UnitId {
        state
            .units
            .values()
            .find(|u| u.owner == Owner::White && u.pos == tile)
            .map(|u| u.id)
            .expect("no white pawn there")
    }

    #[test]
    fn move_queues_legally() {
        let mut s = state();
        s.status = MatchStatus::Active;
        let id = white_pawn_at(&s, Tile::new(0, 1));
        let mut ev = vec![];
        let r = handle_message(
            &mut s,
            Owner::White,
            ClientMessage::Move {
                unit_id: id,
                target: Tile::new(1, 2),
            },
            &SimulationConfig::default(),
            &mut ev,
        );
        assert!(r.is_ok());
        assert_eq!(s.units[&id].pending_move, Some(Tile::new(1, 2)));
        assert_eq!(s.units[&id].state, UnitState::Moving);
        assert!(matches!(ev.first(), Some(GameEvent::MoveQueued { .. })));
    }

    #[test]
    fn move_rejections() {
        let mut s = state();
        s.status = MatchStatus::Active;
        let id = white_pawn_at(&s, Tile::new(0, 1));
        let cfg = SimulationConfig::default();
        let mut ev = vec![];

        // Enemy unit is not yours.
        let enemy_id = s
            .units
            .values()
            .find(|u| u.owner == Owner::Black)
            .map(|u| u.id)
            .unwrap();
        assert_eq!(
            handle_message(
                &mut s,
                Owner::White,
                ClientMessage::Select { unit_id: enemy_id },
                &cfg,
                &mut ev
            ),
            Err(CommandError::NotOwned(enemy_id))
        );

        // Out of range.
        assert_eq!(
            handle_message(
                &mut s,
                Owner::White,
                ClientMessage::Move {
                    unit_id: id,
                    target: Tile::new(2, 2)
                },
                &cfg,
                &mut ev
            ),
            Err(CommandError::IllegalTarget {
                to: Tile::new(2, 2),
                reason: "move range is 1 tile(s)".into()
            })
        );

        // Occupied tile.
        let other = white_pawn_at(&s, Tile::new(1, 1));
        assert!(matches!(
            handle_message(
                &mut s,
                Owner::White,
                ClientMessage::Move {
                    unit_id: id,
                    target: Tile::new(1, 1)
                },
                &cfg,
                &mut ev
            ),
            Err(CommandError::IllegalTarget { .. })
        ));
        let _ = other;

        // Outside the board.
        assert!(matches!(
            handle_message(
                &mut s,
                Owner::White,
                ClientMessage::Move {
                    unit_id: id,
                    target: Tile::new(0, 9)
                },
                &cfg,
                &mut ev
            ),
            Err(CommandError::IllegalTarget { .. })
        ));

        // Lobby state rejects everything.
        s.status = MatchStatus::Lobby;
        assert_eq!(
            handle_message(
                &mut s,
                Owner::White,
                ClientMessage::Move {
                    unit_id: id,
                    target: Tile::new(1, 2)
                },
                &cfg,
                &mut ev
            ),
            Err(CommandError::MatchNotActive)
        );
    }

    #[test]
    fn upgrade_converts_idle_pawn_only() {
        let mut s = state();
        s.status = MatchStatus::Active;
        let id = white_pawn_at(&s, Tile::new(3, 1));
        let cfg = SimulationConfig::default();
        let mut ev = vec![];

        // While moving — rejected.
        s.units.get_mut(&id).unwrap().pending_move = Some(Tile::new(4, 2));
        s.units.get_mut(&id).unwrap().state = UnitState::Moving;
        assert_eq!(
            handle_message(
                &mut s,
                Owner::White,
                ClientMessage::Upgrade {
                    unit_id: id,
                    artillery_type: ArtilleryType::Cannon
                },
                &cfg,
                &mut ev
            ),
            Err(CommandError::NotUpgradable(id))
        );
        s.units.get_mut(&id).unwrap().pending_move = None;
        s.units.get_mut(&id).unwrap().state = UnitState::Idle;

        // Idle — accepted.
        assert!(handle_message(
            &mut s,
            Owner::White,
            ClientMessage::Upgrade {
                unit_id: id,
                artillery_type: ArtilleryType::Cannon
            },
            &cfg,
            &mut ev
        )
        .is_ok());
        assert_eq!(s.units[&id].kind, UnitKind::Artillery);
        assert_eq!(s.units[&id].fire_cooldown, cfg.artillery_cooldown_ticks());

        // Upgrading artillery again — rejected.
        assert_eq!(
            handle_message(
                &mut s,
                Owner::White,
                ClientMessage::Upgrade {
                    unit_id: id,
                    artillery_type: ArtilleryType::Cannon
                },
                &cfg,
                &mut ev
            ),
            Err(CommandError::NotUpgradable(id))
        );
    }
}
