//! Pump.fun new-token listener.
//!
//! `logsSubscribe` → `getTransaction` → parse → bounded channel.
//! Hot paths never panic and never block the socket loop.

pub mod events;
pub mod geyser;
pub mod parser;
pub mod screen;
pub mod stream;
#[cfg(test)]
mod test_fixture;
pub mod types;

pub use geyser::{GeyserError, GeyserSubscriber, YELLOWSTONE_TOKEN_ENV};
pub use migration::{MigratedPool, MigrationDestination, PumpSwapPool, RaydiumV4Pool};
pub use parser::{
    PUMP_CREATE_DISCRIMINATOR, PUMP_PROGRAM_ID, PUMP_PROGRAM_ID_STR, PumpCreateInstructionArgs,
    PumpParseError, PumpTokenLaunchEvent, decode_ix_data, is_pump_create_ix, is_pump_create_v2_ix,
    parse_pump_create, parse_pump_create_v2,
};
pub use screen::{
    BundleAnalysis, BundleDetector, BundleVerdict, DecodedIx, DetectedMigration, DevScreenConfig,
    DevVerdict, EntryGate, GATE_POLL_INTERVAL, GateOutcome, MAX_TRACED_BUYERS, MicroSignals,
    MigrationReport, PUMP_BUY_DISCRIMINATOR, PUMP_BUY_V2_DISCRIMINATOR, PUMP_SELL_DISCRIMINATOR,
    RpcScreener, SYSTEM_PROGRAM_ID, ScreenError, SwapRecord, SwapSide, TapePoint,
    count_dev_creates, extract_pump_swaps, find_funding_source, fixture_wallet, initialize2_amm_id,
    judge_developer, micro_signals, migrate_pool_id, parse_address, read_curve_swaps,
    screen_developer, tape_volume_base_units,
};
pub use stream::{ListenerError, PumpFunListener, backoff_duration};
pub use types::{
    DEFAULT_HTTP_RPC_URL, DEFAULT_WS_RPC_URL, ListenerConfig, ListenerEvent, ListenerStats,
};
