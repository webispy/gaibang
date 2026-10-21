//! tagubong (타구봉): calls official APIs with the caller's own key and prints each answer as one JSON line.
//! Keys come from the environment only and never appear in the output, an error or a log (some APIs take the key
//! in the query string, so request URLs are never printed either). Requests to one API are spaced and cooled
//! down through the same state directory as dongnyang, so every process on the machine keeps one pace per API.

pub use gaibang_core::{clock, http, state};

pub mod caller;
pub mod fred;
pub mod keyed;
pub mod kis;
pub mod toss;
