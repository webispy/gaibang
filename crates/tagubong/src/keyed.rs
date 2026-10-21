//! APIs that take a key and answer a GET (ECOS, 공공데이터포털, 열린국회, 법제처, KRX): one table row each
//! with the base URL, where the key goes (a query parameter, a `{key}` path segment or a header), the pace, and
//! whether a 401/403 is a refused key (stop) or one service's refusal (KRX approves each service on its own).
//! A request is a path under the base and parameters; the answer's body comes back as text (UTF-8) or base64,
//! with the key scrubbed from the text. The caller parses it: these APIs report errors in their own ways.

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::caller::Caller;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyPlace {
    /// `?<name>=<key>`
    Query(&'static str),
    /// the path's `{key}` segment
    Path,
    /// a request header
    Header(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub api: &'static str,
    pub name: &'static str,
    pub key_env: &'static str,
    pub base: &'static str,
    pub key: KeyPlace,
    pub interval: Duration,
    /// A 401/403 refuses the key for every request (stop), else only that service (an error line).
    pub refusal_stops: bool,
}

pub const SPECS: &[Spec] = &[
    Spec {
        api: "ecos",
        name: "한국은행 ECOS",
        key_env: "ECOS_API_KEY",
        base: "https://ecos.bok.or.kr/api/",
        key: KeyPlace::Path,
        interval: Duration::from_millis(500),
        refusal_stops: true,
    },
    Spec {
        api: "datagokr",
        name: "공공데이터포털",
        key_env: "DATA_GO_KR_KEY",
        base: "https://apis.data.go.kr/",
        key: KeyPlace::Query("serviceKey"),
        interval: Duration::from_millis(1000),
        refusal_stops: true,
    },
    Spec {
        api: "assembly",
        name: "열린국회정보",
        key_env: "ASSEMBLY_API_KEY",
        base: "https://open.assembly.go.kr/portal/openapi/",
        key: KeyPlace::Query("KEY"),
        interval: Duration::from_millis(1000),
        refusal_stops: true,
    },
    Spec {
        api: "law",
        name: "법제처 국가법령정보",
        key_env: "LAW_OC",
        base: "https://www.law.go.kr/DRF/",
        key: KeyPlace::Query("OC"),
        interval: Duration::from_millis(1000),
        refusal_stops: true,
    },
    Spec {
        api: "krx",
        name: "KRX Open API",
        key_env: "KRX_API_KEY",
        base: "https://data-dbg.krx.co.kr/svc/apis/",
        key: KeyPlace::Header("AUTH_KEY"),
        interval: Duration::from_millis(300),
        refusal_stops: false,
    },
];

// OpenDART is not here: opendart.fss.or.kr accepts only TLS 1.2 with RSA key exchange (AES128-GCM-SHA256 and
// the like, checked 2026-10-21), which rustls does not offer, so no request of ours completes.

pub fn spec(api: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.api == api)
}

pub fn valid_key(key: &str) -> bool {
    (1..=512).contains(&key.len()) && key.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
}

pub fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Value,
    pub path: String,
    pub params: Vec<(String, String)>,
    pub accept: String,
}

impl Request {
    /// `{"id": any, "path": "<under the base>", "params": {"name": "value"}, "accept": "…"}`.
    pub fn parse(line: &str, spec: &Spec) -> Result<Request, String> {
        let value: Value = serde_json::from_str(line).map_err(|_| "a request line is not JSON".to_string())?;
        let path = value["path"].as_str().unwrap_or_default().to_string();
        let segments_ok = path.split('/').all(|s| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && (s == "{key}" || s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
        });
        if path.len() > 300 || !segments_ok {
            return Err(format!("not a path under the {} base: {path:?}", spec.api));
        }
        if path.contains("{key}") != (spec.key == KeyPlace::Path) {
            return Err("only an API that takes the key in the path has a {key} segment".into());
        }
        let mut params = vec![];
        for (k, v) in value["params"].as_object().into_iter().flatten() {
            let Some(v) = v.as_str() else { return Err(format!("parameter {k} is not a string")) };
            let named = (1..=64).contains(&k.len()) && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !named || matches!(spec.key, KeyPlace::Query(name) if name.eq_ignore_ascii_case(k)) {
                return Err(format!("not a parameter name here: {k:?}"));
            }
            params.push((k.clone(), v.to_string()));
        }
        let accept = value["accept"].as_str().unwrap_or("application/json, application/xml, */*").to_string();
        if accept.chars().any(|c| c.is_control()) {
            return Err("accept holds a control character".into());
        }
        Ok(Request { id: value["id"].clone(), path, params, accept })
    }
}

