//! 토스증권 Open API: any GET of its REST API, one request a line, each in its rate-limit group. Only one access
//! token is valid per client (issuing a new one revokes the previous at once), so the token is kept in the shared
//! state (0600) and every process uses it until TOKEN_MARGIN before it expires; an answer that the token expired
//! or was revoked issues a new one once and asks again. Each group is paced apart (its own key in the state), a
//! 429 is waited out per its Retry-After and a GET answered 5xx is asked again, up to RETRIES times.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::caller::{Caller, STATE_UNAVAILABLE};
use crate::clock::Clock;
use crate::http::Transport;
use crate::keyed::encode;
use crate::state::State;

pub const API: &str = "toss";
pub const ID_ENV: &str = "TOSS_CLIENT_ID";
pub const SECRET_ENV: &str = "TOSS_CLIENT_SECRET";
pub const ENDPOINT: &str = "https://openapi.tossinvest.com";
/// Requests a second per rate-limit group, from the official overview (2026-10); the server may lower them.
pub const GROUPS: &[(&str, u32)] = &[
    ("AUTH", 5),
    ("MARKET_DATA_CHART", 20),
    ("MARKET_INDICATOR_CHART", 5),
    ("MARKET_INFO", 3),
    ("STOCK", 5),
    ("SECTOR", 5),
];
pub const TOKEN_MARGIN_SECS: i64 = 300;
pub const REISSUE_CODES: &[&str] = &["expired-token", "token-revoked", "invalid-token"];
pub const RETRIES: u32 = 3;
pub const TOKEN_UNAVAILABLE: &str = "token_unavailable";

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Value,
    pub path: String,
    pub params: Vec<(String, String)>,
    pub group: String,
}

impl Request {
    /// `{"id": any, "path": "/api/v1/…", "params": {"name": "value"}, "group": "STOCK"}`.
    pub fn parse(line: &str) -> Result<Request, String> {
        let value: Value = serde_json::from_str(line).map_err(|_| "a request line is not JSON".to_string())?;
        let path = value["path"].as_str().unwrap_or_default().to_string();
        let segments_ok = path.trim_start_matches("/api/v1/").split('/').all(|s| {
            !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        });
        if !path.starts_with("/api/v1/") || path.len() > 200 || !segments_ok {
            return Err(format!("not a Toss API path: {path:?}"));
        }
        let group = value["group"].as_str().unwrap_or_default().to_string();
        if group == "AUTH" || !GROUPS.iter().any(|(g, _)| *g == group) {
            return Err(format!("not a Toss rate-limit group: {group:?}"));
        }
        let mut params = vec![];
        for (k, v) in value["params"].as_object().into_iter().flatten() {
            let Some(v) = v.as_str() else { return Err(format!("parameter {k} is not a string")) };
            if !(1..=64).contains(&k.len()) || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("not a parameter name: {k:?}"));
            }
            params.push((k.clone(), v.to_string()));
        }
        Ok(Request { id: value["id"].clone(), path, params, group })
    }
}

pub struct Toss<'a> {
    callers: BTreeMap<&'static str, Caller<'a>>,
    pub endpoint: String,
    id: String,
    secret: String,
    token: Option<String>,
    pub issued: u32,
    stopped: Option<&'static str>,
}

