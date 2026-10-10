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

pub use parser::{
    PUMP_CREATE_DISCRIMINATOR, PUMP_PROGRAM_ID, PUMP_PROGRAM_ID_STR, PumpCreateInstructionArgs,
    PumpParseError, PumpTokenLaunchEvent, is_pump_create_ix, parse_pump_create,
};
pub use screen::{
    DevScreenConfig, DevVerdict, EntryGate, GATE_POLL_INTERVAL, GateOutcome,
    PUMP_BUY_DISCRIMINATOR, PUMP_SELL_DISCRIMINATOR, RpcScreener, ScreenError, SwapRecord,
    SwapSide, count_dev_creates, extract_pump_swaps, fixture_wallet, judge_developer,
    parse_address, read_curve_swaps, screen_developer,
};
pub use stream::{ListenerError, PumpFunListener, backoff_duration};
pub use types::{ListenerConfig, ListenerEvent, ListenerStats};