/// One request: the body (status ok for 2xx, error otherwise), or why the run stopped.
pub fn get(
    caller: &Caller,
    spec: &Spec,
    endpoint: &str,
    key: &str,
    request: &Request,
    now: impl Fn() -> String,
) -> Value {
    let mut line = Map::new();
    line.insert("kind".into(), json!("response"));
    line.insert("api".into(), json!(spec.api));
    line.insert("id".into(), request.id.clone());
    let path = request.path.replace("{key}", &encode(key));
    let mut query: Vec<String> = request.params.iter().map(|(k, v)| format!("{k}={}", encode(v))).collect();
    if let KeyPlace::Query(name) = spec.key {
        query.push(format!("{name}={}", encode(key)));
    }
    let url = format!("{endpoint}{path}{}{}", if query.is_empty() { "" } else { "?" }, query.join("&"));
    let mut headers = vec![("Accept".to_string(), request.accept.clone())];
    if let KeyPlace::Header(name) = spec.key {
        headers.push((name.to_string(), key.to_string()));
    }
    let status = match caller.send(&url, &headers, None) {
        Err(reason) => reason.to_string(),
        Ok(response) => {
            line.insert("http_status".into(), json!(response.status));
            line.insert("content_type".into(), json!(response.header("content-type").unwrap_or_default()));
            match String::from_utf8(response.body.clone()) {
                Ok(text) => line.insert("body".into(), json!(scrub(&text, key))),
                Err(_) => line.insert("body_base64".into(), json!(base64(&response.body))),
            };
            if response.truncated {
                "too_large".into()
            } else if (200..300).contains(&response.status) {
                "ok".into()
            } else {
                line.insert("error".into(), json!(format!("{}: HTTP {}", request.path, response.status)));
                "error".into()
            }
        }
    };
    caller.count(&status);
    line.insert("status".into(), json!(status));
    line.insert("fetched_at".into(), json!(now()));
    Value::Object(line)
}

