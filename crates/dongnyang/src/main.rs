use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use scraper::Html;
use serde_json::{Value, json};

use dongnyang::clock::SystemClock;
use dongnyang::decode::decode;
use dongnyang::fetch::{Fetcher, Format, Options};
use dongnyang::http::UreqTransport;
use dongnyang::normalize::{self, Input};
use dongnyang::source::Registry;
use dongnyang::state::State;

/// Exit codes: 0 done, 2 bad use or setup, 3 output closed, 4 stopped early (cool-down, unusable state, time).
const EXIT_USAGE: u8 = 2;
const EXIT_OUTPUT: u8 = 3;
const EXIT_STOPPED: u8 = 4;

#[derive(Parser)]
#[command(version, about = "Polite news fetcher: one JSON line per feed item, sitemap address or article page")]
struct Cli {
    /// Directory of extra or replacing source specs (*.toml)
    #[arg(long, global = true)]
    sources: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Net {
    /// Outlet id (see `sources`)
    #[arg(long)]
    source: String,
    /// Shared state (pace, cool-downs, robots.txt cache, feed validators): an absolute path, the same for every caller
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long)]
    user_agent: Option<String>,
    /// Extra request header, "Name: value" (repeatable)
    #[arg(long = "header", value_parser = header)]
    headers: Vec<(String, String)>,
    /// Seconds between requests to one outlet (never below 3 s or the outlet's minimum)
    #[arg(long)]
    interval: Option<f64>,
    /// Seconds per request
    #[arg(long, default_value_t = 20)]
    timeout: u64,
    /// Stop starting new requests after this many seconds
    #[arg(long)]
    max_seconds: Option<u64>,
    /// Give up a request (status busy) instead of waiting longer than this many seconds for the outlet's turn
    #[arg(long)]
    max_wait: Option<f64>,
    #[arg(long, value_enum, default_value_t = Format::Both)]
    format: Format,
}

#[derive(Subcommand)]
enum Command {
    /// List the outlets
    Sources,
    /// Read the outlet's feeds (or the given ones)
    Feed {
        #[command(flatten)]
        net: Net,
        feeds: Vec<String>,
    },
    /// Read the sitemaps of the periods from --from to --to (newest first), or the sitemap at --url
    Sitemap {
        #[command(flatten)]
        net: Net,
        #[arg(long, required_unless_present = "url")]
        from: Option<NaiveDate>,
        #[arg(long)]
        to: Option<NaiveDate>,
        /// One sitemap or sitemap index on the outlet's hosts (e.g. a `sitemap_ref` line's url)
        #[arg(long, conflicts_with_all = ["from", "to"])]
        url: Option<String>,
    },
    /// Read article pages: URLs as arguments, else one per line on stdin
    Article {
        #[command(flatten)]
        net: Net,
        /// At most this many pages
        #[arg(long)]
        limit: Option<usize>,
        urls: Vec<String>,
    },
    /// Article ids of URLs on stdin, the outlet found by host; no network (for ids of rows stored before)
    Ids,
    /// Serve the outlets to an agent over MCP (stdio): list_sources, read_feed, read_article, article_id
    Mcp {
        /// Shared state, as for the other commands: an absolute path, the same for every caller
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        user_agent: Option<String>,
        /// Seconds a call waits at most for the outlet's turn before it answers busy
        #[arg(long, default_value_t = 30.0)]
        max_wait: f64,
        /// Seconds per request
        #[arg(long, default_value_t = 20)]
        timeout: u64,
    },
    /// Parse a saved file without the network
    Parse {
        #[arg(long)]
        source: String,
        #[arg(long, value_parser = ["article", "feed", "sitemap"])]
        kind: String,
        /// The URL the file was read from (for ids and relative links)
        #[arg(long, default_value = "https://example.invalid/")]
        url: String,
        #[arg(long, value_enum, default_value_t = Format::Both)]
        format: Format,
        file: PathBuf,
    },
}

fn header(text: &str) -> Result<(String, String), String> {
    let (name, value) = text.split_once(':').ok_or("expected \"Name: value\"")?;
    let name = name.trim();
    if name.is_empty()
        || name.eq_ignore_ascii_case("host")
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err("bad header name".into());
    }
    Ok((name.to_string(), value.trim().to_string()))
}

/// A closed output ends the run at once: what is read and not printed would be read again later.
struct Out;

