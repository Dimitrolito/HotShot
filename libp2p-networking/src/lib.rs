#![warn(
    clippy::all,
    clippy::pedantic,
    rust_2018_idioms,
    missing_docs,
    clippy::panic
)]
#![allow(
    clippy::option_if_let_else,
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::similar_names,
    clippy::unused_self
)]
//! Library for p2p communication

/// Example message used by the UI library
pub mod message;

/// Network logic
pub mod network;

/// symbols needed to implement a networking instance over libp2p-netorking
pub mod reexport {
    pub use libp2p::Multiaddr;
    pub use libp2p_identity::PeerId;
}
