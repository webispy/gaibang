//! The binary end to end against a local server: the key only from the environment and never in the output,
//! one line per series and a summary, a rejected key stops the run (exit 4), a missing key is a usage error.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

const KEY: &str = "abcdef0123456789abcdef0123456789";

/// Answers each request with the next reply; keeps each request (line, headers and body).
fn server(replies: Vec<(&'static str, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    std::thread::spawn(move || {
        for (stream, (status, body)) in listener.incoming().zip(replies) {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut request, mut length) = (String::new(), 0);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                request.push_str(&line);
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();
            request.push_str(&String::from_utf8_lossy(&sent));
            log.lock().unwrap().push(request);
            let reply =
                format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (base, seen)
}

fn state_dir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("tagubong-cli-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fred(endpoint: &str, key: Option<&str>, series: &[&str]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tagubong"));
    let endpoint = format!("{endpoint}/fred/series/observations");
    command.args(["fred", "observations", "--start", "2026-09-06", "--endpoint", &endpoint, "--state-dir"]);
    command.arg(state_dir()).env_remove("FRED_API_KEY");
    for s in series {
        command.args(["--series", s]);
    }
    if let Some(key) = key {
        command.env("FRED_API_KEY", key);
    }
    let output = command.output().unwrap();
    let text = |b: &[u8]| String::from_utf8_lossy(b).to_string();
    (output.status.code().unwrap_or(-1), text(&output.stdout), text(&output.stderr))
}

#[test]
fn each_series_is_a_line_then_a_summary_and_the_key_never_shows() {
    let (endpoint, seen) = server(vec![
        ("200 OK", r#"{"observations":[{"date":"2026-10-20","value":"4.01"}]}"#),
        ("400 Bad Request", r#"{"error_code":400,"error_message":"Bad Request.  The series does not exist."}"#),
    ]);
    let (code, out, err) = fred(&endpoint, Some(KEY), &["DGS10", "NOPE"]);
    assert_eq!(code, 0, "{err}");
    let lines: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["observations"][0]["value"], "4.01");
    assert_eq!(lines[1]["status"], "error");
    assert_eq!((lines[2]["kind"].as_str(), lines[2]["requests"].as_u64()), (Some("summary"), Some(2)));
    assert!(!out.contains(KEY) && !err.contains(KEY));
    assert!(seen.lock().unwrap()[0].contains("series_id=DGS10&api_key="));
}

#[test]
fn a_rejected_key_stops_the_run_and_a_missing_key_is_a_usage_error() {
    let (endpoint, seen) = server(vec![(
        "400 Bad Request",
        r#"{"error_code":400,"error_message":"Bad Request.  The value for variable api_key is not registered."}"#,
    )]);
    let (code, out, _) = fred(&endpoint, Some(KEY), &["DGS10", "DFF"]);
    assert_eq!(code, 4);
    assert!(out.lines().last().unwrap().contains(r#""stopped":"key_rejected""#));
    assert_eq!(seen.lock().unwrap().len(), 1);
    let (code, out, err) = fred(&endpoint, None, &["DGS10"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("FRED_API_KEY is not set"));
    let (code, _, err) = fred(&endpoint, Some(KEY), &["DGS10&api_key=x"]);
    assert_eq!(code, 2, "{err}");
    let (code, _, _) = fred("http://example.org", Some(KEY), &["DGS10"]);
    assert_eq!(code, 2); // the key would leave over plain http
}

const APP_KEY: &str = "app-key-0123";
const SECRET: &str = "app-secret-4567";

fn kis(endpoint: &str, secret: Option<&str>, stdin: &str) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tagubong"))
        .args(["kis", "get", "--endpoint", endpoint, "--state-dir"])
        .arg(state_dir())
        .env_clear()
        .env("KIS_APP_KEY", APP_KEY)
        .envs(secret.map(|s| ("KIS_APP_SECRET", s)))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    let text = |b: &[u8]| String::from_utf8_lossy(b).to_string();
    (output.status.code().unwrap_or(-1), text(&output.stdout), text(&output.stderr))
}

#[test]
fn kis_issues_a_token_then_answers_each_request_line_and_never_prints_a_credential() {
    let (endpoint, seen) = server(vec![
        ("200 OK", r#"{"access_token":"token-abc","expires_in":86400}"#),
        ("200 OK", r#"{"rt_cd":"0","output":[{"stck_bsop_date":"20261021"}]}"#),
        ("500 Internal Server Error", r#"{"rt_cd":"1","msg_cd":"OPSQ2001","msg1":"TIME LIMIT 00:00 ~ 15:40"}"#),
    ]);
    let request = r#"{"id":"005930","path":"/uapi/domestic-stock/v1/quotations/inquire-investor","tr_id":"FHKST01010900","params":{"FID_INPUT_ISCD":"005930"}}"#;
    let (code, out, err) = kis(&endpoint, Some(SECRET), &format!("{request}\n{request}\n"));
    assert_eq!(code, 0, "{err}");
    let lines: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines[0]["payload"]["output"][0]["stck_bsop_date"], "20261021");
    assert_eq!(lines[1]["error"], "FHKST01010900: OPSQ2001 TIME LIMIT 00:00 ~ 15:40");
    assert_eq!((lines[2]["token_issued"].as_u64(), lines[2]["requests"].as_u64()), (Some(1), Some(3)));
    for secret in [APP_KEY, SECRET, "token-abc"] {
        assert!(!out.contains(secret) && !err.contains(secret), "{secret}");
    }
    let seen = seen.lock().unwrap();
    assert!(seen[0].starts_with("POST /oauth2/tokenP") && seen[0].contains(SECRET));
    assert!(seen[1].contains("authorization: Bearer token-abc") && seen[1].contains("FID_INPUT_ISCD=005930"));
}

#[test]
fn kis_without_its_secret_is_a_usage_error() {
    let (code, out, err) = kis("http://127.0.0.1:9", None, "");
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("KIS_APP_SECRET is not set"));
}

#[test]
fn a_keyed_api_reads_its_key_from_the_environment_and_answers_each_line() {
    let (endpoint, seen) = server(vec![("200 OK", r#"{"list":[{"rcept_no":"2026"}],"echo":"law-key-0001"}"#)]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_tagubong"))
        .args(["get", "--api", "law", "--endpoint", &format!("{endpoint}/DRF/"), "--state-dir"])
        .arg(state_dir())
        .env_clear()
        .env("LAW_OC", "law-key-0001")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(br#"{"id":"a","path":"lawSearch.do","params":{"target":"eflaw"}}"#).unwrap();
    let output = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{out}");
    let first: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!((first["id"].as_str(), first["status"].as_str()), (Some("a"), Some("ok")));
    assert!(!out.contains("law-key-0001"));
    let first = seen.lock().unwrap()[0].clone();
    assert!(first.starts_with("GET /DRF/lawSearch.do?target=eflaw&OC=law-key-0001 "), "{first}");
}
