//! Pump.fun new-token listener.
//!
//! Architecture: `logsSubscribe` (WebSocket) → `getTransaction` (HTTP RPC)
//! → in-place Borsh parse → bounded `mpsc` channel.
//!
//! Hot paths never panic, never block the socket read loop on network I/O,
//! and use `try_send` with drop-on-full backpressure accounting.

pub mod parser;
pub mod screen;
pub mod stream;
pub mod types;

pub use migration::{MigrationDestination, RaydiumV4Pool};
pub use parser::{
    PUMP_CREATE_DISCRIMINATOR, PUMP_PROGRAM_ID, PUMP_PROGRAM_ID_STR, PumpCreateInstructionArgs,
    PumpCreateV2InstructionArgs, PumpParseError, PumpTokenLaunchEvent, decode_ix_data,
    is_pump_create_ix, is_pump_create_v2_ix, parse_pump_create, parse_pump_create_v2,
};
pub use screen::{
    BundleAnalysis, BundleDetector, BundleVerdict, DetectedMigration, DevScreenConfig, DevVerdict,
    EntryGate, GATE_POLL_INTERVAL, GateOutcome, MAX_TRACED_BUYERS, MigrationReport,
    PUMP_BUY_DISCRIMINATOR, PUMP_BUY_V2_DISCRIMINATOR, PUMP_SELL_DISCRIMINATOR, RpcScreener,
    SYSTEM_PROGRAM_ID, ScreenError, SwapRecord, SwapSide, count_dev_creates, extract_pump_swaps,
    find_funding_source, fixture_wallet, initialize2_amm_id, judge_developer, parse_address,
    read_curve_swaps, screen_developer,
};
pub use stream::{ListenerError, PumpFunListener, backoff_duration};
pub use types::{ListenerConfig, ListenerEvent, ListenerStats};
