//! What skim gives back to the network: peers' eth/snap requests are answered from verified
//! recent chain data and held bytecode (see `skim_el::serve`).

use std::sync::Arc;

use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes};
use skim_el::{BlockBody, Receipt, ServeSource, serve::BlockHashOrNumber};
use skim_state::Store;

use crate::chaindata::ChainData;

pub struct Served {
    pub chain: Arc<ChainData>,
    pub store: Arc<Store>,
}

impl ServeSource for Served {
    fn header(&self, id: BlockHashOrNumber) -> Option<Header> {
        self.chain.cached_header(id)
    }
    fn body(&self, hash: B256) -> Option<BlockBody> {
        self.chain.cached_body(hash)
    }
    fn receipts(&self, hash: B256) -> Option<Vec<Receipt>> {
        self.chain.cached_receipts(hash)
    }
    fn bal(&self, hash: B256) -> Option<Bytes> {
        self.chain.cached_bal(hash)
    }
    fn code(&self, hash: B256) -> Option<Bytes> {
        self.store.code(&hash)
    }
    fn earliest(&self) -> Option<u64> {
        self.chain.served_from()
    }
}
