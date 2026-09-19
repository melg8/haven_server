//! haven_server library crate.
//!
//! Splitting the lib from the bin lets integration tests (and future
//! tooling) build the router and drive the simulation without spawning a
//! process. The binary in `main.rs` is a thin bootstrap around this library.

pub mod api;
pub mod config;
pub mod game_state;
pub mod input;
pub mod match_manager;
pub mod network;
pub mod simulation;
