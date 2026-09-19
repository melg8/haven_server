//! The fixed-tick simulation loop.
//!
//! One call to [`tick`] advances the world one step and returns the events
//! produced, in the order documented in the game plan:
//!   1. (inputs are drained by the match loop before calling `tick`)
//!   2. resolve probabilistic movement
//!   3. check melee adjacency → form locks
//!   4. process melee damage (cooldowns, hits)
//!   5. process artillery fire (cooldowns, LOS, shots)
//!   6. clean up dead units
//!   7. check win conditions

use crate::config::SimulationConfig;
use crate::game_state::{
    EndReason, GameState, MatchStatus, Owner, Tile, UnitId, UnitKind, UnitState,
};
use rand::Rng;
use serde::{Deserialize, Serialize};

/// Server-side game events broadcast to clients for visual/audio feedback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GameEvent {
    MatchStarted,
    MoveQueued {
        unit: UnitId,
        by: Owner,
        from: Tile,
        to: Tile,
    },
    MoveResolved {
        unit: UnitId,
        by: Owner,
        from: Tile,
        to: Tile,
    },
    MoveBlocked {
        unit: UnitId,
        by: Owner,
        to: Tile,
        reason: String,
    },
    MeleeLocked {
        a: UnitId,
        b: UnitId,
    },
    MeleeHit {
        attacker: UnitId,
        defender: UnitId,
        damage: i32,
        hp_left: i32,
    },
    ArtilleryFired {
        unit: UnitId,
        from: Tile,
        to: Tile,
        target: UnitId,
        damage: i32,
    },
    UnitUpgraded {
        unit: UnitId,
        at: Tile,
        kind: UnitKind,
    },
    UnitDied {
        unit: UnitId,
        owner: Owner,
        at: Tile,
    },
    MatchOver {
        winner: Option<Owner>,
        reason: EndReason,
    },
}

/// Advance the world by exactly one tick. Returns produced events.
pub fn tick(state: &mut GameState, cfg: &SimulationConfig, rng: &mut impl Rng) -> Vec<GameEvent> {
    if state.status != MatchStatus::Active {
        return Vec::new();
    }
    state.tick += 1;

    let mut events = Vec::new();
    resolve_movement(state, cfg, rng, &mut events);
    form_melee_locks(state, cfg, &mut events);
    process_melee_damage(state, cfg, &mut events);
    process_artillery(state, cfg, &mut events);
    cleanup_dead(state, &mut events);
    check_win(state, cfg, &mut events);
    events
}

// ---------------------------------------------------------------------------
// 2. Probabilistic movement
// ---------------------------------------------------------------------------

fn resolve_movement(
    state: &mut GameState,
    cfg: &SimulationConfig,
    rng: &mut impl Rng,
    events: &mut Vec<GameEvent>,
) {
    let ids: Vec<UnitId> = state.units.keys().copied().collect();
    let per_tick = cfg.move_probability_per_tick();
    for id in ids {
        let Some(target) = (match state.units.get(&id) {
            Some(unit) if unit.kind == UnitKind::Pawn && unit.state != UnitState::Locked => {
                unit.pending_move
            }
            _ => None,
        }) else {
            continue;
        };
        if !rng.random::<f64>().lt(&per_tick) {
            continue; // the dice said "not yet" — the tension lives here
        }
        let (from, by) = match state.units.get(&id) {
            Some(u) => (u.pos, u.owner),
            None => continue,
        };
        if target.chebyshev(from) > cfg.pawn_move_range || !state.is_free(target) {
            // Path went stale: target got occupied (or walls changed) while queued.
            if let Some(u) = state.units.get_mut(&id) {
                u.pending_move = None;
                u.state = UnitState::Idle;
            }
            events.push(GameEvent::MoveBlocked {
                unit: id,
                by,
                to: target,
                reason: "target occupied".into(),
            });
            continue;
        }
        let u = state.units.get_mut(&id).expect("checked above");
        u.pos = target;
        u.pending_move = None;
        u.state = UnitState::Idle;
        events.push(GameEvent::MoveResolved {
            unit: id,
            by,
            from,
            to: target,
        });
    }
}

// ---------------------------------------------------------------------------
// 3. Melee locks
// ---------------------------------------------------------------------------

