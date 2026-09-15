//! Expression engine for synth column rules (issue #70).
//!
//! Parser + whitelist validation + exact `rust_decimal` evaluation for
//! `derive` expressions and branch predicates. Contract: see
//! `docs/plans/2026-09-15-synth-rules-v1-extension.md` sections 2 and 3
//! (validation V7-V10, AC2 of issue #70).
