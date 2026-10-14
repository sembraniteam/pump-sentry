//! Adaptive TP/SL risk engine: LLM inference first, heuristic fallback
//! when inference exceeds 300ms, errors, or returns out-of-schema JSON.

pub mod engine;
pub mod fallback;
pub mod schema;
pub mod strategy;

pub use engine::{DecisionSource, RiskDecision, RiskEngine, RiskEngineBuilder, RiskEngineError};
pub use fallback::evaluate_heuristic_fallback;
pub use schema::{Action, RiskProfile, TokenMetrics, build_prompt};
pub use strategy::{RUNNER_MOONBAG_DEFAULT, SCALP_TP_MAX, SCALP_TP_MIN, StrategyConfig, StrategyMode, resolve_strategy};
