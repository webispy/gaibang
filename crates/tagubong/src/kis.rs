//! KIS (한국투자증권) Open API: any GET of its REST API, one request a line, with the access token issued once
//! and shared. A token is valid for a day and issuance is rate-limited, so it is kept in the shared state (0600)
//! and reused by every process until TOKEN_MARGIN before it expires; an answer saying the token is invalid or
//! expired issues a new one once and asks again. KIS reports its errors in the body, often with a 5xx.
//! The app key, secret and token travel in headers and never appear in a line: KIS echoes request fields in
//! messages, so every line is scrubbed of them.

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::caller::{Caller, STATE_UNAVAILABLE};

pub const API: &str = "kis";
pub const KEY_ENV: &str = "KIS_APP_KEY";
pub const SECRET_ENV: &str = "KIS_APP_SECRET";
pub const ENDPOINT: &str = "https://openapi.koreainvestment.com:9443";
pub const INTERVAL: Duration = Duration::from_millis(200);
pub const TOKEN_MARGIN_SECS: i64 = 30 * 60;
/// invalid token, expired token
pub const TOKEN_ERRORS: &[&str] = &["EGW00121", "EGW00123"];
/// more requests a second than allowed: once more after a second
pub const RATE_ERROR: &str = "EGW00201";
pub const TOKEN_UNAVAILABLE: &str = "token_unavailable";

/// One request as the caller writes it on stdin.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Value,
    pub path: String,
    pub tr_id: String,
    pub params: Vec<(String, String)>,
    pub tr_cont: String,
}

