//! The real HTTP client against a local server: no redirect is followed by the client, bodies are cut at the
//! limit, our headers are sent. And the shared pace holds when several threads take turns at once.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dongnyang::clock::SystemClock;
use dongnyang::http::{Transport, UreqTransport};
use dongnyang::state::State;

/// Serves each connection with the next canned response; records request heads.
fn server(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    std::thread::spawn(move || {
        for (stream, response) in listener.incoming().zip(responses) {
            let mut stream = stream.unwrap();
            let mut head = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            log.lock().unwrap().push(head);
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (base, seen)
}

fn reply(status: &str, headers: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len())
}

#[test]
fn redirects_come_back_to_the_caller_and_headers_are_sent() {
    let (base, seen) = server(vec![reply("301 Moved", "Location: /next\r\n", "")]);
    let transport = UreqTransport::new(Duration::from_secs(5));
    let response = transport.get(&format!("{base}/a"), &[("User-Agent".into(), "dongnyang/test".into())], 1000);
    assert_eq!(response.status, 301);
    assert_eq!(response.header("location"), Some("/next"));
    assert!(seen.lock().unwrap()[0].to_lowercase().contains("user-agent: dongnyang/test"));
}

#[test]
fn bodies_are_cut_at_the_limit_and_errors_keep_their_status() {
    let (base, _) = server(vec![
        reply("200 OK", "", &"x".repeat(100)),
        reply("403 Forbidden", "cf-mitigated: challenge\r\n", "no"),
    ]);
    let transport = UreqTransport::new(Duration::from_secs(5));
    let long = transport.get(&format!("{base}/a"), &[], 10);
    assert_eq!((long.status, long.body.len(), long.truncated), (200, 10, true));
    let refused = transport.get(&format!("{base}/b"), &[], 10);
    assert_eq!((refused.status, refused.header("cf-mitigated")), (403, Some("challenge")));
}

#[test]
fn no_answer_is_status_zero() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // nothing listens there now
    let response = UreqTransport::new(Duration::from_secs(2)).get(&format!("http://{addr}/"), &[], 10);
    assert_eq!(response.status, 0);
}

#[test]
fn threads_with_their_own_state_handles_share_one_pace() {
    let dir = std::env::temp_dir().join(format!("dongnyang-pace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let interval = Duration::from_millis(150);
    let times = Arc::new(Mutex::new(vec![]));
    let start = Instant::now();
    let threads: Vec<_> = (0..3)
        .map(|_| {
            let (dir, times) = (dir.clone(), times.clone());
            std::thread::spawn(move || {
                let state = State::open(&dir).unwrap();
                for _ in 0..2 {
                    state.wait_turn("h", interval, &SystemClock).unwrap();
                    times.lock().unwrap().push(Instant::now());
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let mut times = times.lock().unwrap().clone();
    times.sort();
    // Each request is sent at or after its own slot, and the slots are one interval apart whoever takes them: the
    // k-th time is at least k intervals after the start. (A gap between two recorded times can be shorter: a thread
    // records its time after it wakes, and a busy machine wakes one late.)
    for (k, time) in times.iter().enumerate() {
        let earliest = (interval * k as u32).saturating_sub(Duration::from_millis(15));
        assert!(*time - start >= earliest, "request {k} at {:?}", *time - start);
    }
}
