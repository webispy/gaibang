//! What gaibang's tools share: one state directory per machine (each pace key's next request slot, cool-downs
//! after a block or a rate limit, failures in a row), the clock behind a trait so tests never wait, and HTTP.

pub mod clock;
pub mod http;
pub mod state;