/// The key, raw and as it was encoded into the request, out of a text about to be printed.
pub fn scrub(text: &str, key: &str) -> String {
    let mut text = text.to_string();
    for form in [key.to_string(), encode(key)] {
        if !form.is_empty() {
            text = text.replace(&form, "[key]");
        }
    }
    text
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n =
            (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::KEY_REJECTED;
    use crate::clock::FakeClock;
    use crate::http::{Response, Transport};
    use crate::state::State;
    use std::sync::Mutex;

    const KEY: &str = "ab/c+d=e";

    /// A request as sent: the URL and the headers.
    type Sent = (String, Vec<(String, String)>);

    struct Fake {
        answers: Mutex<Vec<(u16, Vec<u8>)>>,
        seen: Mutex<Vec<Sent>>,
    }

    impl Fake {
        fn new(answers: Vec<(u16, &[u8])>) -> Fake {
            let answers = answers.into_iter().rev().map(|(s, b)| (s, b.to_vec())).collect();
            Fake { answers: Mutex::new(answers), seen: Mutex::new(vec![]) }
        }
    }

    impl Transport for Fake {
        fn get(&self, url: &str, headers: &[(String, String)], _: usize) -> Response {
            self.seen.lock().unwrap().push((url.to_string(), headers.to_vec()));
            let (status, body) = self.answers.lock().unwrap().pop().unwrap_or((200, b"{}".to_vec()));
            Response { status, body, ..Response::default() }
        }
    }

    fn new_state() -> State {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("tagubong-keyed-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        State::open(&dir).unwrap()
    }

    fn run(api: &str, fake: &Fake, lines: &[&str]) -> Vec<Value> {
        let spec = spec(api).unwrap();
        let (state, clock) = (new_state(), FakeClock::new(1_000_000_000));
        let mut caller = Caller::new(spec.api, fake, &state, &clock, spec.interval, "t/1".into());
        (caller.retries, caller.refusals_stop) = (1, spec.refusal_stops);
        lines
            .iter()
            .map(|l| get(&caller, spec, spec.base, KEY, &Request::parse(l, spec).unwrap(), || "now".into()))
            .collect()
    }

    #[test]
    fn the_key_goes_where_each_api_takes_it_and_never_into_a_line() {
        let echo = format!("{{\"echo\": \"{KEY} {}\"}}", encode(KEY));
        let fake = Fake::new(vec![(200, echo.as_bytes())]);
        let lines = run(
            "datagokr",
            &fake,
            &[r#"{"id":1,"path":"1160100/GetStocDiviInfoService_V2/getDiviInfo_V2","params":{"pageNo":"2"}}"#],
        );
        assert_eq!(lines[0]["body"], "{\"echo\": \"[key] [key]\"}");
        let (url, _) = fake.seen.lock().unwrap()[0].clone();
        assert_eq!(
            url,
            "https://apis.data.go.kr/1160100/GetStocDiviInfoService_V2/getDiviInfo_V2?pageNo=2&serviceKey=ab%2Fc%2Bd%3De"
        );

        let fake = Fake::new(vec![]);
        run(
            "ecos",
            &fake,
            &[r#"{"path":"StatisticSearch/{key}/json/kr/1/100/817Y002/D/20260901/20261021/010210000"}"#],
        );
        assert!(
            fake.seen.lock().unwrap()[0]
                .0
                .starts_with("https://ecos.bok.or.kr/api/StatisticSearch/ab%2Fc%2Bd%3De/json/")
        );

        let fake = Fake::new(vec![]);
        run("krx", &fake, &[r#"{"path":"idx/krx_dd_trd","params":{"basDd":"20261021"}}"#]);
        let (url, headers) = fake.seen.lock().unwrap()[0].clone();
        assert!(!url.contains("ab%2F") && headers.contains(&("AUTH_KEY".to_string(), KEY.to_string())));
    }

    #[test]
    fn a_binary_body_comes_back_as_base64_and_an_error_status_is_an_error_line() {
        let fake = Fake::new(vec![(200, &[0x50, 0x4b, 0x03, 0x04, 0xff]), (404, b"nope")]);
        let lines = run("assembly", &fake, &[r#"{"path":"corpCode.xml"}"#, r#"{"path":"list.json"}"#]);
        assert_eq!((lines[0]["status"].as_str(), lines[0]["body_base64"].as_str()), (Some("ok"), Some("UEsDBP8=")));
        assert_eq!(
            (lines[1]["status"].as_str(), lines[1]["error"].as_str()),
            (Some("error"), Some("list.json: HTTP 404"))
        );
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn a_refusal_stops_the_run_except_where_each_service_is_approved_alone() {
        let fake = Fake::new(vec![(401, b"no"), (200, b"{}")]);
        let lines = run("law", &fake, &[r#"{"path":"list.json"}"#, r#"{"path":"list.json"}"#]);
        assert_eq!(
            (lines[0]["status"].as_str(), lines[1]["status"].as_str()),
            (Some(KEY_REJECTED), Some(KEY_REJECTED))
        );
        let fake = Fake::new(vec![(401, b"no"), (200, b"{}")]);
        let lines = run("krx", &fake, &[r#"{"path":"idx/a"}"#, r#"{"path":"etp/b"}"#]);
        assert_eq!((lines[0]["status"].as_str(), lines[1]["status"].as_str()), (Some("error"), Some("ok")));
    }

    #[test]
    fn requests_that_would_leave_the_base_or_move_the_key_are_refused() {
        let law = spec("law").unwrap();
        for bad in [
            r#"{"path":"../x"}"#,
            r#"{"path":"//evil.example/x"}"#,
            r#"{"path":"list.json?x=1"}"#,
            r#"{"path":"list.json","params":{"OC":"mine"}}"#,
            r#"{"path":"a/{key}/b"}"#,
            r#"{"path":"list.json","accept":"a\r\nX: y"}"#,
        ] {
            assert!(Request::parse(bad, law).is_err(), "{bad}");
        }
        assert!(Request::parse(r#"{"path":"StatisticSearch/x"}"#, spec("ecos").unwrap()).is_err());
    }
}