fn word(text: &str, extra: &[char], max: usize) -> bool {
    (1..=max).contains(&text.len()) && text.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

impl Request {
    /// `{"id": any, "path": "/uapi/…", "tr_id": "…", "params": {"K": "v"}, "tr_cont": ""}`; Err says what is wrong.
    pub fn parse(line: &str) -> Result<Request, String> {
        let value: Value = serde_json::from_str(line).map_err(|_| "a request line is not JSON".to_string())?;
        let text = |k: &str| value[k].as_str().unwrap_or_default().to_string();
        let (path, tr_id, tr_cont) = (text("path"), text("tr_id"), text("tr_cont"));
        if !path.starts_with("/uapi/") || !word(&path, &['/', '-', '_'], 200) {
            return Err(format!("not a KIS API path: {path:?}"));
        }
        if !word(&tr_id, &[], 20) || !(tr_cont.is_empty() || word(&tr_cont, &[], 2)) {
            return Err("tr_id or tr_cont is not a KIS code".into());
        }
        let mut params = vec![];
        for (k, v) in value["params"].as_object().into_iter().flatten() {
            let Some(v) = v.as_str() else { return Err(format!("parameter {k} is not a string")) };
            if !word(k, &['_'], 64) {
                return Err(format!("not a parameter name: {k:?}"));
            }
            params.push((k.clone(), v.to_string()));
        }
        Ok(Request { id: value["id"].clone(), path, tr_id, params, tr_cont })
    }
}

pub fn valid_credential(text: &str) -> bool {
    (1..=512).contains(&text.len()) && text.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub struct Kis<'a> {
    pub caller: &'a Caller<'a>,
    pub endpoint: String,
    key: String,
    secret: String,
    token: Option<String>,
    pub issued: u32,
}

impl<'a> Kis<'a> {
    pub fn new(caller: &'a Caller<'a>, endpoint: &str, key: String, secret: String) -> Kis<'a> {
        Kis { caller, endpoint: endpoint.trim_end_matches('/').to_string(), key, secret, token: None, issued: 0 }
    }

    /// Every credential out of a text that is about to be printed.
    pub fn scrub(&self, text: &str) -> String {
        let mut text = text.to_string();
        for secret in [Some(&self.key), Some(&self.secret), self.token.as_ref()].into_iter().flatten() {
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
        let now = self.caller.clock.now_millis() / 1000;
        let caller = self.caller;
        let (url, body) = (
            format!("{}/oauth2/tokenP", self.endpoint),
            json!({"grant_type": "client_credentials", "appkey": self.key, "appsecret": self.secret}).to_string(),
        );
        let issued = caller.state.with_credential(API, |stored| {
            let cached = stored.and_then(|t| serde_json::from_str::<Value>(t).ok()).and_then(|v| {
                let token = v["access_token"].as_str()?.to_string();
                (Some(token.as_str()) != stale && v["expires_at"].as_i64()? - TOKEN_MARGIN_SECS > now).then_some(token)
            });
            if let Some(token) = cached {
                return (None, Ok((token, false)));
            }
            let headers = [("Content-Type".to_string(), "application/json; charset=utf-8".to_string())];
            let response = match caller.send(&url, &headers, Some(body.as_bytes())) {
                Ok(response) => response,
                Err(reason) => return (None, Err(reason)),
            };
            let payload: Value = serde_json::from_slice(&response.body).unwrap_or_default();
            let Some(token) = payload["access_token"].as_str().filter(|t| !t.is_empty()) else {
                return (None, Err(TOKEN_UNAVAILABLE));
            };
            let lifetime = payload["expires_in"].as_i64().unwrap_or(86_400);
            let stored = json!({"access_token": token, "expires_at": now + lifetime}).to_string();
            (Some(stored), Ok((token.to_string(), true)))
        });
        let (token, new) = match issued {
            Ok(Ok(found)) => found,
            Ok(Err(reason)) => {
                self.caller.stop(reason);
                return Err(reason);
            }
            Err(_) => {
                self.caller.stop(STATE_UNAVAILABLE);
                return Err(STATE_UNAVAILABLE);
            }
        };
        self.issued += new as u32;
        self.token = Some(token.clone());
        Ok(token)
    }

    fn ask(&self, request: &Request, token: &str) -> Result<(u16, Option<String>, Value), &'static str> {
        let query: Vec<String> = request.params.iter().map(|(k, v)| format!("{k}={}", encode(v))).collect();
        let url = format!("{}{}?{}", self.endpoint, request.path, query.join("&"));
        let headers: Vec<(String, String)> = [
            ("Content-Type", "application/json; charset=utf-8"),
            ("authorization", &format!("Bearer {token}")),
            ("appkey", &self.key),
            ("appsecret", &self.secret),
            ("custtype", "P"),
            ("tr_id", &request.tr_id),
            ("tr_cont", &request.tr_cont),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let response = self.caller.send(&url, &headers, None)?;
        let payload = serde_json::from_slice::<Value>(&response.body).ok().filter(Value::is_object);
        Ok((response.status, response.header("tr_cont").map(String::from), payload.unwrap_or(Value::Null)))
    }

    /// One request: a `response` line with the payload (status ok), KIS's error (status error), or why the run
    /// stopped (nothing sent after it).
    pub fn get(&mut self, request: &Request, now: impl Fn() -> String) -> Value {
        let mut line = Map::new();
        line.insert("kind".into(), json!("response"));
        line.insert("api".into(), json!(API));
        line.insert("id".into(), request.id.clone());
        line.insert("tr_id".into(), json!(request.tr_id));
        let status = match self.answer(request) {
            Err(reason) => reason.to_string(),
            Ok((http_status, tr_cont, payload)) => {
                line.insert("http_status".into(), json!(http_status));
                let code = payload["msg_cd"].as_str().unwrap_or_default().to_string();
                if http_status == 200 && payload["rt_cd"].as_str() == Some("0") {
                    line.insert("tr_cont".into(), json!(tr_cont.unwrap_or_default()));
                    let clean = serde_json::from_str(&self.scrub(&payload.to_string())).unwrap_or(Value::Null);
                    line.insert("payload".into(), clean);
                    "ok".into()
                } else {
                    let error = if payload.is_object() {
                        let message = payload["msg1"].as_str().unwrap_or_default().trim().to_string();
                        format!("{}: {code} {message}", request.tr_id)
                    } else {
                        format!("{}: HTTP {http_status}", request.path)
                    };
                    let error: String = self.scrub(&error).chars().take(200).collect();
                    line.insert("msg_cd".into(), json!(code));
                    line.insert("error".into(), json!(error));
                    "error".into()
                }
            }
        };
        self.caller.count(&status);
        line.insert("status".into(), json!(status));
        line.insert("fetched_at".into(), json!(now()));
        Value::Object(line)
    }

    fn answer(&mut self, request: &Request) -> Result<(u16, Option<String>, Value), &'static str> {
        let token = self.token(None)?;
        let mut answer = self.ask(request, &token)?;
        let code = |a: &(u16, Option<String>, Value)| a.2["msg_cd"].as_str().unwrap_or_default().to_string();
        if TOKEN_ERRORS.contains(&code(&answer).as_str()) {
            let fresh = self.token(Some(&token))?; // revoked or expired early: once more with a new one
            answer = self.ask(request, &fresh)?;
        }
        if code(&answer) == RATE_ERROR {
            self.caller.clock.sleep(Duration::from_secs(1));
            answer = self.ask(request, self.token.clone().as_deref().unwrap_or_default())?;
        }
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::{KEY_REJECTED, UNAVAILABLE};
    use crate::clock::FakeClock;
    use crate::http::{Response, Transport};
    use crate::state::State;
    use std::sync::Mutex;

    const KEY: &str = "app-key-0123";
    const SECRET: &str = "app-secret-4567";

    /// Token POSTs answer `token-N`; GETs answer from `answers` in order (status, body), then an ok payload.
    struct Fake {
        issued: Mutex<u32>,
        token_status: u16,
        answers: Mutex<Vec<(u16, String)>>,
        seen: Mutex<Vec<Vec<(String, String)>>>,
    }

    impl Fake {
        fn new(answers: Vec<(u16, &str)>) -> Fake {
            let answers = answers.into_iter().rev().map(|(s, b)| (s, b.to_string())).collect();
            Fake { issued: Mutex::new(0), token_status: 200, answers: Mutex::new(answers), seen: Mutex::new(vec![]) }
        }
    }

    impl Transport for Fake {
        fn get(&self, _: &str, headers: &[(String, String)], _: usize) -> Response {
            self.seen.lock().unwrap().push(headers.to_vec());
            let (status, body) =
                self.answers.lock().unwrap().pop().unwrap_or((200, r#"{"rt_cd":"0","output":[1]}"#.into()));
            let headers = vec![("tr_cont".to_string(), "D".to_string())];
            Response { status, headers, body: body.into_bytes(), ..Response::default() }
        }

        fn post(&self, _: &str, _: &[(String, String)], body: &[u8], _: usize) -> Response {
            assert!(String::from_utf8_lossy(body).contains(SECRET));
            if self.token_status != 200 {
                return Response { status: self.token_status, body: b"{}".to_vec(), ..Response::default() };
            }
            let mut n = self.issued.lock().unwrap();
            *n += 1;
            let body = format!(r#"{{"access_token":"token-{n}","expires_in":86400}}"#).into_bytes();
            Response { status: 200, body, ..Response::default() }
        }
    }

    fn new_state() -> State {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("tagubong-kis-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        State::open(&dir).unwrap()
    }

    fn request(tr_id: &str) -> Request {
        Request::parse(&format!(
            r#"{{"id":"005930","path":"/uapi/x/v1/y","tr_id":"{tr_id}","params":{{"FID_INPUT_ISCD":"005930"}}}}"#
        ))
        .unwrap()
    }

    /// One process: a Kis over its own caller; returns the lines and the tokens it issued.
    fn run(fake: &Fake, state: &State, clock: &FakeClock, requests: &[Request]) -> (Vec<Value>, u32) {
        let mut caller = Caller::new(API, fake, state, clock, INTERVAL, "t/1".into());
        (caller.retries, caller.answers_5xx) = (1, true);
        let mut kis = Kis::new(&caller, ENDPOINT, KEY.into(), SECRET.into());
        let lines = requests.iter().map(|r| kis.get(r, || "now".into())).collect();
        (lines, kis.issued)
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
        headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or_default()
    }

    #[test]
    fn processes_share_one_token_until_it_nears_expiry() {
        let (fake, state, clock) = (Fake::new(vec![]), new_state(), FakeClock::new(1_000_000_000));
        let (lines, issued) = run(&fake, &state, &clock, &[request("A1"), request("A1")]);
        assert_eq!((lines[0]["status"].as_str(), lines[1]["tr_cont"].as_str(), issued), (Some("ok"), Some("D"), 1));
        assert_eq!(run(&fake, &state, &clock, &[request("A1")]).1, 0); // another process: the stored token
        clock.advance(Duration::from_secs((86_400 - TOKEN_MARGIN_SECS + 1) as u64));
        assert_eq!(run(&fake, &state, &clock, &[request("A1")]).1, 1);
        let seen = fake.seen.lock().unwrap();
        assert_eq!(header(&seen[0], "authorization"), "Bearer token-1");
        assert_eq!(header(seen.last().unwrap(), "authorization"), "Bearer token-2");
        assert_eq!((header(&seen[0], "appkey"), header(&seen[0], "tr_id")), (KEY, "A1"));
    }

    #[test]
    fn an_expired_token_answer_issues_a_new_one_once_and_asks_again() {
        let expired = r#"{"rt_cd":"1","msg_cd":"EGW00123","msg1":"기간이 만료된 token 입니다."}"#;
        let (fake, state, clock) = (Fake::new(vec![(500, expired)]), new_state(), FakeClock::new(1_000_000_000));
        let (lines, issued) = run(&fake, &state, &clock, &[request("A1")]);
        assert_eq!((lines[0]["status"].as_str(), issued), (Some("ok"), 2));
    }

    #[test]
    fn kis_errors_keep_their_code_and_never_a_credential() {
        let limit = format!(r#"{{"rt_cd":"1","msg_cd":"OPSQ2001","msg1":"TIME LIMIT 00:00 ~ 15:40 {KEY}"}}"#);
        let echo = format!(r#"{{"rt_cd":"0","output":[{{"echo":"{SECRET} token-1"}}]}}"#);
        let fake = Fake::new(vec![(200, &limit), (200, &echo), (500, "oops")]);
        let (state, clock) = (new_state(), FakeClock::new(1_000_000_000));
        let (lines, _) = run(&fake, &state, &clock, &[request("FHPTJ04160001"), request("A1"), request("A1")]);
        assert_eq!(lines[0]["error"], "FHPTJ04160001: OPSQ2001 TIME LIMIT 00:00 ~ 15:40 [key]");
        assert_eq!(lines[1]["payload"]["output"][0]["echo"], "[key] [key]");
        assert_eq!(
            (lines[2]["status"].as_str(), lines[2]["error"].as_str()),
            (Some("error"), Some("/uapi/x/v1/y: HTTP 500"))
        );
        for line in &lines {
            let text = line.to_string();
            assert!(!text.contains(KEY) && !text.contains(SECRET) && !text.contains("token-1"), "{text}");
        }
    }

    #[test]
    fn too_many_a_second_waits_a_second_and_a_dropped_connection_is_tried_once_more() {
        let busy = r#"{"rt_cd":"1","msg_cd":"EGW00201","msg1":"초당 거래건수를 초과하였습니다."}"#;
        let ok = r#"{"rt_cd":"0"}"#;
        let fake = Fake::new(vec![(500, busy), (200, ok), (0, ""), (200, ok), (0, ""), (0, "")]);
        let (state, clock) = (new_state(), FakeClock::new(1_000_000_000));
        let (lines, _) = run(&fake, &state, &clock, &[request("A1"), request("A1"), request("A1"), request("A1")]);
        let statuses: Vec<&str> = lines.iter().map(|l| l["status"].as_str().unwrap()).collect();
        assert_eq!(statuses, ["ok", "ok", UNAVAILABLE, UNAVAILABLE]); // the last one is not sent
        assert!(clock.slept().contains(&Duration::from_secs(1)));
        assert_eq!(fake.seen.lock().unwrap().len(), 6);
    }

    #[test]
    fn a_refused_key_stops_at_the_token() {
        let mut fake = Fake::new(vec![]);
        fake.token_status = 403;
        let (state, clock) = (new_state(), FakeClock::new(1_000_000_000));
        let (lines, issued) = run(&fake, &state, &clock, &[request("A1"), request("A1")]);
        assert_eq!(
            (lines[0]["status"].as_str(), lines[1]["status"].as_str(), issued),
            (Some(KEY_REJECTED), Some(KEY_REJECTED), 0)
        );
        assert!(fake.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn requests_that_would_change_the_url_or_headers_are_refused() {
        assert!(Request::parse(r#"{"path":"/uapi/a/b","tr_id":"X1","params":{"K":"a&b=c"}}"#).is_ok());
        for bad in [
            r#"{"path":"/oauth2/tokenP","tr_id":"X1"}"#,
            r#"{"path":"/uapi/a?b=1","tr_id":"X1"}"#,
            r#"{"path":"/uapi/a","tr_id":"X1\r\nx: y"}"#,
            r#"{"path":"/uapi/a","tr_id":"X1","params":{"K=1&L":"v"}}"#,
            r#"{"path":"/uapi/a","tr_id":"X1","params":{"K":1}}"#,
            "not json",
        ] {
            assert!(Request::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(encode("a&b=c 가"), "a%26b%3Dc%20%EA%B0%80");
    }
}
