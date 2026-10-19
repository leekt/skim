//! skim's minimal consensus-layer gossip listener (PROJECT.md §3.2, M0 Spike A).
//!
//! - [`node`]: discv5 + libp2p swarm that joins eth2 gossip as a listen-mostly peer.
//! - [`rpc`]: the req/resp subset needed to be kept as a peer (Status, MetaData, Ping, Goodbye).
//! - [`gossip`]: topics, snappy, eth2 message ids.
//! - [`ssz`]: offset parser for `SignedExecutionPayloadEnvelope` and the Gloas beacon block.
//! - [`verify`]: rebuild the EL header from a payload and check its hash (ties the BAL to it).

pub mod gossip;
pub mod light;
pub mod network;
pub mod node;
pub mod rpc;
pub mod serve;
pub mod ssz;
pub mod verify;

pub use libp2p::Multiaddr;
pub use node::{ClEvent, ClNode, ClPeer, NodeConfig, NodeStats, SessionEnd};