impl Out {
    fn print(&self, value: &Value) {
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{value}").and_then(|_| out.flush()).is_err() {
            std::process::exit(EXIT_OUTPUT as i32);
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let registry = match Registry::load(cli.sources.as_deref()) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("dongnyang: {error}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match run(cli.command, &registry) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("dongnyang: {error}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn run(command: Command, registry: &Registry) -> Result<u8, String> {
    let out = Out;
    let plugin_of = |id: &str| registry.get(id).ok_or_else(|| format!("unknown source {id:?}"));
    let net = match command {
        Command::Sources => {
            for id in registry.ids() {
                let spec = registry.get(id).unwrap().spec();
                out.print(&json!({"id": id, "outlet": spec.outlet, "hosts": spec.hosts, "feeds": spec.feeds.len(),
                                  "sitemap": spec.sitemap.is_some(), "min_interval_secs": spec.min_interval_secs}));
            }
            return Ok(0);
        }
        Command::Mcp { state_dir, user_agent, max_wait, timeout } => {
            if !state_dir.is_absolute() {
                return Err(format!("--state-dir must be an absolute path: {}", state_dir.display()));
            }
            let user_agent = user_agent.unwrap_or_else(|| dongnyang::fetch::DEFAULT_USER_AGENT.to_string());
            if dongnyang::fetch::robots_token(&user_agent).is_none() {
                return Err("--user-agent must start with a product token of its own".into());
            }
            let state = State::open(&state_dir)?;
            let transport = UreqTransport::new(Duration::from_secs(timeout));
            let server = dongnyang::mcp::Server {
                registry,
                transport: &transport,
                state: &state,
                clock: &SystemClock,
                user_agent,
                max_wait: Duration::from_secs_f64(max_wait.clamp(0.0, 600.0)),
            };
            server.serve(std::io::stdin().lock(), std::io::stdout().lock()).map_err(|e| e.to_string())?;
            return Ok(0);
        }
        Command::Ids => {
            let mut stdin = std::io::stdin().lock();
            let mut buffer = vec![];
            while stdin.read_until(b'\n', &mut buffer).map_err(|e| e.to_string())? > 0 {
                let url = String::from_utf8_lossy(&buffer).trim().to_string();
                buffer.clear();
                if url.is_empty() {
                    continue;
                }
                let line = match registry.for_url(&url) {
                    Some(plugin) => {
                        let spec = plugin.spec();
                        let input = Input {
                            source: &spec.id,
                            outlet: &spec.outlet,
                            url: &url,
                            page: None,
                            item: None,
                            body: None,
                        };
                        let id = normalize::normalize(&input, plugin.normalize_spec()).id;
                        json!({"kind": "id", "url": url, "source": spec.id, "outlet": spec.outlet, "id": id})
                    }
                    None => json!({"kind": "id", "url": url, "source": null, "id": null}),
                };
                out.print(&line);
            }
            return Ok(0);
        }
        Command::Parse { source, kind, url, format, file } => {
            let plugin = plugin_of(&source)?;
            let raw = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let text = decode(&raw, None).text;
            let spec = plugin.spec();
            let shape = |normalized: Value, mut raw: Value| -> Value {
                normalize::scrub_value(&mut raw);
                match format {
                    Format::Normalized => normalized,
                    Format::Raw => json!({ "raw": raw }),
                    Format::Both => {
                        let mut both = normalized;
                        both["raw"] = raw;
                        both
                    }
                }
            };
            match kind.as_str() {
                "article" => {
                    let document = Html::parse_document(&text);
                    let page = plugin.page(&document);
                    let body = plugin.body(&document).map(|b| normalize::scrub_body(&b));
                    let input = Input {
                        source: &spec.id,
                        outlet: &spec.outlet,
                        url: &url,
                        page: Some(&page),
                        item: None,
                        body: body.as_deref(),
                    };
                    let mut normalized = json!(normalize::normalize(&input, plugin.normalize_spec()));
                    normalized["body"] = json!(body);
                    out.print(&shape(normalized, json!({ "page": page })));
                }
                "feed" => {
                    for item in plugin.feed_items(&text)? {
                        let link = item.get("link").and_then(|v| v[0].as_str()).unwrap_or(&url).to_string();
                        let input = Input {
                            source: &spec.id,
                            outlet: &spec.outlet,
                            url: &link,
                            page: None,
                            item: Some(&item),
                            body: None,
                        };
                        let mut normalized = json!(normalize::normalize(&input, plugin.normalize_spec()));
                        normalized["url"] = json!(link);
                        out.print(&shape(normalized, json!({ "rss": item })));
                    }
                }
                _ => {
                    let map = plugin.sitemap(&text)?;
                    out.print(&json!({"index": map.index, "entries": map.entries}));
                }
            }
            return Ok(0);
        }
        Command::Feed { ref net, .. } | Command::Sitemap { ref net, .. } | Command::Article { ref net, .. } => net,
    };
    let plugin = plugin_of(&net.source)?;
    if !net.state_dir.is_absolute() {
        return Err(format!("--state-dir must be an absolute path: {}", net.state_dir.display()));
    }
    let state = State::open(&net.state_dir)?;
    let transport = UreqTransport::new(Duration::from_secs(net.timeout));
    let mut options = Options {
        headers: net.headers.clone(),
        interval: net.interval,
        max_wait: net.max_wait.filter(|w| w.is_finite() && *w >= 0.0).map(Duration::from_secs_f64),
        format: net.format,
        ..Options::default()
    };
    if let Some(agent) = &net.user_agent {
        // robots.txt is read for the agent's own product token, so it must name one (and not pose as a browser)
        if dongnyang::fetch::robots_token(agent).is_none() {
            return Err(
                "--user-agent must start with a product token of its own, e.g. \"myresearch/1.0 (+contact)\"".into()
            );
        }
        options.user_agent = agent.clone();
    }
    let fetcher = Fetcher::new(plugin, &transport, &state, &SystemClock, options);
    let started = Instant::now();
    let deadline = net.max_seconds.map(Duration::from_secs);
    let out_of_time = || deadline.is_some_and(|d| started.elapsed() >= d);
    let mut stopped_for_time = false;
    let mut unsent = 0usize;
    match &command {
        Command::Feed { feeds, .. } => {
            let feeds = if feeds.is_empty() { plugin.spec().feeds.clone() } else { feeds.clone() };
            for (i, feed) in feeds.iter().enumerate() {
                if fetcher.stopped().is_some() || out_of_time() {
                    stopped_for_time = fetcher.stopped().is_none();
                    unsent = feeds.len() - i;
                    break;
                }
                fetcher.feed(feed, &mut |v| out.print(&v));
            }
        }
        Command::Sitemap { from, to, url, .. } => {
            if let Some(url) = url {
                fetcher.sitemap_at(url, None, &mut |v| out.print(&v));
            } else {
                let period = plugin.period().ok_or("this source has no sitemap")?;
                let from = from.ok_or("--from is required")?;
                let to = to.unwrap_or(from);
                let (newest, oldest) = if from >= to { (from, to) } else { (to, from) };
                let mut start = period.start(newest);
                while start >= period.start(oldest) {
                    if fetcher.stopped().is_some() || out_of_time() {
                        stopped_for_time = fetcher.stopped().is_none();
                        unsent += 1;
                    } else {
                        fetcher.sitemap(start, &mut |v| out.print(&v));
                    }
                    match period.previous(start) {
                        Some(previous) => start = previous,
                        None => break,
                    }
                }
            }
        }
        Command::Article { limit, urls, .. } => {
            let urls: Vec<String> = if urls.is_empty() {
                let mut lines = vec![];
                let mut stdin = std::io::stdin().lock();
                let mut buffer = vec![];
                while stdin.read_until(b'\n', &mut buffer).map_err(|e| e.to_string())? > 0 {
                    let line = String::from_utf8_lossy(&buffer).trim().to_string();
                    if !line.is_empty() {
                        lines.push(line);
                    }
                    buffer.clear();
                }
                lines
            } else {
                urls.clone()
            };
            // each address once per run, in the order given
            let mut seen = std::collections::HashSet::new();
            let urls: Vec<String> =
                urls.into_iter().filter(|u| seen.insert(u.clone())).take(limit.unwrap_or(usize::MAX)).collect();
            for (i, url) in urls.iter().enumerate() {
                if fetcher.stopped().is_some() || out_of_time() {
                    stopped_for_time = fetcher.stopped().is_none();
                    unsent = urls.len() - i;
                    break;
                }
                out.print(&fetcher.article(url));
            }
        }
        _ => unreachable!(),
    }
    let mut summary = fetcher.summary();
    summary["unsent"] = json!(unsent);
    if stopped_for_time {
        summary["stopped"] = json!("time_limit");
    }
    out.print(&summary);
    Ok(if fetcher.stopped().is_some() || stopped_for_time { EXIT_STOPPED } else { 0 })
}
