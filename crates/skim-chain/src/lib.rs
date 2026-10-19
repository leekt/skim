//! skim's chain data, shared by the `skim` binary and skim-hub.
//!
//! - [`chaindata`]: headers, bodies, receipts, withdrawals and BALs from EL peers, each verified
//!   against its header; log scans over a hash-linked header range; transaction lookup and
//!   sending; what each retained structure holds ([`chaindata::MemPart`]).
//! - [`history`]: the post-Glamsterdam header store and the log matches block streams asked for
//!   (`--history local`).

pub mod chaindata;
pub mod history;
