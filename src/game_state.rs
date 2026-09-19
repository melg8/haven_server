//! Core game state: board geometry, units and the authoritative `GameState`.
//!
//! Everything in this module is plain data + query helpers. Mutations happen in
//! `simulation.rs` (the tick loop) and `input.rs` (command validation), which
//! keeps the state model trivially serializable for WebSocket snapshots.

use crate::config::BoardConfig;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;

pub type UnitId = u32;

/// Board side: white moves up the board (from low `y`), black moves down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    White,
    Black,
}

impl Owner {
    pub fn opposite(self) -> Owner {
        match self {
            Owner::White => Owner::Black,
            Owner::Black => Owner::White,
        }
    }
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Owner::White => write!(f, "white"),
            Owner::Black => write!(f, "black"),
        }
    }
}

/// Tile coordinate. `x` = file (0..size), `y` = rank (0..size).
/// White's back rank is `y = 0`, black's is `y = size - 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Tile {
    pub x: u8,
    pub y: u8,
}

impl Tile {
    pub const ORTHO_DIRS: [(i32, i32); 4] = [(0, -1), (0, 1), (-1, 0), (1, 0)];
    pub const KING_DIRS: [(i32, i32); 8] = [
        (0, -1),
        (0, 1),
        (-1, 0),
        (1, 0),
        (-1, -1),
        (1, -1),
        (-1, 1),
        (1, 1),
    ];

    pub fn new(x: u8, y: u8) -> Self {
        Self { x, y }
    }

    /// Chebyshev distance (king move distance).
    pub fn chebyshev(self, other: Tile) -> u8 {
        self.x.abs_diff(other.x).max(self.y.abs_diff(other.y))
    }

