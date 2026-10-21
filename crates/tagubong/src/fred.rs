//! FRED (Federal Reserve Bank of St. Louis): series observations.
//! API: https://fred.stlouisfed.org/docs/api/fred/series_observations.html (120 requests a minute per key).
//! The key goes in the query string, so the URL is never printed; an error keeps FRED's status and error code only.

use std::time::Duration;

use chrono::NaiveDate;
use serde_json::{Map, Value, json};

use crate::caller::{Caller, KEY_REJECTED};

pub const API: &str = "fred";
pub const KEY_ENV: &str = "FRED_API_KEY";
pub const ENDPOINT: &str = "https://api.stlouisfed.org/fred/series/observations";
/// FRED allows 120 a minute; a little slower than that.
pub const INTERVAL: Duration = Duration::from_millis(600);

/// A series id as FRED writes them (DGS10, DEXKOUS, NIKKEI225): nothing that would change the query.
pub fn valid_series(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

pub fn valid_key(key: &str) -> bool {
    (1..=128).contains(&key.len()) && key.chars().all(|c| c.is_ascii_alphanumeric())
}

/// One series from `start`: a line with every observation (a day FRED has no value for reads "."), or the
/// error. `now` stamps the line after the answer, so an as-of reader never sees a value before it was fetched.
pub fn observations(
    caller: &Caller,
    endpoint: &str,
    key: &str,
    series: &str,
    start: NaiveDate,
    now: impl Fn() -> String,
) -> Value {
    let mut line = Map::new();
    line.insert("kind".into(), json!("observations"));
    line.insert("api".into(), json!(API));
    line.insert("series".into(), json!(series));
    line.insert("start".into(), json!(start.to_string()));
    let url = format!("{endpoint}?series_id={series}&api_key={key}&file_type=json&observation_start={start}");
    let status = match caller.get(&url, "application/json") {
        Err(reason) => reason,
        Ok(response) => {
            line.insert("http_status".into(), json!(response.status));
            let payload = serde_json::from_slice::<Value>(&response.body).ok().filter(Value::is_object);
            match (response.status, payload) {
                (200, Some(payload)) => {
                    let rows: Vec<Value> = payload["observations"]
                        .as_array()
                        .map(|rows| {
                            rows.iter()
                                .filter_map(|row| {
                                    let (day, value) = (row["date"].as_str()?, row["value"].as_str()?);
                                    let value = if value.trim().is_empty() { "." } else { value.trim() };
                                    Some(json!({"date": day, "value": value}))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    line.insert("observations".into(), json!(rows));
                    "ok"
                }
                (_, None) => {
                    line.insert("error".into(), json!("non-JSON response"));
                    "error"
                }
                (code, Some(payload)) => {
                    let message = payload["error_message"].as_str().unwrap_or_default();
                    // an unregistered key is a 400 like a bad series id: told apart without keeping the message
                    if code == 400 && message.contains("api_key") {
                        caller.stop(KEY_REJECTED);
                        KEY_REJECTED
                    } else {
                        let error = payload["error_code"].as_i64().map(|c| format!("error_code {c}"));
                        line.insert("error".into(), json!(error.unwrap_or_else(|| "error".into())));
                        "error"
                    }
                }
            }
        }
    };
    caller.count(status);
    line.insert("status".into(), json!(status));
    line.insert("fetched_at".into(), json!(now()));
    Value::Object(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::{COOLDOWN, RATE_LIMITED, UNAVAILABLE};
    use crate::clock::FakeClock;
    use crate::http::{Response, Transport};
    use crate::state::State;
    use std::sync::Mutex;

    const KEY: &str = "abcdef0123456789abcdef0123456789";

    struct Fake {
        answers: Mutex<Vec<Response>>,
        asked: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(answers: Vec<(u16, &str)>) -> Fake {
            let answers = answers
                .into_iter()
                .rev()
                .map(|(status, body)| Response { status, body: body.as_bytes().to_vec(), ..Response::default() })
                .collect();
            Fake { answers: Mutex::new(answers), asked: Mutex::new(vec![]) }
        }
    }

    impl Transport for Fake {
        fn get(&self, url: &str, _: &[(String, String)], _: usize) -> Response {
            self.asked.lock().unwrap().push(url.to_string());
            self.answers.lock().unwrap().pop().unwrap_or_default()
        }
    }

    fn new_state() -> State {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("tagubong-fred-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        State::open(&dir).unwrap()
    }

    fn read(fake: &Fake, state: &State, clock: &FakeClock, series: &[&str]) -> Vec<Value> {
        let caller = Caller::new(API, fake, state, clock, INTERVAL, "t/1".into());
        let start = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();
        series.iter().map(|s| observations(&caller, ENDPOINT, KEY, s, start, || "now".into())).collect()
    }

    #[test]
    fn observations_come_with_holidays_as_a_dot_and_requests_are_spaced() {
        let body = r#"{"observations":[{"date":"2026-10-19","value":"4.01"},{"date":"2026-10-20","value":"."},
            {"date":"2026-10-21","value":" "},{"date":7}]}"#;
        let (fake, state, clock) = (Fake::new(vec![(200, body), (200, body)]), new_state(), FakeClock::new(1_000_000));
        let lines = read(&fake, &state, &clock, &["DGS10", "DFF"]);
        assert_eq!(lines[0]["status"], "ok");
        assert_eq!(
            lines[0]["observations"],
            json!([{"date":"2026-10-19","value":"4.01"},{"date":"2026-10-20","value":"."},{"date":"2026-10-21","value":"."}])
        );
        assert_eq!(clock.slept(), vec![INTERVAL]);
        assert!(fake.asked.lock().unwrap()[1].contains("series_id=DFF&"));
    }

    #[test]
    fn a_series_error_is_its_own_but_a_rejected_key_stops_and_the_key_is_never_printed() {
        let bad_series = r#"{"error_code":400,"error_message":"Bad Request.  The series does not exist."}"#;
        let bad_key =
            r#"{"error_code":400,"error_message":"Bad Request.  The value for variable api_key is not registered."}"#;
        let (fake, state, clock) =
            (Fake::new(vec![(400, bad_series), (400, bad_key)]), new_state(), FakeClock::new(1_000_000));
        let lines = read(&fake, &state, &clock, &["NOPE", "DGS10", "DFF"]);
        assert_eq!((lines[0]["status"].as_str(), lines[0]["error"].as_str()), (Some("error"), Some("error_code 400")));
        assert_eq!(lines[1]["status"], KEY_REJECTED);
        assert_eq!(lines[2]["status"], KEY_REJECTED); // not sent
        assert_eq!(fake.asked.lock().unwrap().len(), 2);
        for line in &lines {
            assert!(!line.to_string().contains(KEY) && !line.to_string().contains("api_key"), "{line}");
        }
    }

    #[test]
    fn a_rate_limit_cools_the_api_down_for_every_process_and_no_answer_stops_the_run() {
        let (fake, state, clock) = (Fake::new(vec![(429, "")]), new_state(), FakeClock::new(1_000_000));
        assert_eq!(read(&fake, &state, &clock, &["DGS10"])[0]["status"], RATE_LIMITED);
        let later = Fake::new(vec![(200, "{}")]);
        assert_eq!(read(&later, &state, &clock, &["DGS10"])[0]["status"], COOLDOWN); // another run: nothing sent
        assert!(later.asked.lock().unwrap().is_empty());
        let (dropped, fresh) = (Fake::new(vec![(0, "")]), new_state());
        let lines = read(&dropped, &fresh, &clock, &["DGS10", "DFF"]);
        assert_eq!((lines[0]["status"].as_str(), lines[1]["status"].as_str()), (Some(UNAVAILABLE), Some(UNAVAILABLE)));
        assert_eq!(dropped.asked.lock().unwrap().len(), 1);
    }

    #[test]
    fn ids_and_keys_that_would_change_the_query_are_refused() {
        assert!(valid_series("DGS10") && valid_series("NIKKEI225"));
        assert!(!valid_series("DGS10&api_key=x") && !valid_series("") && !valid_series("a b"));
        assert!(valid_key(KEY) && !valid_key("a&b") && !valid_key(""));
    }
}