fn form_melee_locks(state: &mut GameState, cfg: &SimulationConfig, events: &mut Vec<GameEvent>) {
    let ids: Vec<UnitId> = state.units.keys().copied().collect();
    let mut already_locked: Vec<UnitId> = Vec::new();
    for (i, &a) in ids.iter().enumerate() {
        if already_locked.contains(&a) {
            continue;
        }
        let (a_owner, a_pos) = match state.units.get(&a) {
            Some(u) => (u.owner, u.pos),
            None => continue,
        };
        for &b in ids.iter().skip(i + 1) {
            if already_locked.contains(&b) {
                continue;
            }
            let Some(ub) = state.units.get(&b) else {
                continue;
            };
            if ub.owner == a_owner || ub.state == UnitState::Locked || ub.pos.chebyshev(a_pos) != 1
            {
                continue;
            }
            // Lock the pair: both stop moving and start swinging.
            for (side, other) in [(a, b), (b, a)] {
                if let Some(u) = state.units.get_mut(&side) {
                    u.state = UnitState::Locked;
                    u.melee_target = Some(other);
                    u.pending_move = None;
                    u.attack_cooldown = cfg.melee_cooldown_ticks();
                }
            }
            already_locked.push(a);
            already_locked.push(b);
            events.push(GameEvent::MeleeLocked { a, b });
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Melee damage
// ---------------------------------------------------------------------------

fn process_melee_damage(
    state: &mut GameState,
    cfg: &SimulationConfig,
    events: &mut Vec<GameEvent>,
) {
    // Collect unique locked pairs (a < b) so each fight is processed once.
    let pairs: Vec<(UnitId, UnitId)> = state
        .units
        .values()
        .filter(|u| u.state == UnitState::Locked && u.melee_target.is_some())
        .map(|u| {
            (
                u.id.min(u.melee_target.unwrap()),
                u.id.max(u.melee_target.unwrap()),
            )
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    for (a, b) in pairs {
        if !seen.insert((a, b)) {
            continue;
        }
        // Both cooldowns tick down; whoever reaches zero lands a hit.
        for (attacker, defender) in [(a, b), (b, a)] {
            let ready = matches!(
                state.units.get(&attacker),
                Some(ua) if ua.state == UnitState::Locked
            );
            if !ready {
                continue;
            }
            if state.units.get(&attacker).unwrap().attack_cooldown > 0 {
                state.units.get_mut(&attacker).unwrap().attack_cooldown -= 1;
                continue;
            }
            let damage = cfg.melee_damage;
            let hp_left = match state.units.get_mut(&defender) {
                Some(ud) => {
                    ud.hp -= damage;
                    ud.hp
                }
                None => continue,
            };
            state
                .units
                .get_mut(&attacker)
                .expect("attacker checked above")
                .attack_cooldown = cfg.melee_cooldown_ticks();
            events.push(GameEvent::MeleeHit {
                attacker,
                defender,
                damage,
                hp_left,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Artillery
// ---------------------------------------------------------------------------

fn process_artillery(state: &mut GameState, cfg: &SimulationConfig, events: &mut Vec<GameEvent>) {
    let ids: Vec<UnitId> = state
        .units
        .values()
        .filter(|u| u.kind == UnitKind::Artillery && u.state != UnitState::Locked)
        .map(|u| u.id)
        .collect();
    for id in ids {
        // Read-only pass: cooldown tick or gather firing parameters without
        // holding the mutable borrow across the target scan.
        let (pos, owner) = match state.units.get(&id) {
            Some(unit) => {
                if unit.fire_cooldown > 0 {
                    state
                        .units
                        .get_mut(&id)
                        .expect("checked above")
                        .fire_cooldown -= 1;
                    continue;
                }
                (unit.pos, unit.owner)
            }
            None => continue,
        };
        let Some((target, target_tile)) =
            state.scan_orthogonal_target(pos, cfg.artillery_range, owner)
        else {
            continue;
        };
        let hp_left = match state.units.get_mut(&target) {
            Some(t) => {
                t.hp -= cfg.artillery_damage;
                t.hp
            }
            None => continue,
        };
        if let Some(unit) = state.units.get_mut(&id) {
            unit.fire_cooldown = cfg.artillery_cooldown_ticks();
        }
        events.push(GameEvent::ArtilleryFired {
            unit: id,
            from: pos,
            to: target_tile,
            target,
            damage: cfg.artillery_damage,
        });
        let _ = hp_left; // deaths are resolved in cleanup_dead
    }
}

// ---------------------------------------------------------------------------
// 6. Death cleanup
// ---------------------------------------------------------------------------

fn cleanup_dead(state: &mut GameState, events: &mut Vec<GameEvent>) {
    let dead: Vec<UnitId> = state
        .units
        .values()
        .filter(|u| !u.is_alive())
        .map(|u| u.id)
        .collect();
    for id in dead {
        let Some(unit) = state.units.remove(&id) else {
            continue;
        };
        events.push(GameEvent::UnitDied {
            unit: id,
            owner: unit.owner,
            at: unit.pos,
        });
        // Break the partner's grapple so it can fight/move again.
        if let Some(partner_id) = unit.melee_target {
            if let Some(partner) = state.units.get_mut(&partner_id) {
                partner.melee_target = None;
                partner.state = UnitState::Idle;
                partner.attack_cooldown = 0;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Win conditions
// ---------------------------------------------------------------------------

fn check_win(state: &mut GameState, cfg: &SimulationConfig, events: &mut Vec<GameEvent>) {
    if state.status != MatchStatus::Active {
        return;
    }
    let white = state.alive_count(Owner::White) as u64;
    let black = state.alive_count(Owner::Black) as u64;

    fn finish(
        state: &mut GameState,
        winner: Option<Owner>,
        reason: EndReason,
        events: &mut Vec<GameEvent>,
    ) {
        state.status = MatchStatus::Finished;
        state.winner = winner;
        state.end_reason = Some(reason.clone());
        events.push(GameEvent::MatchOver { winner, reason });
    }

    if white == 0 || black == 0 {
        let (winner, reason) = match (white == 0, black == 0) {
            (true, true) => (None, EndReason::MutualAnnihilation),
            (true, false) => (Some(Owner::Black), EndReason::Annihilation),
            (false, true) => (Some(Owner::White), EndReason::Annihilation),
            (false, false) => unreachable!(),
        };
        finish(state, winner, reason, events);
        return;
    }

    if state.tick >= cfg.match_timeout_ticks() {
        let (winner, reason) = match white.cmp(&black) {
            std::cmp::Ordering::Greater => (Some(Owner::White), EndReason::Timeout),
            std::cmp::Ordering::Less => (Some(Owner::Black), EndReason::Timeout),
            std::cmp::Ordering::Equal => (None, EndReason::Draw),
        };
        finish(state, winner, reason, events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BoardConfig;
    use crate::config::SimulationConfig;
    use crate::game_state::{ArtilleryType, GameState};
    use crate::input;
    use crate::network::ClientMessage;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn cfg() -> SimulationConfig {
        SimulationConfig::default()
    }

    fn state() -> GameState {
        GameState::new(&BoardConfig::default(), 2)
    }

    fn unit_at(state: &GameState, owner: Owner, tile: Tile) -> UnitId {
        state
            .units
            .values()
            .find(|u| u.owner == owner && u.pos == tile)
            .map(|u| u.id)
            .unwrap_or_else(|| panic!("no {owner:?} unit at {:?}", tile))
    }
    fn activate(state: &mut GameState) {
        state.status = MatchStatus::Active;
    }

    /// Setup used by melee tests: white pawn d2 (3,1), black pawn d3 (3,2).
    /// Diagonal white neighbours (c2, e2) are removed so d2 is the black
    /// pawn's only possible lock partner.
    fn melee_pair() -> (GameState, UnitId, UnitId) {
        let mut s = state();
        activate(&mut s);
        let w = unit_at(&s, Owner::White, Tile::new(3, 1));
        let b = unit_at(&s, Owner::Black, Tile::new(3, 6));
        s.units.get_mut(&b).unwrap().pos = Tile::new(3, 2); // d3, adjacent to d2
        for tile in [Tile::new(2, 1), Tile::new(4, 1)] {
            let id = unit_at(&s, Owner::White, tile);
            s.units.remove(&id);
        }
        (s, w, b)
    }

    #[test]
    fn queued_move_resolves_eventually() {
        let mut s = state();
        activate(&mut s);
        let id = unit_at(&s, Owner::White, Tile::new(0, 1)); // a2
        let mut ev = vec![];
        input::handle_message(
            &mut s,
            Owner::White,
            ClientMessage::Move {
                unit_id: id,
                target: Tile::new(1, 2),
            }, // b3
            &cfg(),
            &mut ev,
        )
        .unwrap();
        assert_eq!(s.units[&id].pos, Tile::new(0, 1)); // not applied yet
        assert_eq!(s.units[&id].state, UnitState::Moving);

        let mut rng = StdRng::seed_from_u64(42);
        let mut resolved = false;
        for _ in 0..3000 {
            let events = tick(&mut s, &cfg(), &mut rng);
            if events
                .iter()
                .any(|e| matches!(e, GameEvent::MoveResolved { unit, .. } if *unit == id))
            {
                resolved = true;
                break;
            }
        }
        assert!(resolved, "move must resolve within a few seconds of ticks");
        assert_eq!(s.units[&id].pos, Tile::new(1, 2));
        assert_eq!(s.units[&id].pending_move, None);
        assert_eq!(s.units[&id].state, UnitState::Idle);
    }

    #[test]
    fn locked_units_never_move() {
        let (mut s, w, b) = melee_pair();
        // Queue a legal escape move, but make the dice nearly never resolve it.
        let mut c = cfg();
        c.move_probability_per_second = 0.0001;
        let mut ev = vec![];
        input::handle_message(
            &mut s,
            Owner::White,
            ClientMessage::Move {
                unit_id: w,
                target: Tile::new(2, 1),
            },
            &c,
            &mut ev,
        )
        .unwrap();

        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..30 {
            tick(&mut s, &c, &mut rng);
        }
        // Adjacent enemies locked on tick 1; queued move was cancelled with the
        // lock; the unit never moved.
        let uw = &s.units[&w];
        assert_eq!(uw.state, UnitState::Locked);
        assert_eq!(uw.pos, Tile::new(3, 1));
        assert_eq!(uw.pending_move, None);
        assert_eq!(uw.melee_target, Some(b));
        let _ = b;
    }

    #[test]
    fn melee_trade_kills_both_on_second_hit() {
        let (mut s, w, b) = melee_pair();
        let start_tick = s.tick;
        let mut rng = StdRng::seed_from_u64(11);
        let mut hits = 0;
        for _ in 0..500 {
            let events = tick(&mut s, &cfg(), &mut rng);
            hits += events
                .iter()
                .filter(|e| matches!(e, GameEvent::MeleeHit { .. }))
                .count();
            if !s.units.contains_key(&w) {
                break;
            }
        }
        // 2 HP each + simultaneous damage = a clean 1-for-1 trade.
        assert!(!s.units.contains_key(&w), "white unit must die");
        assert!(!s.units.contains_key(&b), "black unit must die");
        assert!(hits >= 2, "2 HP means at least two hits landed, got {hits}");
        let elapsed = s.tick - start_tick;
        assert!(
            elapsed >= cfg().melee_cooldown_ticks() as u64 * 2,
            "two hits need at least two cooldowns, took {elapsed} ticks"
        );
        // The rest of the army is untouched; the match continues.
        assert_eq!(s.status, MatchStatus::Active);
        assert_eq!(s.alive_count(Owner::White), 5); // 8 minus setup(2) minus traded
        assert_eq!(s.alive_count(Owner::Black), 7); // 8 minus traded
    }

    #[test]
    fn artillery_fires_and_kills_in_line_of_sight() {
        let mut s = state();
        activate(&mut s);
        let w = unit_at(&s, Owner::White, Tile::new(3, 1)); // d2 pawn
        let b = unit_at(&s, Owner::Black, Tile::new(3, 6)); // d7 pawn — same file
        let mut ev = vec![];
        input::handle_message(
            &mut s,
            Owner::White,
            ClientMessage::Upgrade {
                unit_id: w,
                artillery_type: ArtilleryType::Cannon,
            },
            &cfg(),
            &mut ev,
        )
        .unwrap();
        assert_eq!(s.units[&w].kind, UnitKind::Artillery);
        s.units.get_mut(&w).unwrap().fire_cooldown = 0; // shoot immediately

        let mut rng = StdRng::seed_from_u64(3);
        let mut shots = 0;
        for _ in 0..200 {
            let events = tick(&mut s, &cfg(), &mut rng);
            shots += events
                .iter()
                .filter(|e| matches!(e, GameEvent::ArtilleryFired { .. }))
                .count();
            if !s.units.contains_key(&b) {
                break;
            }
        }
        assert!(
            shots >= 2,
            "artillery must fire repeatedly (damage 1, hp 2)"
        );
        assert!(!s.units.contains_key(&b), "target must die after two shots");
        // Artillery never moved.
        assert_eq!(s.units[&w].pos, Tile::new(3, 1));
    }

    #[test]
    fn friendly_unit_blocks_artillery_line_of_sight() {
        let mut s = state();
        activate(&mut s);
        let w = unit_at(&s, Owner::White, Tile::new(3, 1));
        // Friendly blocker one tile in front of the artillery.
        let blocker = s.spawn_unit(UnitKind::Pawn, Owner::White, Tile::new(3, 3), 2);
        let b = unit_at(&s, Owner::Black, Tile::new(3, 6));
        input::handle_message(
            &mut s,
            Owner::White,
            ClientMessage::Upgrade {
                unit_id: w,
                artillery_type: ArtilleryType::Cannon,
            },
            &cfg(),
            &mut vec![],
        )
        .unwrap();
        s.units.get_mut(&w).unwrap().fire_cooldown = 0;

        let mut rng = StdRng::seed_from_u64(13);
        for _ in 0..120 {
            tick(&mut s, &cfg(), &mut rng);
        }
        assert!(
            s.units.contains_key(&b),
            "friendly blocker must protect the target"
        );
        assert!(s.units.contains_key(&blocker));
        assert_eq!(s.units[&blocker].hp, 2);
    }

    #[test]
    fn artillery_cannot_move() {
        let mut s = state();
        activate(&mut s);
        let w = unit_at(&s, Owner::White, Tile::new(3, 1));
        let c = cfg();
        input::handle_message(
            &mut s,
            Owner::White,
            ClientMessage::Upgrade {
                unit_id: w,
                artillery_type: ArtilleryType::Cannon,
            },
            &c,
            &mut vec![],
        )
        .unwrap();
        assert!(matches!(
            input::handle_message(
                &mut s,
                Owner::White,
                ClientMessage::Move {
                    unit_id: w,
                    target: Tile::new(4, 2)
                },
                &c,
                &mut vec![],
            ),
            Err(crate::input::CommandError::NotMovable(_))
        ));
        let mut rng = StdRng::seed_from_u64(5);
        for _ in 0..100 {
            tick(&mut s, &c, &mut rng);
        }
        assert_eq!(s.units[&w].pos, Tile::new(3, 1));
    }

    #[test]
    fn mutual_annihilation_ends_in_draw() {
        let mut s = GameState::new(&BoardConfig::default(), 1); // 1 HP units
        activate(&mut s);
        let w = unit_at(&s, Owner::White, Tile::new(3, 1));
        let b = unit_at(&s, Owner::Black, Tile::new(3, 6));
        // Keep only this pair.
        let ids: Vec<UnitId> = s.units.keys().copied().collect();
        for id in ids {
            if id != w && id != b {
                s.units.remove(&id);
            }
        }
        s.units.get_mut(&b).unwrap().pos = Tile::new(3, 2); // adjacent

        let mut rng = StdRng::seed_from_u64(99);
        for _ in 0..500 {
            tick(&mut s, &cfg(), &mut rng);
            if s.status == MatchStatus::Finished {
                break;
            }
        }
        assert_eq!(s.status, MatchStatus::Finished);
        assert_eq!(s.winner, None);
        assert!(matches!(s.end_reason, Some(EndReason::MutualAnnihilation)));
    }

    #[test]
    fn timeout_picks_the_side_with_more_units() {
        let mut s = state();
        activate(&mut s);
        let extra = unit_at(&s, Owner::Black, Tile::new(7, 6)); // h7
        s.units.remove(&extra);
        let mut c = cfg();
        c.match_timeout_secs = 0.1; // 1 tick @ 10 Hz
        let mut rng = StdRng::seed_from_u64(1);
        tick(&mut s, &c, &mut rng);
        assert_eq!(s.status, MatchStatus::Finished);
        assert_eq!(s.winner, Some(Owner::White));
        assert!(matches!(s.end_reason, Some(EndReason::Timeout)));
    }

    #[test]
    fn stale_target_gets_blocked_at_resolve_time() {
        let mut s = state();
        activate(&mut s);
        let a = unit_at(&s, Owner::White, Tile::new(0, 1)); // a2
        let other = unit_at(&s, Owner::White, Tile::new(1, 1)); // b2
        let mut ev = vec![];
        // Both queue the same tile b3.
        for id in [a, other] {
            input::handle_message(
                &mut s,
                Owner::White,
                ClientMessage::Move {
                    unit_id: id,
                    target: Tile::new(1, 2),
                },
                &cfg(),
                &mut ev,
            )
            .unwrap();
        }
        // Force both rolls to succeed on the same tick: probability 1.
        let mut c = cfg();
        c.move_probability_per_second = 1.0;
        let mut rng = StdRng::seed_from_u64(23);
        let mut blocked = false;
        for _ in 0..5 {
            let events = tick(&mut s, &c, &mut rng);
            if events
                .iter()
                .any(|e| matches!(e, GameEvent::MoveBlocked { .. }))
            {
                blocked = true;
                break;
            }
        }
        assert!(
            blocked,
            "second arrival on an occupied tile must be blocked"
        );
    }
}
