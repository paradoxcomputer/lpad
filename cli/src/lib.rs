//! Library surface of the `lpad` CLI.
//!
//! The `lpad` binary (`main.rs`) is a thin clap dispatcher over these modules:
//! * [`fmt`] - pure parsing/formatting helpers (account/program ids, Q64.64
//!   weights, hex).
//! * [`online`] - the wallet-backed command layer (open wallet → read state /
//!   build-sign-submit txs via `lpad_sdk`).
//! * [`network`] - which sequencer this wallet talks to: the remembered choice,
//!   the first-run picker, and the deployment check that follows a pick.
//!
//! Exposing them as a library makes the helpers unit-testable and lets the
//! integration tests reuse the exact parsers the binary uses.

pub mod fmt;
pub mod network;
pub mod online;
pub mod ui;
