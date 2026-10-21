//! dongnyang (동냥): reads news feeds, sitemaps and article pages politely and prints what each read gives as one
//! JSON line: common fields mapped from the outlet's own, and the outlet's own as `raw`. It keeps no
//! database; the caller stores the lines. See README.md and docs/dongnyang-output.md.

pub use gaibang_core::{clock, http, state};

pub mod decode;
pub mod extract;
pub mod fetch;
pub mod mcp;
pub mod normalize;
pub mod parse;
pub mod robots;
pub mod source;
