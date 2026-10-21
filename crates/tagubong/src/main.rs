use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use tagubong::caller::{Caller, DEFAULT_USER_AGENT};
use tagubong::clock::SystemClock;
use tagubong::fred;
use tagubong::http::UreqTransport;
use tagubong::keyed;
use tagubong::kis::{self, Kis, Request};
use tagubong::state::State;
use tagubong::toss::{self, Toss};

/// Exit codes: 0 done, 2 bad use or setup (a missing key too), 3 output closed, 4 stopped early (key rejected,
/// rate limit, no answer, cool-down, unusable state, time).
const EXIT_USAGE: u8 = 2;
const EXIT_OUTPUT: u8 = 3;
const EXIT_STOPPED: u8 = 4;

#[derive(Parser)]
#[command(version, about = "Official APIs with your own key: one JSON line per answer; keys from the environment only")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Common {
    /// Shared state (each API's pace and cool-downs): an absolute path, the same for every caller on the machine
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long)]
    user_agent: Option<String>,
    /// Seconds per request
    #[arg(long, default_value_t = 30)]
    timeout: u64,
    /// Stop starting new requests after this many seconds
    #[arg(long)]
    max_seconds: Option<u64>,
}

#[derive(Subcommand)]
enum Command {
    /// List the APIs: their commands and the environment variable that holds the key
    Apis,
    /// FRED (St. Louis Fed): key in FRED_API_KEY
    Fred {
        #[command(subcommand)]
        command: Fred,
    },
    /// GET requests to a keyed API (see `apis`) from stdin, one JSON line each ({"id", "path", "params"}), one back each
    Get {
        /// ecos, datagokr, assembly, law or krx
        #[arg(long)]
        api: String,
        #[command(flatten)]
        common: Common,
        /// Another base URL (tests): https, or http on the loopback only
        #[arg(long, hide = true)]
        endpoint: Option<String>,
    },
    /// 토스증권 Open API: credentials in TOSS_CLIENT_ID and TOSS_CLIENT_SECRET
    Toss {
        #[command(subcommand)]
        command: TossCommand,
    },
    /// 한국투자증권 Open API: keys in KIS_APP_KEY and KIS_APP_SECRET
    Kis {
        #[command(subcommand)]
        command: KisCommand,
    },
}

#[derive(Subcommand)]
enum TossCommand {
    /// GET requests from stdin, one JSON line each ({"id", "path", "params", "group"}), one line back each
    Get {
        #[command(flatten)]
        common: Common,
        /// Another endpoint (tests): https, or http on the loopback only
        #[arg(long, hide = true, default_value = toss::ENDPOINT)]
        endpoint: String,
    },
}

#[derive(Subcommand)]
enum KisCommand {
    /// GET requests from stdin, one JSON line each ({"id", "path", "tr_id", "params", "tr_cont"}), one line back each
    Get {
        #[command(flatten)]
        common: Common,
        /// Another endpoint (tests): https, or http on the loopback only
        #[arg(long, hide = true, default_value = kis::ENDPOINT)]
        endpoint: String,
    },
}

#[derive(Subcommand)]
enum Fred {
    /// Observations of each --series from --start, one line per series
    Observations {
        #[command(flatten)]
        common: Common,
        /// FRED series id (repeatable), e.g. DGS10
        #[arg(long, required = true)]
        series: Vec<String>,
        /// First day (YYYY-MM-DD)
        #[arg(long)]
        start: NaiveDate,
        /// Another endpoint (tests): https, or http on the loopback only
        #[arg(long, hide = true, default_value = fred::ENDPOINT)]
        endpoint: String,
    },
}

