//! Server, board and simulation configuration.
//!
//! Loaded from `config.toml` (path overridable via the `HAVEN_CONFIG` env var).
//! Every field has a sensible default so a partial config file is valid.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub board: BoardConfig,
    pub simulation: SimulationConfig,
    pub lifecycle: LifecycleConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Maximum number of concurrently live matches (memory guard).
    pub max_matches: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            max_matches: 64,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BoardConfig {
    /// Board is `size x size` tiles.
    pub size: u8,
    /// How many pawn rows each side starts with (1 => classic chess placement).
    pub pawn_row_depth: u8,
}

impl Default for BoardConfig {
    fn default() -> Self {
        Self {
            size: 8,
            pawn_row_depth: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SimulationConfig {
    /// Ticks per second for the fixed-timestep simulation loop.
    pub tick_rate: u32,
    /// Chance per second that a queued move resolves. The per-tick probability
    /// is `1 - (1 - p)^(1/tick_rate)` (30%/s at 10 Hz ≈ 3.5% per tick).
    pub move_probability_per_second: f64,
    /// Pawn step range in tiles (Chebyshev distance).
    pub pawn_move_range: u8,
    /// Unit health. With `melee_damage = 1` a unit dies on the second hit.
    pub unit_hp: i32,
    /// Melee damage dealt per hit.
    pub melee_damage: i32,
    /// Seconds between melee hits.
    pub melee_cooldown_secs: f64,
    /// Artillery: straight-line firing range in tiles.
    pub artillery_range: u8,
    /// Artillery damage per shot.
    pub artillery_damage: i32,
    /// Seconds between artillery shots.
    pub artillery_cooldown_secs: f64,
    /// Match hard timeout in seconds. On timeout the side with more units alive
    /// wins; equal counts draw.
    pub match_timeout_secs: f64,
    /// Broadcast mode: "snapshot" (full state each tick) or "delta"
    /// (only changed units each tick).
    pub broadcast_mode: BroadcastMode,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            tick_rate: 10,
            move_probability_per_second: 0.30,
            pawn_move_range: 1,
            unit_hp: 2,
            melee_damage: 1,
            melee_cooldown_secs: 2.0,
            artillery_range: 8,
            artillery_damage: 1,
            artillery_cooldown_secs: 3.0,
            match_timeout_secs: 300.0,
            broadcast_mode: BroadcastMode::Snapshot,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BroadcastMode {
    Snapshot,
    Delta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LifecycleConfig {
    /// Unstarted lobby matches older than this are garbage collected.
    pub lobby_ttl_secs: f64,
    /// Finished matches are removed this long after they end.
    pub finished_ttl_secs: f64,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            lobby_ttl_secs: 600.0,
            finished_ttl_secs: 300.0,
        }
    }
}

impl SimulationConfig {
    /// Ticks between two melee hits.
    pub fn melee_cooldown_ticks(&self) -> u32 {
        secs_to_ticks(self.melee_cooldown_secs, self.tick_rate)
    }

    /// Ticks between two artillery shots.
    pub fn artillery_cooldown_ticks(&self) -> u32 {
        secs_to_ticks(self.artillery_cooldown_secs, self.tick_rate)
    }

    /// Match duration in ticks before the timeout rule kicks in.
    pub fn match_timeout_ticks(&self) -> u64 {
        secs_to_ticks(self.match_timeout_secs, self.tick_rate) as u64
    }

    /// Per-tick resolution probability for a queued move:
    /// `1 - (1 - p)^(1/tick_rate)` — exactly `move_probability_per_second`
    /// over one second of ticks.
    pub fn move_probability_per_tick(&self) -> f64 {
        1.0 - (1.0 - self.move_probability_per_second).powf(1.0 / self.tick_rate as f64)
    }
}

fn secs_to_ticks(secs: f64, tick_rate: u32) -> u32 {
    ((secs * tick_rate as f64).round() as u32).max(1)
}

impl Config {
    /// Load config from `HAVEN_CONFIG` or `./config.toml`.
    pub fn load() -> Result<Config, String> {
        let path = std::env::var("HAVEN_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
        Self::from_path(Path::new(&path))
    }

    pub fn from_path(path: &Path) -> Result<Config, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).map_err(|e| format!("cannot parse config: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        let sim = &self.simulation;
        if !(1..=240).contains(&sim.tick_rate) {
            return Err("simulation.tick_rate must be 1..=240".into());
        }
        if !(0.0 < sim.move_probability_per_second && sim.move_probability_per_second <= 1.0) {
            return Err("simulation.move_probability_per_second must be in (0, 1]".into());
        }
        if sim.pawn_move_range == 0 {
            return Err("simulation.pawn_move_range must be >= 1".into());
        }
        if sim.unit_hp <= 0 || sim.melee_damage <= 0 || sim.artillery_damage <= 0 {
            return Err("hp and damage values must be positive".into());
        }
        if sim.melee_cooldown_secs <= 0.0 || sim.artillery_cooldown_secs <= 0.0 {
            return Err("cooldowns must be positive".into());
        }
        if sim.artillery_range == 0 || sim.artillery_range > self.board.size {
            return Err("simulation.artillery_range must be within 1..=board.size".into());
        }
        if sim.match_timeout_secs <= 0.0 {
            return Err("simulation.match_timeout_secs must be positive".into());
        }
        if !(4..=16).contains(&self.board.size) {
            return Err("board.size must be 4..=16".into());
        }
        if self.board.pawn_row_depth == 0 || self.board.pawn_row_depth * 2 >= self.board.size {
            return Err("board.pawn_row_depth must leave at least one empty row per side".into());
        }
        if self.server.max_matches == 0 {
            return Err("server.max_matches must be >= 1".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default()
            .validate()
            .expect("default config must validate");
    }

    #[test]
    fn per_tick_probability_compounds_to_per_second() {
        let cfg = SimulationConfig::default();
        let per_tick = cfg.move_probability_per_tick();
        let per_second = 1.0 - (1.0 - per_tick).powi(cfg.tick_rate as i32);
        assert!((per_second - cfg.move_probability_per_second).abs() < 1e-9);
    }

    #[test]
    fn cooldown_conversions() {
        let cfg = SimulationConfig::default();
        assert_eq!(cfg.melee_cooldown_ticks(), 20); // 2.0s @ 10Hz
        assert_eq!(cfg.artillery_cooldown_ticks(), 30); // 3.0s @ 10Hz
        assert_eq!(cfg.match_timeout_ticks(), 3000); // 300s @ 10Hz
    }

    #[test]
    fn partial_toml_takes_defaults() {
        let cfg: Config = toml::from_str("[simulation]\ntick_rate = 5").unwrap();
        assert_eq!(cfg.simulation.tick_rate, 5);
        assert_eq!(cfg.simulation.unit_hp, 2);
        assert_eq!(cfg.server.port, 8080);
        cfg.validate().unwrap();
    }
}
