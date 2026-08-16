//! Connectors to Polymarket's public market-data surfaces.
//!
//! Read-only by construction: this module speaks to the Gamma metadata API,
//! the CLOB REST read endpoints, and the public `market` WebSocket channel.
//! It holds no keys, signs nothing, and has no code path that can place,
//! modify or cancel an order.

pub mod api;
pub mod market_discovery;
pub mod parser;
pub mod websocket;
