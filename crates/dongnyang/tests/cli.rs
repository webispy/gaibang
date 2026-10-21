//! The binary end to end against a local server: separate processes honour one cool-down, a closed output
//! stops the run before the next request, exit codes say why a run ended, and a relative state directory is
//! refused. Slow by design: the pace floor (3 s) holds here too.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Answers by path; counts requests per path.
fn server(routes: Vec<(&'static str, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
            log.lock().unwrap().push(path.clone());
            let reply = routes
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, r)| r.clone())
                .unwrap_or_else(|| reply("404 Not Found", "", ""));
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (host, seen)
}

fn reply(status: &str, headers: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len())
}

fn temp(name: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("dongnyang-cli-{name}-{}-{n}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A source spec for the local server.
fn sources(host: &str) -> std::path::PathBuf {
    let dir = temp("sources");
    std::fs::write(
        dir.join("local.toml"),
        format!(
            "id = \"local\"\noutlet = \"로컬\"\nhosts = [\"{host}\"]\narticle_path = '^/a/\\d+$'\nid_pattern = '^/a/(\\d+)$'\n\
             [article]\nbody = [\"#b\"]\n"
        ),
    )
    .unwrap();
    dir
}

fn run(sources: &std::path::Path, state: &std::path::Path, urls: &[String]) -> (i32, Vec<serde_json::Value>) {
    let output = Command::new(env!("CARGO_BIN_EXE_dongnyang"))
        .args([
            "--sources",
            sources.to_str().unwrap(),
            "article",
            "--source",
            "local",
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .args(urls)
        .output()
        .unwrap();
    let lines = String::from_utf8_lossy(&output.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    (output.status.code().unwrap_or(-1), lines)
}

#[test]
fn a_challenge_stops_this_process_and_the_next_one_sends_nothing() {
    let (host, seen) = server(vec![
        ("/robots.txt", reply("200 OK", "", "User-agent: *\nAllow: /\n")),
        ("/a/1", reply("403 Forbidden", "cf-mitigated: challenge\r\n", "")),
    ]);
    let (specs, state) = (sources(&host), temp("state"));
    let urls: Vec<String> = (1..=3).map(|i| format!("http://{host}/a/{i}")).collect();
    let (code, lines) = run(&specs, &state, &urls);
    assert_eq!(code, 4);
    assert_eq!(lines[0]["status"], "cooldown");
    let summary = lines.last().unwrap();
    assert_eq!(
        (summary["kind"].as_str(), summary["stopped"].as_str(), summary["unsent"].as_u64()),
        (Some("summary"), Some("cooldown"), Some(2))
    );
    assert_eq!(*seen.lock().unwrap(), vec!["/robots.txt", "/a/1"]);
    let (code, lines) = run(&specs, &state, &urls[1..]); // another process, same state
    assert_eq!(code, 4);
    assert_eq!(lines[0]["status"], "cooldown");
    assert_eq!(seen.lock().unwrap().len(), 2); // nothing more was sent
}

#[test]
fn a_closed_output_ends_the_run_before_the_next_request() {
    let page =
        "<html><head><meta property=\"og:title\" content=\"T\"></head><body><div id=\"b\">본문</div></body></html>";
    let (host, seen) = server(vec![
        ("/robots.txt", reply("200 OK", "", "User-agent: *\nAllow: /\n")),
        ("/a/1", reply("200 OK", "Content-Type: text/html; charset=utf-8\r\n", page)),
        ("/a/2", reply("200 OK", "Content-Type: text/html; charset=utf-8\r\n", page)),
    ]);
    let (specs, state) = (sources(&host), temp("state"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_dongnyang"))
        .args([
            "--sources",
            specs.to_str().unwrap(),
            "article",
            "--source",
            "local",
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .args([format!("http://{host}/a/1"), format!("http://{host}/a/2")])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // the reader goes away
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(3));
    assert!(!seen.lock().unwrap().contains(&"/a/2".to_string()));
}

#[test]
fn a_relative_state_directory_is_refused() {
    let output = Command::new(env!("CARGO_BIN_EXE_dongnyang"))
        .args(["article", "--source", "yna", "--state-dir", "fetch-state", "https://www.yna.co.kr/view/AKR1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("absolute"));
}

#[test]
fn the_same_address_twice_is_read_once_and_a_browser_user_agent_is_refused() {
    let page = "<html><body><div id=\"b\">본문</div></body></html>";
    let (host, seen) = server(vec![
        ("/robots.txt", reply("200 OK", "", "User-agent: *\nAllow: /\n")),
        ("/a/1", reply("200 OK", "Content-Type: text/html\r\n", page)),
    ]);
    let (specs, state) = (sources(&host), temp("state"));
    let url = format!("http://{host}/a/1");
    let (code, lines) = run(&specs, &state, &[url.clone(), url.clone()]);
    assert_eq!(code, 0);
    assert_eq!(lines.iter().filter(|l| l["kind"] == "article").count(), 1);
    assert_eq!(seen.lock().unwrap().iter().filter(|p| *p == "/a/1").count(), 1);
    let output = Command::new(env!("CARGO_BIN_EXE_dongnyang"))
        .args([
            "--sources",
            specs.to_str().unwrap(),
            "article",
            "--source",
            "local",
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .args(["--user-agent", "Mozilla/5.0", &url])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn ids_are_computed_without_the_network() {
    use std::io::Write as _;
    let mut child = Command::new(env!("CARGO_BIN_EXE_dongnyang"))
        .arg("ids")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"https://www.yna.co.kr/amp/view/AKR20261013029300003\nhttps://www.etnews.com/20261020000418?SNS=2\nhttps://elsewhere.example/x\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let lines: Vec<serde_json::Value> =
        String::from_utf8_lossy(&output.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines[0]["id"], "yna:AKR20261013029300003");
    assert_eq!(lines[1]["id"], "etnews:20261020000418");
    assert!(lines[2]["id"].is_null());
}

#[test]
fn robots_txt_is_read_for_the_callers_own_product_token() {
    let page = "<html><body><div id=\"b\">본문</div></body></html>";
    let (host, seen) = server(vec![
        ("/robots.txt", reply("200 OK", "", "User-agent: *\nAllow: /\n\nUser-agent: myresearch\nDisallow: /a/\n")),
        ("/a/1", reply("200 OK", "Content-Type: text/html\r\n", page)),
    ]);
    let (specs, state) = (sources(&host), temp("state"));
    let url = format!("http://{host}/a/1");
    let read_as = |agent: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_dongnyang"))
            .args(["--sources", specs.to_str().unwrap(), "article", "--source", "local", "--state-dir"])
            .args([state.to_str().unwrap(), "--user-agent", agent, &url])
            .output()
            .unwrap();
        let first = String::from_utf8_lossy(&output.stdout).lines().next().unwrap().to_string();
        serde_json::from_str::<serde_json::Value>(&first).unwrap()["status"].as_str().unwrap().to_string()
    };
    assert_eq!(read_as("MyResearch/1.0 (+https://example.org/contact)"), "robots");
    assert_eq!(seen.lock().unwrap().iter().filter(|p| *p == "/a/1").count(), 0);
    assert_eq!(read_as("otherbot/2"), "ok"); // the * group
}

fn mcp(specs: &std::path::Path, state: &std::path::Path, messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dongnyang"))
        .args(["--sources", specs.to_str().unwrap(), "mcp", "--state-dir", state.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for message in messages {
        writeln!(stdin, "{message}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&output.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn an_agent_reads_an_article_over_mcp_under_the_same_rules() {
    let page = "<html><head><title>가상 기사</title></head><body><div id=\"b\">본문 한 줄.</div></body></html>";
    let (host, seen) = server(vec![
        ("/robots.txt", reply("200 OK", "", "User-agent: *\nAllow: /\nDisallow: /a/2\n")),
        ("/a/1", reply("200 OK", "Content-Type: text/html\r\n", page)),
    ]);
    let (specs, state) = (sources(&host), temp("state"));
    let call = |id: u32, name: &str, arguments: serde_json::Value| serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
    let answers = mcp(
        &specs,
        &state,
        &[
            serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            call(1, "read_article", serde_json::json!({"url": format!("http://{host}/a/1"), "max_body_chars": 3})),
            call(2, "read_article", serde_json::json!({"url": format!("http://{host}/a/2")})),
            call(3, "read_article", serde_json::json!({"url": "https://elsewhere.example/a/1"})),
        ],
    );
    assert_eq!(answers.len(), 4); // nothing for the notification
    assert_eq!(answers[0]["result"]["protocolVersion"], "2025-03-26");
    let article = &answers[1]["result"]["structuredContent"];
    assert_eq!(
        (article["status"].as_str(), article["body"].as_str(), article["body_cut"].as_bool()),
        (Some("ok"), Some("본문 "), Some(true))
    );
    assert!(article.get("raw").is_none());
    assert_eq!(answers[2]["result"]["structuredContent"]["status"], "robots");
    assert_eq!(answers[3]["result"]["isError"], true);
    assert_eq!(seen.lock().unwrap().iter().filter(|p| p.starts_with("/a/")).count(), 1); // robots kept /a/2 unread
}