/// A closed output ends the run at once: an answer read and not printed would be asked for again.
fn print(value: &Value) {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{value}").and_then(|_| out.flush()).is_err() {
        std::process::exit(EXIT_OUTPUT as i32);
    }
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("tagubong: {error}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The key from the environment, never from an argument (arguments show in the process list).
fn key(env: &str, valid: fn(&str) -> bool) -> Result<String, String> {
    match std::env::var(env) {
        Ok(key) if valid(key.trim()) => Ok(key.trim().to_string()),
        Ok(_) => Err(format!("{env} does not look like a key")),
        Err(_) => Err(format!("{env} is not set")),
    }
}

fn endpoint_allowed(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:")
}

fn run(command: Command) -> Result<u8, String> {
    match command {
        Command::Apis => {
            print(&json!({"kind": "api", "api": fred::API, "key_env": [fred::KEY_ENV], "commands": ["observations"],
                          "interval_secs": fred::INTERVAL.as_secs_f64()}));
            print(
                &json!({"kind": "api", "api": kis::API, "key_env": [kis::KEY_ENV, kis::SECRET_ENV], "commands": ["get"],
                          "interval_secs": kis::INTERVAL.as_secs_f64()}),
            );
            Ok(0)
        }
        Command::Fred { command: Fred::Observations { common, series, start, endpoint } } => {
            if let Some(bad) = series.iter().find(|s| !fred::valid_series(s)) {
                return Err(format!("not a FRED series id: {bad:?}"));
            }
            if !endpoint_allowed(&endpoint) {
                return Err("--endpoint must be https (or http on the loopback)".into());
            }
            let key = key(fred::KEY_ENV, fred::valid_key)?;
            let (state, user_agent) = open(&common)?;
            let transport = UreqTransport::new(Duration::from_secs(common.timeout));
            let caller = Caller::new(fred::API, &transport, &state, &SystemClock, fred::INTERVAL, user_agent);
            let started = Instant::now();
            let mut unsent = 0;
            let mut out_of_time = false;
            for (i, id) in series.iter().enumerate() {
                if common.max_seconds.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) {
                    (out_of_time, unsent) = (true, series.len() - i);
                    break;
                }
                print(&fred::observations(&caller, &endpoint, &key, id, start, now_iso));
            }
            Ok(finish(&caller, unsent, out_of_time, json!({})))
        }
        Command::Get { api, common, endpoint } => {
            let Some(spec) = keyed::spec(&api) else {
                return Err(format!("no such API: {api:?} (see `tagubong apis`)"));
            };
            let endpoint = endpoint.unwrap_or_else(|| spec.base.to_string());
            if !endpoint_allowed(&endpoint) || !endpoint.ends_with('/') {
                return Err("--endpoint must be https (or http on the loopback) and end with /".into());
            }
            let key = key(spec.key_env, keyed::valid_key)?;
            let (state, user_agent) = open(&common)?;
            let transport = UreqTransport::new(Duration::from_secs(common.timeout));
            let mut caller = Caller::new(spec.api, &transport, &state, &SystemClock, spec.interval, user_agent);
            (caller.retries, caller.refusals_stop) = (1, spec.refusal_stops);
            let started = Instant::now();
            let (mut unsent, mut out_of_time) = (0, false);
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if out_of_time || common.max_seconds.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) {
                    (out_of_time, unsent) = (true, unsent + 1);
                    continue;
                }
                match keyed::Request::parse(&line, spec) {
                    Ok(request) => print(&keyed::get(&caller, spec, &endpoint, &key, &request, now_iso)),
                    Err(error) => {
                        caller.count("bad_request");
                        print(&json!({"kind": "response", "api": spec.api, "status": "bad_request", "error": error}));
                    }
                }
            }
            Ok(finish(&caller, unsent, out_of_time, json!({})))
        }
        Command::Toss { command: TossCommand::Get { common, endpoint } } => {
            if !endpoint_allowed(&endpoint) {
                return Err("--endpoint must be https (or http on the loopback)".into());
            }
            let (id, secret) =
                (key(toss::ID_ENV, toss::valid_credential)?, key(toss::SECRET_ENV, toss::valid_credential)?);
            let (state, user_agent) = open(&common)?;
            let transport = UreqTransport::new(Duration::from_secs(common.timeout));
            let mut client = Toss::new(&transport, &state, &SystemClock, &user_agent, &endpoint, id, secret);
            let started = Instant::now();
            let (mut unsent, mut out_of_time, mut bad) = (0, false, 0);
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if out_of_time || common.max_seconds.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) {
                    (out_of_time, unsent) = (true, unsent + 1);
                    continue;
                }
                match toss::Request::parse(&line) {
                    Ok(request) => print(&client.get(&request, now_iso)),
                    Err(error) => {
                        bad += 1;
                        print(&json!({"kind": "response", "api": toss::API, "status": "bad_request", "error": error}));
                    }
                }
            }
            let stopped = client.stopped().or(out_of_time.then_some("time_limit"));
            let mut statuses = client.statuses();
            if bad > 0 {
                statuses.insert("bad_request".into(), bad);
            }
            print(&json!({"kind": "summary", "api": toss::API, "requests": client.requests(), "statuses": statuses,
                          "stopped": stopped, "unsent": unsent, "token_issued": client.issued, "fetched_at": now_iso()}));
            Ok(if stopped.is_some() { EXIT_STOPPED } else { 0 })
        }
        Command::Kis { command: KisCommand::Get { common, endpoint } } => {
            if !endpoint_allowed(&endpoint) {
                return Err("--endpoint must be https (or http on the loopback)".into());
            }
            let (app_key, secret) =
                (key(kis::KEY_ENV, kis::valid_credential)?, key(kis::SECRET_ENV, kis::valid_credential)?);
            let (state, user_agent) = open(&common)?;
            let transport = UreqTransport::new(Duration::from_secs(common.timeout));
            let mut caller = Caller::new(kis::API, &transport, &state, &SystemClock, kis::INTERVAL, user_agent);
            (caller.retries, caller.answers_5xx) = (1, true);
            let mut client = Kis::new(&caller, &endpoint, app_key, secret);
            let started = Instant::now();
            let (mut unsent, mut out_of_time) = (0, false);
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if out_of_time || common.max_seconds.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) {
                    (out_of_time, unsent) = (true, unsent + 1);
                    continue;
                }
                match Request::parse(&line) {
                    Ok(request) => print(&client.get(&request, now_iso)),
                    Err(error) => {
                        caller.count("bad_request");
                        print(&json!({"kind": "response", "api": kis::API, "status": "bad_request", "error": error}));
                    }
                }
            }
            let issued = client.issued;
            Ok(finish(&caller, unsent, out_of_time, json!({"token_issued": issued})))
        }
    }
}

fn open(common: &Common) -> Result<(State, String), String> {
    if !common.state_dir.is_absolute() {
        return Err(format!("--state-dir must be an absolute path: {}", common.state_dir.display()));
    }
    let user_agent = common.user_agent.clone().unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
    if user_agent.trim().is_empty() {
        return Err("--user-agent must not be empty".into());
    }
    Ok((State::open(&common.state_dir)?, user_agent))
}

fn finish(caller: &Caller, unsent: usize, out_of_time: bool, extra: Value) -> u8 {
    let stopped = caller.stopped().or(out_of_time.then_some("time_limit"));
    let mut summary = json!({"kind": "summary", "api": caller.api, "requests": caller.requests(),
                             "statuses": caller.statuses(), "stopped": stopped, "unsent": unsent, "fetched_at": now_iso()});
    if let (Some(summary), Some(extra)) = (summary.as_object_mut(), extra.as_object()) {
        summary.extend(extra.clone());
    }
    print(&summary);
    if stopped.is_some() { EXIT_STOPPED } else { 0 }
}
