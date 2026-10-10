//! Adaptive TP/SL risk engine (see skill `adaptive-risk-prompting`).
//!
//! [`RiskEngine::evaluate`] tries LLM inference first and drops to the
//! deterministic [`evaluate_heuristic_fallback`] when inference exceeds
//! 300ms, errors, or returns out-of-schema JSON.

pub mod engine;
pub mod fallback;
pub mod schema;

pub use engine::{DecisionSource, RiskDecision, RiskEngine, RiskEngineBuilder, RiskEngineError};
pub use fallback::evaluate_heuristic_fallback;
pub use schema::{Action, RiskProfile, TokenMetrics, build_prompt};
