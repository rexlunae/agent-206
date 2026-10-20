//! A push or fetch over git's smart HTTP protocol (version 0 of
//! `git-receive-pack` and `git-upload-pack`), without running git: the
//! reference advertisement a server answers with, the update command and
//! pack a push sends and the server's report, and a fetch's request and the
//! pack it receives.
//!
//! Only what one fast-forward push of one branch, or a fetch of one
//! branch, needs is implemented. The
//! pack carries whole objects (no deltas), which every server accepts.
//! Everything a server sends is bounded by the caller and parsed strictly;
//! text a server reports is cleaned of control characters and shortened
//! before it is passed on.
#![forbid(unsafe_code)]
mod fetch;
mod pack;
mod pkt;
mod wire;

pub use fetch::{MAX_HAVES, Stored, check_complete, fetch_request, parse_fetch, store_pack};
pub use pack::{Limits, Objects, PackError, objects, write_pack};
pub use wire::{
    Advertisement, Report, Service, Update, WireError, parse_advertisement, parse_report,
};