impl<'a> Toss<'a> {
    pub fn new(
        transport: &'a dyn Transport,
        state: &'a State,
        clock: &'a dyn Clock,
        user_agent: &str,
        endpoint: &str,
        id: String,
        secret: String,
    ) -> Toss<'a> {
        let callers = GROUPS
            .iter()
            .map(|(group, rate)| {
                let interval = Duration::from_secs_f64(1.0 / *rate as f64);
                let mut caller = Caller::new(API, transport, state, clock, interval, user_agent.to_string());
                caller.pace_key = Some(format!("api-toss-{}", group.to_ascii_lowercase()));
                (caller.retries, caller.rate_retries, caller.server_retries) = (1, RETRIES, RETRIES);
                // a 401 of a data request may be an expired or revoked token (asked again with a new one)
                caller.refusals_stop = *group == "AUTH";
                (*group, caller)
            })
            .collect();
        let endpoint = endpoint.trim_end_matches('/').to_string();
        Toss { callers, endpoint, id, secret, token: None, issued: 0, stopped: None }
    }

    pub fn requests(&self) -> u32 {
        self.callers.values().map(Caller::requests).sum()
    }

    pub fn statuses(&self) -> BTreeMap<String, u32> {
        let mut all = BTreeMap::new();
        for caller in self.callers.values() {
            for (status, n) in caller.statuses() {
                *all.entry(status).or_default() += n;
            }
        }
        all
    }

    pub fn stopped(&self) -> Option<&'static str> {
        self.stopped.or_else(|| self.callers.values().find_map(Caller::stopped))
    }

    fn stop(&mut self, reason: &'static str) -> &'static str {
        self.stopped.get_or_insert(reason);
        reason
    }

    pub fn scrub(&self, text: &str) -> String {
        let mut text = text.to_string();
        for secret in [Some(&self.id), Some(&self.secret), self.token.as_ref()].into_iter().flatten() {
            if !secret.is_empty() {
                text = text.replace(secret.as_str(), "[key]");
            }
        }
        text
    }

    /// The shared token with more than TOKEN_MARGIN left (not `stale`), else a new one stored for the others.
    fn token(&mut self, stale: Option<&str>) -> Result<String, &'static str> {
        if let (Some(token), None) = (&self.token, stale) {
            return Ok(token.clone());
        }
        let auth = &self.callers["AUTH"];
        let now = auth.clock.now_millis() / 1000;
        let url = format!("{}/oauth2/token", self.endpoint);
        let body = format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            encode(&self.id),
            encode(&self.secret)
        );
        let issued = auth.state.with_credential(API, |stored| {
            let cached = stored.and_then(|t| serde_json::from_str::<Value>(t).ok()).and_then(|v| {
                let token = v["access_token"].as_str()?.to_string();
                (Some(token.as_str()) != stale && v["expires_at"].as_i64()? - TOKEN_MARGIN_SECS > now).then_some(token)
            });
            if let Some(token) = cached {
                return (None, Ok((token, false)));
            }
            let headers = [
                ("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ];
            let response = match auth.send(&url, &headers, Some(body.as_bytes())) {
                Ok(response) => response,
                Err(reason) => return (None, Err(reason)),
            };
            let payload: Value = serde_json::from_slice(&response.body).unwrap_or_default();
            let token = payload["access_token"].as_str().filter(|t| !t.is_empty() && response.status == 200);
            let Some(token) = token else { return (None, Err(TOKEN_UNAVAILABLE)) };
            let lifetime = payload["expires_in"].as_i64().unwrap_or(3600);
            let stored = json!({"access_token": token, "expires_at": now + lifetime}).to_string();
            (Some(stored), Ok((token.to_string(), true)))
        });
        let (token, new) = match issued {
            Ok(Ok(found)) => found,
            Ok(Err(reason)) => return Err(self.stop(reason)),
            Err(_) => return Err(self.stop(STATE_UNAVAILABLE)),
        };
        self.issued += new as u32;
        self.token = Some(token.clone());
        Ok(token)
    }

    fn ask(&mut self, request: &Request, token: &str) -> Result<(u16, Value, Option<String>), &'static str> {
        let query: Vec<String> = request.params.iter().map(|(k, v)| format!("{k}={}", encode(v))).collect();
        let url =
            format!("{}{}{}{}", self.endpoint, request.path, if query.is_empty() { "" } else { "?" }, query.join("&"));
        let headers = [
            ("Authorization".to_string(), format!("Bearer {token}")),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        let caller = &self.callers[request.group.as_str()];
        let response = caller.send(&url, &headers, None)?;
        let payload = serde_json::from_slice::<Value>(&response.body).unwrap_or(Value::Null);
        Ok((response.status, payload, response.header("x-request-id").map(String::from)))
    }

    /// The error code of an answer: the API envelope's `error.code`, or the OAuth2 `error` string.
    fn code(payload: &Value) -> String {
        payload["error"]["code"].as_str().or(payload["error"].as_str()).unwrap_or("unknown").to_string()
    }

    /// One request: a `response` line with the body (status ok), Toss's error (status error), or why it stopped.
    pub fn get(&mut self, request: &Request, now: impl Fn() -> String) -> Value {
        let mut line = Map::new();
        line.insert("kind".into(), json!("response"));
        line.insert("api".into(), json!(API));
        line.insert("id".into(), request.id.clone());
        let status = match self.answer(request) {
            Err(reason) => reason.to_string(),
            Ok((http_status, payload, request_id)) => {
                line.insert("http_status".into(), json!(http_status));
                if http_status == 200 && payload.is_object() {
                    let clean = serde_json::from_str(&self.scrub(&payload.to_string())).unwrap_or(Value::Null);
                    line.insert("payload".into(), clean);
                    "ok".to_string()
                } else {
                    let request_id = payload["error"]["requestId"].as_str().map(String::from).or(request_id);
                    line.insert(
                        "code".into(),
                        json!(self.scrub(&Self::code(&payload)).chars().take(80).collect::<String>()),
                    );
                    line.insert("request_id".into(), json!(request_id));
                    "error".to_string()
                }
            }
        };
        if let Some(caller) = self.callers.get(request.group.as_str()) {
            caller.count(&status);
        }
        line.insert("status".into(), json!(status));
        line.insert("fetched_at".into(), json!(now()));
        Value::Object(line)
    }

    fn answer(&mut self, request: &Request) -> Result<(u16, Value, Option<String>), &'static str> {
        if let Some(reason) = self.stopped() {
            return Err(reason);
        }
        let token = self.token(None)?;
        let mut answer = self.ask(request, &token)?;
        if answer.0 == 401 && REISSUE_CODES.contains(&Self::code(&answer.1).as_str()) {
            let fresh = self.token(Some(&token))?;
            answer = self.ask(request, &fresh)?;
        }
        Ok(answer) // another 401/403 is that request's answer: Toss also refuses one symbol so (not-found)
    }
}

