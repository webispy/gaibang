//! One API's guarded requests: its cool-down checked before and after waiting for its turn, the answer noted
//! (429 cools the API down for every process, failures in a row too), and nothing sent once the run stopped.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::Duration;

use crate::clock::Clock;
use crate::http::{Response, Transport};
use crate::state::State;

pub const RATE_COOLDOWN_SECS: i64 = 600;
pub const FAILURE_COOLDOWN_SECS: i64 = 3600;
pub const MAX_FAILURES: u32 = 5;
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
pub const RETRY_WAIT: Duration = Duration::from_secs(2);
pub const RETRY_AFTER_MAX: Duration = Duration::from_secs(60);
pub const DEFAULT_USER_AGENT: &str =
    concat!("tagubong/", env!("CARGO_PKG_VERSION"), " (+https://github.com/webispy/gaibang)");

/// Why a run stopped: nothing more is sent to the API in it.
pub const COOLDOWN: &str = "cooldown";
pub const STATE_UNAVAILABLE: &str = "state_unavailable";
pub const KEY_REJECTED: &str = "key_rejected";
pub const RATE_LIMITED: &str = "rate_limited";
pub const UNAVAILABLE: &str = "unavailable";

pub struct Caller<'a> {
    pub api: &'static str,
    pub transport: &'a dyn Transport,
    pub state: &'a State,
    pub clock: &'a dyn Clock,
    pub interval: Duration,
    pub user_agent: String,
    /// A request that got no answer is sent this many more times (RETRY_WAIT apart) before the run stops.
    pub retries: u32,
    /// A 5xx with a body is the API's own answer (KIS reports its errors so), not a failure.
    pub answers_5xx: bool,
    /// A 401/403 refuses the key for the whole run; when false it is one request's answer.
    pub refusals_stop: bool,
    /// A 429 is waited out (its Retry-After, at most RETRY_AFTER_MAX) and sent again this many times before the
    /// API cools down; a GET answered 5xx is sent again this many times, 1, 2, 4 … s apart.
    pub rate_retries: u32,
    pub server_retries: u32,
    /// The pace key in the shared state; `api-<api>` unless an API paces groups of requests apart.
    pub pace_key: Option<String>,
    requests: Cell<u32>,
    stopped: RefCell<Option<&'static str>>,
    statuses: RefCell<BTreeMap<String, u32>>,
}

impl<'a> Caller<'a> {
    pub fn new(
        api: &'static str,
        transport: &'a dyn Transport,
        state: &'a State,
        clock: &'a dyn Clock,
        interval: Duration,
        user_agent: String,
    ) -> Caller<'a> {
        Caller {
            api,
            transport,
            state,
            clock,
            interval,
            user_agent,
            retries: 0,
            answers_5xx: false,
            refusals_stop: true,
            rate_retries: 0,
            server_retries: 0,
            pace_key: None,
            requests: Cell::new(0),
            stopped: RefCell::new(None),
            statuses: RefCell::new(BTreeMap::new()),
        }
    }

    /// The pace key in the shared state: one per API, apart from any outlet's.
    pub fn key(&self) -> String {
        self.pace_key.clone().unwrap_or_else(|| format!("api-{}", self.api))
    }

    pub fn stopped(&self) -> Option<&'static str> {
        *self.stopped.borrow()
    }

    pub fn stop(&self, reason: &'static str) {
        self.stopped.borrow_mut().get_or_insert(reason);
    }

    pub fn requests(&self) -> u32 {
        self.requests.get()
    }

    pub fn count(&self, status: &str) {
        *self.statuses.borrow_mut().entry(status.to_string()).or_default() += 1;
    }

    pub fn statuses(&self) -> BTreeMap<String, u32> {
        self.statuses.borrow().clone()
    }

    /// GET `url` (which may hold the key: it is never printed). Err: the run is stopped and why.
    pub fn get(&self, url: &str, accept: &str) -> Result<Response, &'static str> {
        self.send(url, &[("Accept".to_string(), accept.into())], None)
    }

    /// GET, or POST `body`, with the User-Agent and `headers` (which may hold credentials: never printed).
    pub fn send(&self, url: &str, headers: &[(String, String)], body: Option<&[u8]>) -> Result<Response, &'static str> {
        if let Some(reason) = self.stopped() {
            return Err(reason);
        }
        let mut all = vec![("User-Agent".to_string(), self.user_agent.clone())];
        all.extend_from_slice(headers);
        let (mut dropped, mut limited, mut failed) = (0, 0, 0);
        let response = loop {
            if let Err(reason) = self.turn() {
                self.stop(reason);
                return Err(reason);
            }
            self.requests.set(self.requests.get() + 1);
            let response = match body {
                Some(body) => self.transport.post(url, &all, body, MAX_RESPONSE_BYTES),
                None => self.transport.get(url, &all, MAX_RESPONSE_BYTES),
            };
            if response.status == 0 && dropped < self.retries {
                dropped += 1;
                self.clock.sleep(RETRY_WAIT);
                continue;
            }
            if response.status == 429 && limited < self.rate_retries {
                limited += 1;
                let after = response.header("retry-after").and_then(|v| v.trim().parse::<f64>().ok());
                let wait = after.filter(|s| s.is_finite() && *s >= 0.0).map(Duration::from_secs_f64);
                self.clock.sleep(wait.unwrap_or(Duration::from_secs(1 << limited)).min(RETRY_AFTER_MAX));
                continue;
            }
            if response.status >= 500 && body.is_none() && failed < self.server_retries {
                failed += 1;
                self.clock.sleep(Duration::from_secs(1 << (failed - 1)));
                continue;
            }
            break response;
        };
        if let Err(reason) = self.note(&response) {
            self.stop(reason);
            return Err(reason);
        }
        Ok(response)
    }

    fn turn(&self) -> Result<(), &'static str> {
        let key = self.key();
        let cooling = |state: &State| match state.cooldown(&key, self.clock) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(COOLDOWN),
            Err(_) => Err(STATE_UNAVAILABLE),
        };
        cooling(self.state)?;
        self.state.wait_turn(&key, self.interval, self.clock).map_err(|_| STATE_UNAVAILABLE)?;
        cooling(self.state)
    }

    fn note(&self, response: &Response) -> Result<(), &'static str> {
        let key = self.key();
        let cool = |secs: i64, reason: &str, stop: &'static str| -> Result<(), &'static str> {
            self.state.set_cooldown(&key, secs, reason, self.clock).map_err(|_| STATE_UNAVAILABLE)?;
            Err(stop)
        };
        match response.status {
            429 => {
                let secs = response
                    .header("retry-after")
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .unwrap_or(RATE_COOLDOWN_SECS);
                cool(secs.clamp(60, 24 * 3600), "429", RATE_LIMITED)
            }
            401 | 403 if self.refusals_stop => Err(KEY_REJECTED),
            500..=599 if self.answers_5xx && !response.body.is_empty() && !response.incomplete => {
                self.state.answered(&key).map_err(|_| STATE_UNAVAILABLE)
            }
            _ if response.status == 0 || response.status >= 500 || response.incomplete => {
                match self.state.failure(&key) {
                    Ok(n) if n >= MAX_FAILURES => cool(FAILURE_COOLDOWN_SECS, "no answer or 5xx in a row", UNAVAILABLE),
                    Ok(_) => Err(UNAVAILABLE),
                    Err(_) => Err(STATE_UNAVAILABLE),
                }
            }
            _ => self.state.answered(&key).map_err(|_| STATE_UNAVAILABLE),
        }
    }
}