    /// Checked tile translation; returns `None` outside the u8 grid plane.
    pub fn offset(self, dx: i32, dy: i32) -> Option<Tile> {
        let x = self.x as i32 + dx;
        let y = self.y as i32 + dy;
        if x < 0 || y < 0 || x > u8::MAX as i32 || y > u8::MAX as i32 {
            None
        } else {
            Some(Tile::new(x as u8, y as u8))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    Pawn,
    Artillery,
}

/// Artillery variants. MVP ships a single `Cannon`; extend here and in
/// `input.rs` when adding more types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtilleryType {
    Cannon,
}

impl From<ArtilleryType> for UnitKind {
    fn from(_: ArtilleryType) -> Self {
        UnitKind::Artillery
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitState {
    /// Standing still, free to act.
    Idle,
    /// Has a queued move awaiting its probabilistic roll.
    Moving,
    /// Grappled with an adjacent enemy; cannot move until the fight resolves.
    Locked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unit {
    pub id: UnitId,
    pub owner: Owner,
    pub kind: UnitKind,
    pub pos: Tile,
    pub hp: i32,
    pub max_hp: i32,
    pub state: UnitState,
    /// Queued move target (present while `state == Moving`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_move: Option<Tile>,
    /// Opponent this unit is grappled with (present while `state == Locked`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub melee_target: Option<UnitId>,
    /// Ticks until the next melee hit lands (both directions).
    pub attack_cooldown: u32,
    /// Ticks until the next artillery shot.
    pub fire_cooldown: u32,
}

impl Unit {
    pub fn is_alive(&self) -> bool {
        self.hp > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    /// Created, waiting for players to join.
    Lobby,
    /// Simulation running.
    Active,
    /// Game over (`winner` + `end_reason` describe the outcome).
    Finished,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// One side lost all units.
    Annihilation,
    /// Both sides lost all units in the same tick.
    MutualAnnihilation,
    /// Time ran out; more units alive wins, equal counts draw.
    Timeout,
    Draw,
}

/// Full authoritative state pushed to clients as `state_snapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub tick: u64,
    pub board_size: u8,
    pub status: MatchStatus,
    pub winner: Option<Owner>,
    pub end_reason: Option<EndReason>,
    /// All alive units keyed by id. BTreeMap keeps snapshot JSON stable.
    pub units: BTreeMap<UnitId, Unit>,
    next_unit_id: UnitId,
    /// Obstacles block movement and artillery line of sight. MVP spawns none,
    /// but the model (and simulation) already supports them.
    pub walls: HashSet<Tile>,
}

impl GameState {
    /// Build a fresh lobby state and deploy starting pawns.
    pub fn new(board: &BoardConfig, unit_hp: i32) -> Self {
        let mut state = Self {
            tick: 0,
            board_size: board.size,
            status: MatchStatus::Lobby,
            winner: None,
            end_reason: None,
            units: BTreeMap::new(),
            next_unit_id: 1,
            walls: HashSet::new(),
        };
        state.deploy_pawns(board, unit_hp);
        state
    }

    /// Deploy `pawn_row_depth` rows of pawns per side, chess-style:
    /// white on ranks `1 + d`, black on rank `size - 2 - d`.
    fn deploy_pawns(&mut self, board: &BoardConfig, unit_hp: i32) {
        let size = board.size;
        for depth in 0..board.pawn_row_depth {
            let white_y = 1 + depth;
            let black_y = size.saturating_sub(2 + depth);
            for x in 0..size {
                self.spawn_unit(UnitKind::Pawn, Owner::White, Tile::new(x, white_y), unit_hp);
                self.spawn_unit(UnitKind::Pawn, Owner::Black, Tile::new(x, black_y), unit_hp);
            }
        }
    }

    pub(crate) fn spawn_unit(
        &mut self,
        kind: UnitKind,
        owner: Owner,
        pos: Tile,
        hp: i32,
    ) -> UnitId {
        let id = self.next_unit_id;
        self.next_unit_id += 1;
        self.units.insert(
            id,
            Unit {
                id,
                owner,
                kind,
                pos,
                hp,
                max_hp: hp,
                state: UnitState::Idle,
                pending_move: None,
                melee_target: None,
                attack_cooldown: 0,
                fire_cooldown: 0,
            },
        );
        id
    }

    pub fn is_inside(&self, tile: Tile) -> bool {
        tile.x < self.board_size && tile.y < self.board_size
    }

    /// Tile has no wall and no unit on it.
    pub fn is_free(&self, tile: Tile) -> bool {
        self.is_inside(tile) && !self.walls.contains(&tile) && self.unit_at(tile).is_none()
    }

    pub fn unit_at(&self, tile: Tile) -> Option<UnitId> {
        self.units.values().find(|u| u.pos == tile).map(|u| u.id)
    }

    pub fn alive_count(&self, owner: Owner) -> usize {
        self.units.values().filter(|u| u.owner == owner).count()
    }

    /// First unit hit when scanning orthogonally from `from` up to `range`
    /// tiles. Walls and any unit block line of sight. Only enemy units are
    /// returned; a friendly blocker simply stops that direction's scan.
    pub fn scan_orthogonal_target(
        &self,
        from: Tile,
        range: u8,
        owner: Owner,
    ) -> Option<(UnitId, Tile)> {
        for (dx, dy) in Tile::ORTHO_DIRS {
            let mut dist = 1u8;
            let mut cursor = from;
            while dist <= range {
                let Some(next) = cursor.offset(dx, dy) else {
                    break;
                };
                if !self.is_inside(next) || self.walls.contains(&next) {
                    break; // wall blocks LOS
                }
                if let Some(id) = self.unit_at(next) {
                    if self.units.get(&id).map(|u| u.owner) != Some(owner) {
                        return Some((id, next)); // first enemy in line
                    }
                    break; // friendly blocker
                }
                cursor = next;
                dist += 1;
            }
        }
        None
    }
}

/// Per-tick change set pushed to clients in "delta" broadcast mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDelta {
    pub tick: u64,
    pub status: MatchStatus,
    pub winner: Option<Owner>,
    pub end_reason: Option<EndReason>,
    /// Full records for units created or changed this tick.
    pub updated: Vec<Unit>,
    /// Units removed (died) this tick.
    pub removed: Vec<UnitId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> BoardConfig {
        BoardConfig::default()
    }

    #[test]
    fn deployment_is_classic_chess() {
        let state = GameState::new(&board(), 2);
        assert_eq!(state.units.len(), 16);
        assert_eq!(state.alive_count(Owner::White), 8);
        assert_eq!(state.alive_count(Owner::Black), 8);
        // White pawns on rank 2 (y=1), black on rank 7 (y=6).
        let wp: Vec<u8> = state
            .units
            .values()
            .filter(|u| u.owner == Owner::White)
            .map(|u| u.pos.y)
            .collect();
        assert!(wp.iter().all(|&y| y == 1));
        let bp: Vec<u8> = state
            .units
            .values()
            .filter(|u| u.owner == Owner::Black)
            .map(|u| u.pos.y)
            .collect();
        assert!(bp.iter().all(|&y| y == 6));
    }

    #[test]
    fn unit_at_and_is_free() {
        let state = GameState::new(&board(), 2);
        let a2 = Tile::new(0, 1);
        assert!(state.unit_at(a2).is_some());
        assert!(!state.is_free(a2));
        assert!(state.is_free(Tile::new(0, 3)));
        assert!(!state.is_free(Tile::new(9, 9))); // outside
    }

    #[test]
    fn orthogonal_scan_respects_range_and_blockers() {
        let mut state = GameState::new(&board(), 2);
        // Grab the white pawn at d2 (x=3, y=1) and black pawn at d7 (x=3, y=6).
        let white = state
            .units
            .values()
            .find(|u| u.owner == Owner::White && u.pos == Tile::new(3, 1))
            .map(|u| u.id)
            .unwrap();
        let black = state
            .units
            .values()
            .find(|u| u.owner == Owner::Black && u.pos == Tile::new(3, 6))
            .map(|u| u.id)
            .unwrap();

        // Out of range: only 4 tiles of clearance between them.
        let hit = state.scan_orthogonal_target(Tile::new(3, 1), 4, Owner::White);
        assert!(hit.is_none());
        let hit = state.scan_orthogonal_target(Tile::new(3, 1), 5, Owner::White);
        assert_eq!(hit.map(|(id, _)| id), Some(black));

        // Friendly blocker stops the scan.
        let blocker = state.spawn_unit(UnitKind::Pawn, Owner::White, Tile::new(3, 3), 2);
        let hit = state.scan_orthogonal_target(Tile::new(3, 1), 8, Owner::White);
        assert!(hit.is_none(), "friendly unit must block LOS");
        let _ = blocker;
        let _ = white;
    }

    #[test]
    fn snapshot_roundtrip_through_json() {
        let state = GameState::new(&board(), 2);
        let json = serde_json::to_string(&state).unwrap();
        let back: GameState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.units.len(), state.units.len());
        assert_eq!(back.tick, state.tick);
    }
}
