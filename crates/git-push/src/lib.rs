//! A push over git's smart HTTP protocol (version 0 of `git-receive-pack`),
//! without running git: the reference advertisement a server answers with,
//! the update command and pack the client sends, and the server's report.
//!
//! Only what one fast-forward push of one branch needs is implemented. The
//! pack carries whole objects (no deltas), which every server accepts.
//! Everything a server sends is bounded by the caller and parsed strictly;
//! text a server reports is cleaned of control characters and shortened
//! before it is passed on.
#![forbid(unsafe_code)]
mod pack;
mod pkt;
mod wire;

pub use pack::{Limits, Objects, PackError, objects, write_pack};
pub use wire::{Advertisement, Report, Update, WireError, parse_advertisement, parse_report};