pub fn valid_credential(text: &str) -> bool {
    (1..=512).contains(&text.len()) && text.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::KEY_REJECTED;
    use crate::clock::FakeClock;
    use crate::http::Response;
    use std::sync::Mutex;

    const ID: &str = "client-id-0001";
    const SECRET: &str = "client-secret-0001";

    /// An answer to give: status, body, headers.
    type Answer = (u16, String, Vec<(String, String)>);

    struct Fake {
        issued: Mutex<u32>,
        token_status: u16,
        answers: Mutex<Vec<Answer>>,
        seen: Mutex<Vec<(String, String)>>,
    }

    impl Fake {
        fn new(answers: Vec<(u16, &str)>) -> Fake {
            let answers = answers.into_iter().rev().map(|(s, b)| (s, b.to_string(), vec![])).collect();
            Fake { issued: Mutex::new(0), token_status: 200, answers: Mutex::new(answers), seen: Mutex::new(vec![]) }
        }
    }

    impl Transport for Fake {
        fn get(&self, url: &str, headers: &[(String, String)], _: usize) -> Response {
            let auth = headers.iter().find(|(k, _)| k == "Authorization").map(|(_, v)| v.clone()).unwrap_or_default();
            self.seen.lock().unwrap().push((url.to_string(), auth));
            let (status, body, headers) =
                self.answers.lock().unwrap().pop().unwrap_or((200, r#"{"result":{"ok":true}}"#.into(), vec![]));
            Response { status, headers, body: body.into_bytes(), ..Response::default() }
        }

        fn post(&self, _: &str, _: &[(String, String)], body: &[u8], _: usize) -> Response {
            assert!(String::from_utf8_lossy(body).contains("client_secret=client-secret-0001"));
            if self.token_status != 200 {
                return Response {
                    status: self.token_status,
                    body: br#"{"error":"invalid_client"}"#.to_vec(),
                    ..Response::default()
                };
            }
            let mut n = self.issued.lock().unwrap();
            *n += 1;
            let body = format!(r#"{{"access_token":"tok-{n}","expires_in":3600}}"#).into_bytes();
            Response { status: 200, body, ..Response::default() }
        }
    }

    fn state_in() -> (State, std::path::PathBuf) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("tagubong-toss-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (State::open(&dir).unwrap(), dir)
    }

    fn new_state() -> State {
        state_in().0
    }

    fn request(path: &str, group: &str) -> Request {
        Request::parse(&format!(r#"{{"id":1,"path":"{path}","params":{{"symbols":"005930"}},"group":"{group}"}}"#))
            .unwrap()
    }

    fn run(fake: &Fake, state: &State, clock: &FakeClock, requests: &[Request]) -> (Vec<Value>, u32, u32) {
        let mut toss = Toss::new(fake, state, clock, "t/1", ENDPOINT, ID.into(), SECRET.into());
        let lines = requests.iter().map(|r| toss.get(r, || "now".into())).collect();
        (lines, toss.issued, toss.requests())
    }

    #[test]
    fn one_token_for_every_process_and_a_revoked_one_is_issued_again_once() {
        let revoked = r#"{"error":{"code":"token-revoked","requestId":"r1"}}"#;
        let (fake, state, clock) =
            (Fake::new(vec![(200, r#"{"result":[1]}"#), (401, revoked)]), new_state(), FakeClock::new(1_000_000_000));
        let (lines, issued, _) = run(&fake, &state, &clock, &[request("/api/v1/stocks", "STOCK")]);
        assert_eq!((lines[0]["payload"]["result"][0].as_i64(), issued), (Some(1), 1));
        let (lines, issued, _) = run(&fake, &state, &clock, &[request("/api/v1/stocks", "STOCK")]);
        assert_eq!((lines[0]["status"].as_str(), issued), (Some("ok"), 1)); // revoked: a new one, asked again
        let seen = fake.seen.lock().unwrap();
        assert_eq!(
            seen.iter().map(|(_, a)| a.as_str()).collect::<Vec<_>>(),
            ["Bearer tok-1", "Bearer tok-1", "Bearer tok-2"]
        );
        assert!(seen[0].0.starts_with("https://openapi.tossinvest.com/api/v1/stocks?symbols=005930"));
    }

    #[test]
    fn a_rate_limit_is_waited_out_and_a_server_error_asked_again() {
        let fake = Fake::new(vec![(429, "{}"), (503, "{}"), (200, r#"{"result":2}"#)]);
        fake.answers.lock().unwrap()[2].2 = vec![("Retry-After".into(), "7".into())];
        let (state, clock) = (new_state(), FakeClock::new(1_000_000_000));
        let (lines, _, requests) = run(&fake, &state, &clock, &[request("/api/v1/candles", "MARKET_DATA_CHART")]);
        assert_eq!((lines[0]["status"].as_str(), requests), (Some("ok"), 4)); // the token and three tries
        let slept = clock.slept();
        assert!(slept.contains(&Duration::from_secs(7)) && slept.contains(&Duration::from_secs(1)), "{slept:?}");
    }

    #[test]
    fn groups_are_paced_apart_and_errors_keep_code_and_request_id_only() {
        let bad = format!(r#"{{"error":{{"code":"invalid-parameter {SECRET}","requestId":"req-9"}}}}"#);
        let fake = Fake::new(vec![(400, &bad), (200, "{}")]);
        let ((state, dir), clock) = (state_in(), FakeClock::new(1_000_000_000));
        let (lines, _, _) =
            run(&fake, &state, &clock, &[request("/api/v1/sectors", "SECTOR"), request("/api/v1/stocks", "STOCK")]);
        assert_eq!(lines[1]["status"], "ok");
        assert_eq!(
            (lines[0]["code"].as_str(), lines[0]["request_id"].as_str()),
            (Some("invalid-parameter [key]"), Some("req-9"))
        );
        assert!(clock.slept().is_empty()); // SECTOR then STOCK: two queues, no wait
        let keys: Vec<String> = std::fs::read_dir(dir.join("keys"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            keys.iter().any(|k| k == "api-toss-sector.json") && keys.iter().any(|k| k == "api-toss-stock.json"),
            "{keys:?}"
        );
    }

    #[test]
    fn a_refused_client_stops_at_the_token() {
        let mut fake = Fake::new(vec![]);
        fake.token_status = 401;
        let (state, clock) = (new_state(), FakeClock::new(1_000_000_000));
        let (lines, issued, _) =
            run(&fake, &state, &clock, &[request("/api/v1/stocks", "STOCK"), request("/api/v1/stocks", "STOCK")]);
        assert_eq!(
            (lines[0]["status"].as_str(), lines[1]["status"].as_str(), issued),
            (Some(KEY_REJECTED), Some(KEY_REJECTED), 0)
        );
        assert!(fake.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn requests_outside_the_api_or_its_groups_are_refused() {
        for bad in [
            r#"{"path":"/oauth2/token","group":"STOCK"}"#,
            r#"{"path":"/api/v1/../x","group":"STOCK"}"#,
            r#"{"path":"/api/v1/stocks","group":"AUTH"}"#,
            r#"{"path":"/api/v1/stocks","group":"NOPE"}"#,
            r#"{"path":"/api/v1/stocks?x=1","group":"STOCK"}"#,
        ] {
            assert!(Request::parse(bad).is_err(), "{bad}");
        }
    }
}
