//! The polite part. Every request goes through `Fetcher::send`: only the outlet's hosts; robots.txt (cached a
//! day, failures an hour, shared); one shared pace per pace key, the largest of the floor, the spec, Crawl-delay
//! and the command line; the cool-down checked before and again after waiting; every redirect hop checked.
//! Block signals start a shared cool-down: a Cloudflare challenge (24 h), 429 (Retry-After, else 1 h),
//! BLOCK_STRIKES plain 403s in a row (24 h), MAX_FAILURES answers-less or 5xx in a row (1 h). State that cannot
//! be read or written stops the run before any request (fail closed). Commands turn feeds, sitemaps and pages
//! into JSON lines.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::Duration;

use chrono::NaiveDate;
use scraper::Html;
use serde_json::{Map, Value, json};

use crate::clock::Clock;
use crate::decode::decode;
use crate::extract::RawItem;
use crate::http::{Response, Transport};
use crate::normalize::{self, Input, Normalized};
use crate::parse::{self, PARSER_VERSION};
use crate::robots::Robots;
use crate::source::{MIN_INTERVAL_FLOOR_SECS, Plugin};
use crate::state::{CachedRobots, State, Validators};

/// Says who reads and where to learn more; a caller running it for itself should give its own (`--user-agent`).
pub const DEFAULT_USER_AGENT: &str =
    concat!("dongnyang/", env!("CARGO_PKG_VERSION"), " (+https://github.com/webispy/gaibang)");
/// The robots.txt token when a User-Agent gives none of its own (tests build Options by hand).
pub const DEFAULT_ROBOTS_TOKEN: &str = "dongnyang";
pub const BLOCK_COOLDOWN_SECS: i64 = 24 * 3600;
pub const RATE_COOLDOWN_SECS: i64 = 3600;
pub const FAILURE_COOLDOWN_SECS: i64 = 3600;
pub const BLOCK_STRIKES: u32 = 5;
pub const MAX_FAILURES: u32 = 5;
pub const MAX_REDIRECTS: usize = 5;
pub const MAX_FEED_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_SITEMAP_BYTES: usize = 32 * 1024 * 1024; // 파이낸셜뉴스: 4.7 MB a part (2025-01)
pub const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_ROBOTS_BYTES: usize = 512 * 1024;
pub const MAX_URL_BYTES: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Format {
    /// Common fields only
    Normalized,
    /// The outlet's own fields only
    Raw,
    /// Common fields and `raw`
    Both,
}

pub struct Options {
    pub user_agent: String,
    pub headers: Vec<(String, String)>,
    /// Seconds between requests to one pace key; never below the floor or the outlet's own minimum.
    pub interval: Option<f64>,
    /// Give up a request (status `busy`) rather than wait longer than this for its turn.
    pub max_wait: Option<Duration>,
    pub format: Format,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            user_agent: DEFAULT_USER_AGENT.to_string(),
            headers: vec![],
            interval: None,
            max_wait: None,
            format: Format::Both,
        }
    }
}

pub struct Fetcher<'a> {
    pub plugin: &'a dyn Plugin,
    pub transport: &'a dyn Transport,
    pub state: &'a State,
    pub clock: &'a dyn Clock,
    pub options: Options,
    requests: Cell<u32>,
    stopped: RefCell<Option<String>>,
    statuses: RefCell<BTreeMap<String, u32>>,
    /// Parsed robots.txt per host for this run (the shared cache holds the text).
    parsed_robots: RefCell<BTreeMap<String, Robots>>,
}

/// What a guarded GET came to.
pub enum Outcome {
    Got { response: Response, final_url: String },
    Refused(&'static str),
}

/// Refusals after which nothing more should be sent this run.
const STOPPING: &[&str] = &["cooldown", "state_unavailable", "robots_unavailable"];

/// The robots.txt product token of a User-Agent (RFC 9309 §2.2.1): its first product's name, lowercased. None
/// when it has none or names a browser: a polite reader says who it is, and robots.txt is read for that name.
pub fn robots_token(user_agent: &str) -> Option<String> {
    let token = user_agent.split_whitespace().next()?.split('/').next()?;
    let named = !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    (named && !token.eq_ignore_ascii_case("mozilla")).then(|| token.to_ascii_lowercase())
}

fn now_iso(clock: &dyn Clock) -> String {
    chrono::DateTime::from_timestamp_millis(clock.now_millis())
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn resolve(base: &str, location: &str) -> Option<String> {
    if location.starts_with("https://") || location.starts_with("http://") {
        return Some(location.to_string());
    }
    let scheme_end = base.find("://")? + 3;
    let host_end = base[scheme_end..].find('/').map(|i| scheme_end + i).unwrap_or(base.len());
    if location.starts_with("//") {
        return Some(format!("{}{}", &base[..scheme_end - 2], location));
    }
    if location.starts_with('/') {
        return Some(format!("{}{location}", &base[..host_end]));
    }
    None // a relative path without a slash: not used by news sites, refused
}

impl<'a> Fetcher<'a> {
    pub fn new(
        plugin: &'a dyn Plugin,
        transport: &'a dyn Transport,
        state: &'a State,
        clock: &'a dyn Clock,
        options: Options,
    ) -> Fetcher<'a> {
        Fetcher {
            plugin,
            transport,
            state,
            clock,
            options,
            requests: Cell::new(0),
            stopped: RefCell::new(None),
            statuses: RefCell::new(BTreeMap::new()),
            parsed_robots: RefCell::new(BTreeMap::new()),
        }
    }

    /// Why the run should send nothing more (a cool-down, unusable state), if so.
    pub fn stopped(&self) -> Option<String> {
        self.stopped.borrow().clone()
    }

    pub fn summary(&self) -> Value {
        let mut record = self.base("summary");
        record.insert("requests".into(), json!(self.requests.get()));
        record.insert("statuses".into(), json!(*self.statuses.borrow()));
        record.insert("stopped".into(), json!(self.stopped()));
        Value::Object(record)
    }

    fn stop(&self, reason: String) {
        self.stopped.borrow_mut().get_or_insert(reason);
    }

    fn count(&self, status: &str) {
        *self.statuses.borrow_mut().entry(status.to_string()).or_default() += 1;
    }

    fn headers(&self, accept: &str, extra: &[(String, String)]) -> Vec<(String, String)> {
        let mut headers = vec![
            ("User-Agent".to_string(), self.options.user_agent.clone()),
            ("Accept".to_string(), accept.to_string()),
        ];
        headers.extend(self.options.headers.iter().cloned());
        headers.extend(extra.iter().cloned());
        headers
    }

    fn interval(&self, robots: &Robots) -> Duration {
        let secs = [
            MIN_INTERVAL_FLOOR_SECS,
            self.plugin.spec().min_interval_secs,
            robots.crawl_delay.unwrap_or(0.0),
            self.options.interval.filter(|i| i.is_finite()).unwrap_or(0.0),
        ]
        .into_iter()
        .fold(0.0, f64::max)
        .min(crate::state::MAX_WAIT_SECS as f64);
        Duration::from_millis((secs * 1000.0) as u64)
    }

    fn own_host(&self, host: &str) -> bool {
        self.plugin.spec().hosts.iter().any(|h| h == host)
    }

    /// Wait for the pace key's turn, then check the cool-down again (another process may have been refused
    /// while this one waited). Err: send nothing.
    fn turn(&self, interval: Duration) -> Result<(), &'static str> {
        let key = self.plugin.pace_key();
        match self.state.cooldown(key, self.clock) {
            Ok(None) => {}
            Ok(Some(_)) => return Err("cooldown"),
            Err(_) => return Err("state_unavailable"),
        }
        match self.state.wait_turn_within(key, interval, self.options.max_wait, self.clock) {
            Ok(true) => {}
            Ok(false) => return Err("busy"), // the queue is longer than the reader will wait: nothing sent
            Err(_) => return Err("state_unavailable"),
        }
        match self.state.cooldown(key, self.clock) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err("cooldown"),
            Err(_) => Err("state_unavailable"),
        }
    }

    /// Record what an answer says about being welcome. Err: the key is now cooling down (or the state could
    /// not record it), send nothing more.
    fn note(&self, response: &Response) -> Result<(), &'static str> {
        let (key, state, clock) = (self.plugin.pace_key(), self.state, self.clock);
        let cool = |secs: i64, reason: &str| -> Result<(), &'static str> {
            state.set_cooldown(key, secs, reason, clock).map_err(|_| "state_unavailable")?;
            Err("cooldown")
        };
        if response.header("cf-mitigated").is_some_and(|v| v.eq_ignore_ascii_case("challenge")) {
            return cool(BLOCK_COOLDOWN_SECS, "cloudflare challenge");
        }
        match response.status {
            429 => {
                let secs = response
                    .header("retry-after")
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .unwrap_or(RATE_COOLDOWN_SECS);
                cool(secs.clamp(60, BLOCK_COOLDOWN_SECS), "429")
            }
            403 => match state.strike(key) {
                Ok(n) if n >= BLOCK_STRIKES => cool(BLOCK_COOLDOWN_SECS, "403 in a row"),
                Ok(_) => Ok(()),
                Err(_) => Err("state_unavailable"),
            },
            _ if response.incomplete => match state.failure(key) {
                Ok(n) if n >= MAX_FAILURES => cool(FAILURE_COOLDOWN_SECS, "no answer or 5xx in a row"),
                Ok(_) => Ok(()),
                Err(_) => Err("state_unavailable"),
            },
            0 | 500..=599 => match state.failure(key) {
                Ok(n) if n >= MAX_FAILURES => cool(FAILURE_COOLDOWN_SECS, "no answer or 5xx in a row"),
                Ok(_) => Ok(()),
                Err(_) => Err("state_unavailable"),
            },
            _ => state.answered(key).map_err(|_| "state_unavailable"),
        }
    }

    /// One request, in turn and noted. Refusals that stop the run are remembered.
    fn send(
        &self,
        url: &str,
        accept: &str,
        extra: &[(String, String)],
        max: usize,
        interval: Duration,
    ) -> Result<Response, &'static str> {
        let result = (|| {
            self.turn(interval)?;
            self.requests.set(self.requests.get() + 1);
            let response = self.transport.get(url, &self.headers(accept, extra), max);
            self.note(&response)?;
            Ok(response)
        })();
        if let Err(reason) = result
            && STOPPING.contains(&reason)
        {
            self.stop(reason.to_string());
        }
        result
    }

    fn robots(&self, scheme: &str, host: &str) -> Result<Robots, &'static str> {
        if let Some(robots) = self.parsed_robots.borrow().get(host) {
            return Ok(robots.clone());
        }
        let robots = self.robots_uncached(scheme, host)?;
        self.parsed_robots.borrow_mut().insert(host.to_string(), robots.clone());
        Ok(robots)
    }

    fn robots_agent(&self) -> String {
        robots_token(&self.options.user_agent).unwrap_or_else(|| DEFAULT_ROBOTS_TOKEN.to_string())
    }

    fn robots_uncached(&self, scheme: &str, host: &str) -> Result<Robots, &'static str> {
        match self.state.robots(host, self.clock) {
            Ok(Some(CachedRobots::Text(text))) => return Ok(Robots::parse(&text, &self.robots_agent())),
            Ok(Some(CachedRobots::Missing)) => return Ok(Robots::allow_all()),
            Ok(Some(CachedRobots::Unavailable)) => {
                self.stop("robots_unavailable".into());
                return Err("robots_unavailable");
            }
            Ok(None) => {}
            Err(_) => {
                self.stop("state_unavailable".into());
                return Err("state_unavailable");
            }
        }
        let interval = self.interval(&Robots::allow_all());
        let mut url = format!("{scheme}://{host}/robots.txt");
        let mut found = CachedRobots::Unavailable;
        for _ in 0..=MAX_REDIRECTS {
            let response = self.send(&url, "text/plain, */*", &[], MAX_ROBOTS_BYTES, interval)?;
            found = match response.status {
                200 if !response.incomplete => {
                    CachedRobots::Text(decode(&response.body, response.header("content-type")).text)
                }
                300..=399 => match response.header("location").and_then(|l| resolve(&url, l)) {
                    // RFC 9309 §2.3.1.2: follow redirects, here only on the same host
                    Some(next) if parse::host_and_path(&next).is_some_and(|(h, _)| h == host) => {
                        url = next;
                        continue;
                    }
                    _ => CachedRobots::Unavailable,
                },
                400..=499 if !matches!(response.status, 401 | 403 | 429) => CachedRobots::Missing,
                _ => CachedRobots::Unavailable,
            };
            break;
        }
        if self.state.save_robots(host, found.clone(), self.clock).is_err() {
            self.stop("state_unavailable".into());
            return Err("state_unavailable");
        }
        match found {
            CachedRobots::Text(text) => Ok(Robots::parse(&text, &self.robots_agent())),
            CachedRobots::Missing => Ok(Robots::allow_all()),
            CachedRobots::Unavailable => {
                self.stop("robots_unavailable".into());
                Err("robots_unavailable")
            }
        }
    }

    /// GET `url` within the rules; redirects are followed only to the outlet's hosts and allowed paths.
    pub fn get(&self, url: &str, accept: &str, extra: &[(String, String)], max: usize) -> Outcome {
        if let Some(reason) = self.stopped.borrow().as_deref().and_then(|r| STOPPING.iter().find(|s| **s == r)) {
            return Outcome::Refused(reason);
        }
        if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Outcome::Refused("not_allowed"); // a malformed address must not count as a failed answer
        }
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let first = current == url;
            let Some((host, path)) = parse::host_and_path(&current) else { return Outcome::Refused("not_allowed") };
            if !self.own_host(&host) {
                return Outcome::Refused(if first { "not_allowed" } else { "redirect_blocked" });
            }
            let scheme = if current.starts_with("http://") { "http" } else { "https" };
            let robots = match self.robots(scheme, &host) {
                Ok(robots) => robots,
                Err(reason) => return Outcome::Refused(reason),
            };
            if !robots.allows(&path) {
                return Outcome::Refused(if first { "robots" } else { "redirect_blocked" });
            }
            let response = match self.send(&current, accept, extra, max, self.interval(&robots)) {
                Ok(response) => response,
                Err(reason) => return Outcome::Refused(reason),
            };
            if (300..400).contains(&response.status) && response.status != 304 {
                match response.header("location").and_then(|l| resolve(&current, l)) {
                    // never from https down to http
                    Some(next) if !(current.starts_with("https://") && next.starts_with("http://")) => {
                        current = next;
                        continue;
                    }
                    _ => return Outcome::Refused("redirect_blocked"),
                }
            }
            return Outcome::Got { response, final_url: current };
        }
        Outcome::Refused("redirect_blocked")
    }

    fn base(&self, kind: &str) -> Map<String, Value> {
        let mut record = Map::new();
        record.insert("kind".into(), json!(kind));
        record.insert("source".into(), json!(self.plugin.spec().id));
        record.insert("outlet".into(), json!(self.plugin.spec().outlet));
        record.insert("fetched_at".into(), json!(now_iso(self.clock)));
        record
    }

    fn finish(&self, mut record: Map<String, Value>, status: &str) -> Value {
        self.count(status);
        record.insert("status".into(), json!(status));
        Value::Object(record)
    }

    /// Add the common fields and/or the raw record, as the format asks; e-mail addresses are removed from both.
    fn shape(&self, record: &mut Map<String, Value>, normalized: Normalized, mut raw: Value) {
        if self.options.format != Format::Raw
            && let Value::Object(fields) = json!(normalized)
        {
            record.extend(fields);
        }
        if self.options.format != Format::Normalized {
            normalize::scrub_value(&mut raw);
            record.insert("raw".into(), raw);
        }
    }

    /// One feed: a `feed_item` line per item, then a `feed` line. Conditional GET: an unchanged feed costs no parse.
    pub fn feed(&self, url: &str, emit: &mut dyn FnMut(Value)) {
        let validators = self.state.validators(url);
        let mut extra = vec![];
        if let Some(etag) = &validators.etag {
            extra.push(("If-None-Match".to_string(), etag.clone()));
        }
        if let Some(modified) = &validators.last_modified {
            extra.push(("If-Modified-Since".to_string(), modified.clone()));
        }
        let mut record = self.base("feed");
        record.insert("url".into(), json!(url));
        let (response, _) = match self.get(url, "application/rss+xml, application/xml, */*", &extra, MAX_FEED_BYTES) {
            Outcome::Refused(reason) => return emit(self.finish(record, reason)),
            Outcome::Got { response, final_url } => (response, final_url),
        };
        record.insert("http_status".into(), json!(response.status));
        let status = match response.status {
            304 => "not_modified",
            200 if response.truncated => "too_large",
            200 if response.incomplete => "failed",
            200 => {
                let decoded = decode(&response.body, response.header("content-type"));
                record.insert("encoding".into(), json!(decoded.encoding));
                match self.plugin.feed_items(&decoded.text) {
                    Ok(items) => {
                        let (mut emitted, mut dropped) = (0, 0);
                        for item in items {
                            match self.feed_item(url, &item) {
                                Some(line) => {
                                    emitted += 1;
                                    emit(line);
                                }
                                None => dropped += 1,
                            }
                        }
                        record.insert("items".into(), json!(emitted));
                        record.insert("dropped".into(), json!(dropped));
                        let fresh = Validators {
                            url: url.to_string(),
                            etag: response.header("etag").map(str::to_string),
                            last_modified: response.header("last-modified").map(str::to_string),
                        };
                        if self.state.save_validators(&fresh).is_err() {
                            record.insert("warning".into(), json!("validators not saved"));
                        }
                        "ok"
                    }
                    Err(error) => {
                        record.insert("error".into(), json!(error));
                        "unparsable"
                    }
                }
            }
            _ => "failed",
        };
        emit(self.finish(record, status));
    }

    /// A feed item as a line, or None without a usable link or title.
    fn feed_item(&self, feed: &str, item: &RawItem) -> Option<Value> {
        let spec = self.plugin.spec();
        let text = |name: &str| -> Option<String> {
            match item.get(name)?.as_array()?.first()? {
                Value::String(s) => Some(s.trim().to_string()),
                Value::Object(o) => o.get("#text").and_then(Value::as_str).map(|s| s.trim().to_string()),
                _ => None,
            }
        };
        let link = text("link").filter(|l| {
            (l.starts_with("https://") || l.starts_with("http://")) && l.len() <= MAX_URL_BYTES // the caller's key: never cut
        })?;
        let input =
            Input { source: &spec.id, outlet: &spec.outlet, url: &link, page: None, item: Some(item), body: None };
        let normalized = normalize::normalize(&input, self.plugin.normalize_spec());
        normalized.title.as_ref()?;
        let mut line = self.base("feed_item");
        line.insert("feed".into(), json!(feed));
        line.insert("url".into(), json!(link));
        self.shape(&mut line, normalized, Value::Object(json!({"rss": item}).as_object().cloned().unwrap_or_default()));
        Some(Value::Object(line))
    }

    /// The sitemap of the period holding `day` (see `sitemap_at`).
    pub fn sitemap(&self, day: NaiveDate, emit: &mut dyn FnMut(Value)) {
        let Some(url) = self.plugin.sitemap_url(day) else {
            let mut record = self.base("sitemap");
            record.insert("day".into(), json!(day.to_string()));
            return emit(self.finish(record, "no_sitemap"));
        };
        self.sitemap_at(&url, Some(day), emit);
    }

    /// One sitemap: a `sitemap_url` line per article (with its lastmod; articles older than the outlet serves
    /// are counted, not listed), or for an index a `sitemap_ref` line per sitemap; then a `sitemap` line.
    pub fn sitemap_at(&self, url: &str, day: Option<NaiveDate>, emit: &mut dyn FnMut(Value)) {
        let mut record = self.base("sitemap");
        if let Some(day) = day {
            record.insert("day".into(), json!(day.to_string()));
        }
        record.insert("url".into(), json!(url));
        let response = match self.get(url, "application/xml, */*", &[], MAX_SITEMAP_BYTES) {
            Outcome::Refused(reason) => return emit(self.finish(record, reason)),
            Outcome::Got { response, .. } => response,
        };
        record.insert("http_status".into(), json!(response.status));
        if response.status != 200 || response.truncated || response.incomplete {
            let status = if response.truncated { "too_large" } else { "failed" };
            return emit(self.finish(record, status));
        }
        let decoded = decode(&response.body, response.header("content-type"));
        let status = match self.plugin.sitemap(&decoded.text) {
            Ok(map) => {
                let (old, recent): (Vec<_>, Vec<_>) =
                    map.entries.into_iter().partition(|e| !map.index && self.plugin.too_old(&e.url));
                record.insert("index".into(), json!(map.index));
                record.insert("urls".into(), json!(recent.len()));
                record.insert("old".into(), json!(old.len()));
                let spec = self.plugin.spec();
                for entry in recent {
                    let mut line = self.base(if map.index { "sitemap_ref" } else { "sitemap_url" });
                    line.insert("sitemap".into(), json!(url));
                    if !map.index {
                        let input = Input {
                            source: &spec.id,
                            outlet: &spec.outlet,
                            url: &entry.url,
                            page: None,
                            item: None,
                            body: None,
                        };
                        line.insert("id".into(), json!(normalize::normalize(&input, self.plugin.normalize_spec()).id));
                    }
                    line.insert("url".into(), json!(entry.url));
                    line.insert("lastmod".into(), json!(entry.lastmod));
                    emit(Value::Object(line));
                }
                "ok"
            }
            Err(error) => {
                record.insert("error".into(), json!(error));
                "unparsable"
            }
        };
        emit(self.finish(record, status));
    }

    /// One article page, everything it gives in one read: the common fields, the whole body, the raw record.
    pub fn article(&self, url: &str) -> Value {
        let mut record = self.base("article");
        record.insert("url".into(), json!(url));
        record.insert("parser_version".into(), json!(PARSER_VERSION));
        if self.plugin.too_old(url) {
            return self.finish(record, "too_old");
        }
        if !self.plugin.is_article(url) {
            return self.finish(record, "not_article"); // another host, a skipped section, not an article address
        }
        let (response, final_url) = match self.get(url, "text/html, */*", &[], MAX_PAGE_BYTES) {
            Outcome::Refused(reason) => return self.finish(record, reason),
            Outcome::Got { response, final_url } => (response, final_url),
        };
        record.insert("http_status".into(), json!(response.status));
        if final_url != url {
            record.insert("final_url".into(), json!(final_url));
        }
        let status = match response.status {
            200 if response.truncated => "too_large",
            200 if response.incomplete => "failed",
            200 => {
                let decoded = decode(&response.body, response.header("content-type"));
                record.insert("encoding".into(), json!(decoded.encoding));
                let document = Html::parse_document(&decoded.text);
                let page = self.plugin.page(&document);
                let spec = self.plugin.spec();
                let body = self.plugin.body(&document).map(|b| normalize::scrub_body(&b));
                // The requested address gives the id, as on the feed or sitemap line that listed it.
                let input = Input {
                    source: &spec.id,
                    outlet: &spec.outlet,
                    url,
                    page: Some(&page),
                    item: None,
                    body: body.as_deref(),
                };
                let normalized = normalize::normalize(&input, self.plugin.normalize_spec());
                let has_body = body.is_some();
                self.shape(&mut record, normalized, json!({"page": page}));
                if self.options.format != Format::Raw {
                    record.insert("body".into(), json!(body));
                }
                if decoded.is_bad() {
                    "bad_encoding"
                } else if has_body {
                    "ok"
                } else {
                    "no_body" // the body selectors missed: read again once the plug-in is fixed
                }
            }
            403 => "refused",
            0 | 429 | 500..=599 => "failed",
            404 | 410 => "not_found",
            _ => "no_text",
        };
        self.finish(record, status)
    }
}

#[cfg(test)]
mod tests {
    /// A fresh state directory per call: parallel tests sharing one would reset each other's counts.
    fn temp_state_dir() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("dongnyang-fetch-{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    use super::*;
    use crate::clock::FakeClock;
    use crate::source::Registry;
    use std::collections::HashMap;

    type Call = (String, Vec<(String, String)>);

    #[derive(Default)]
    struct Mock {
        answers: RefCell<HashMap<String, Vec<Response>>>,
        calls: RefCell<Vec<Call>>,
    }

    impl Mock {
        fn on(&self, url: &str, status: u16, body: &str, headers: &[(&str, &str)]) {
            let response = Response {
                status,
                headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                body: body.as_bytes().to_vec(),
                ..Default::default()
            };
            self.answers.borrow_mut().entry(url.into()).or_default().push(response);
        }

        fn urls(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|(u, _)| u.clone()).collect()
        }
    }

    impl Transport for Mock {
        fn get(&self, url: &str, headers: &[(String, String)], _max: usize) -> Response {
            self.calls.borrow_mut().push((url.into(), headers.to_vec()));
            let mut answers = self.answers.borrow_mut();
            match answers.get_mut(url) {
                Some(queue) if queue.len() > 1 => queue.remove(0),
                Some(queue) => queue[0].clone(),
                None => Response { status: 404, ..Default::default() },
            }
        }
    }

    struct Setup {
        registry: Registry,
        mock: Mock,
        dir: std::path::PathBuf,
        state: State,
        clock: FakeClock,
    }

    const ROBOTS: &str = "https://www.hankyung.com/robots.txt";

    fn setup() -> Setup {
        let dir = temp_state_dir();
        let mock = Mock::default();
        mock.on(ROBOTS, 200, "User-agent: *\nDisallow: /print/\n", &[]);
        let state = State::open(&dir).unwrap();
        Setup { registry: Registry::load(None).unwrap(), mock, dir, state, clock: FakeClock::new(1_760_000_000_000) }
    }

    fn fetcher(s: &Setup) -> Fetcher<'_> {
        Fetcher::new(s.registry.get("hankyung").unwrap(), &s.mock, &s.state, &s.clock, Options::default())
    }

    const PAGE: &str = r#"<html><head><meta property="og:title" content="제목 &amp; 부제 | 한국경제">
        <meta property="article:published_time" content="2025-04-15T23:36:40+09:00">
        <meta property="og:description" content="설명 kim@example.com"></head><body><div class="article-body" itemprop="articleBody">
        첫 문단입니다.<br><br>둘째 문단<script>x()</script><figure><figcaption>사진</figcaption></figure>
        <div class="ad">광고</div><p>셋째 문단 kim@example.com</p></div></body></html>"#;

    #[test]
    fn an_article_is_read_once_with_everything_it_gives() {
        let s = setup();
        let url = "https://www.hankyung.com/article/2025041512345";
        s.mock.on(url, 200, PAGE, &[("Content-Type", "text/html; charset=UTF-8")]);
        let record = fetcher(&s).article(url);
        assert_eq!(record["status"], "ok");
        assert_eq!(record["id"], "hankyung:2025041512345");
        assert_eq!(record["title"], "제목 & 부제");
        assert_eq!(record["published_at"], "2025-04-15T14:36:40+00:00");
        assert_eq!(record["provenance"]["published_at"], "meta:article:published_time");
        assert_eq!(record["description"], "설명");
        assert_eq!(record["body"], "첫 문단입니다.\n\n둘째 문단\n\n셋째 문단");
        assert!(!record["raw"].to_string().contains("example.com")); // e-mail addresses removed from raw too
        assert_eq!(s.mock.urls(), vec![ROBOTS, url]);
    }

    #[test]
    fn requests_are_paced_and_robots_is_read_once() {
        let s = setup();
        let f = fetcher(&s);
        for i in 0..3 {
            f.article(&format!("https://www.hankyung.com/article/202504150000{i}"));
        }
        assert_eq!(s.mock.urls().iter().filter(|u| u.ends_with("robots.txt")).count(), 1);
        assert_eq!(s.clock.slept(), vec![Duration::from_secs(3); 3]);
    }

    #[test]
    fn the_robots_token_is_the_user_agents_first_product_name() {
        assert_eq!(robots_token("MyResearch/1.0 (+https://example.org)").as_deref(), Some("myresearch"));
        assert_eq!(robots_token(DEFAULT_USER_AGENT).as_deref(), Some(DEFAULT_ROBOTS_TOKEN));
        for posing in ["Mozilla/5.0 (X11)", "", "  ", "(bot)", "a.b/1"] {
            assert_eq!(robots_token(posing), None, "{posing}");
        }
    }

    #[test]
    fn the_interval_cannot_go_below_the_floor_and_crawl_delay_or_inf_are_handled() {
        let s = setup();
        let mut f = fetcher(&s);
        f.options.interval = Some(0.5);
        assert_eq!(f.interval(&Robots::allow_all()), Duration::from_secs(3));
        assert_eq!(
            f.interval(&Robots::parse("User-agent: *\nCrawl-delay: 7\n", DEFAULT_ROBOTS_TOKEN)),
            Duration::from_secs(7)
        );
        f.options.interval = Some(f64::INFINITY);
        assert_eq!(f.interval(&Robots::allow_all()), Duration::from_secs(3));
        f.options.interval = Some(1e19);
        assert_eq!(f.interval(&Robots::allow_all()), Duration::from_secs(crate::state::MAX_WAIT_SECS));
    }

    #[test]
    fn robots_disallow_and_foreign_hosts_send_nothing() {
        let s = setup();
        let f = fetcher(&s);
        s.mock.answers.borrow_mut().insert(
            ROBOTS.into(),
            vec![Response {
                status: 200,
                body: b"User-agent: *\nDisallow: /article/2025041512345\n".to_vec(),
                ..Default::default()
            }],
        );
        assert_eq!(f.article("https://www.hankyung.com/article/2025041512345")["status"], "robots");
        assert_eq!(f.article("https://www.hankyung.com/print/2025041512345")["status"], "not_article");
        assert_eq!(f.article("https://elsewhere.example/article/2025041512345")["status"], "not_article");
        assert!(matches!(f.get("https://www.hankyung.com/a b", "*/*", &[], 10), Outcome::Refused("not_allowed")));
        assert_eq!(s.mock.urls(), vec![ROBOTS]);
    }

    #[test]
    fn articles_older_than_the_outlet_serves_are_not_requested() {
        let s = setup();
        assert_eq!(fetcher(&s).article("https://www.hankyung.com/article/202412183989i")["status"], "too_old");
        assert!(s.mock.urls().is_empty());
    }

    #[test]
    fn a_cloudflare_challenge_stops_this_run_and_every_other() {
        let s = setup();
        let first = "https://www.hankyung.com/article/2025041500001";
        s.mock.on(first, 403, "", &[("cf-mitigated", "challenge")]);
        let f = fetcher(&s);
        assert_eq!(f.article(first)["status"], "cooldown");
        assert_eq!(f.stopped().as_deref(), Some("cooldown"));
        let other = Fetcher::new(s.registry.get("hankyung").unwrap(), &s.mock, &s.state, &s.clock, Options::default());
        assert_eq!(other.article("https://www.hankyung.com/article/2025041500002")["status"], "cooldown");
        assert_eq!(s.mock.urls().len(), 2);
    }

    #[test]
    fn a_challenge_on_robots_txt_cools_down_too() {
        let s = setup();
        s.mock.answers.borrow_mut().clear();
        s.mock.on(ROBOTS, 403, "", &[("cf-mitigated", "challenge")]);
        assert_eq!(fetcher(&s).article("https://www.hankyung.com/article/2025041500001")["status"], "cooldown");
        assert_eq!(s.mock.urls(), vec![ROBOTS]);
    }

    #[test]
    fn plain_403s_cool_down_only_when_they_keep_coming() {
        let s = setup();
        let f = fetcher(&s);
        for i in 0..BLOCK_STRIKES - 1 {
            let url = format!("https://www.hankyung.com/article/202504150000{i}");
            s.mock.on(&url, 403, "", &[]);
            assert_eq!(f.article(&url)["status"], "refused");
        }
        let ok = "https://www.hankyung.com/article/2025041500009";
        s.mock.on(ok, 200, PAGE, &[]);
        assert_eq!(f.article(ok)["status"], "ok"); // an answer resets the count
        for i in 0..BLOCK_STRIKES {
            let url = format!("https://www.hankyung.com/article/202504150001{i}");
            s.mock.on(&url, 403, "", &[]);
            f.article(&url);
        }
        assert!(s.state.cooldown("hankyung", &s.clock).unwrap().is_some());
    }

    #[test]
    fn answerless_or_5xx_in_a_row_cool_down_for_an_hour() {
        let s = setup();
        let f = fetcher(&s);
        for i in 0..MAX_FAILURES + 2 {
            let url = format!("https://www.hankyung.com/article/202504150000{i}");
            s.mock.on(&url, if i % 2 == 0 { 0 } else { 503 }, "", &[]);
            f.article(&url);
        }
        assert_eq!(s.mock.urls().len(), 1 + MAX_FAILURES as usize); // robots, then five failures, then nothing
        assert_eq!(s.state.cooldown("hankyung", &s.clock).unwrap().unwrap().reason, "no answer or 5xx in a row");
    }

    #[test]
    fn rate_limits_cool_down_for_retry_after_and_an_http_date_means_an_hour() {
        let s = setup();
        let url = "https://www.hankyung.com/article/2025041500001";
        s.mock.on(url, 429, "", &[("Retry-After", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        assert_eq!(fetcher(&s).article(url)["status"], "cooldown");
        s.clock.advance(Duration::from_secs(RATE_COOLDOWN_SECS as u64 - 10));
        assert!(s.state.cooldown("hankyung", &s.clock).unwrap().is_some());
        s.clock.advance(Duration::from_secs(20));
        assert!(s.state.cooldown("hankyung", &s.clock).unwrap().is_none());
    }

    #[test]
    fn a_cooldown_started_while_waiting_is_seen_before_sending() {
        struct Interrupting<'a> {
            inner: &'a FakeClock,
            state: &'a State,
        }
        impl Clock for Interrupting<'_> {
            fn now_millis(&self) -> i64 {
                self.inner.now_millis()
            }
            fn sleep(&self, d: Duration) {
                self.state.set_cooldown("hankyung", 100, "another process", self.inner).unwrap();
                self.inner.sleep(d);
            }
        }
        let s = setup();
        let clock = Interrupting { inner: &s.clock, state: &s.state };
        let f = Fetcher::new(s.registry.get("hankyung").unwrap(), &s.mock, &s.state, &clock, Options::default());
        assert_eq!(f.article("https://www.hankyung.com/article/2025041500001")["status"], "cooldown");
        assert_eq!(s.mock.urls(), vec![ROBOTS]); // the page was never asked for
    }

    #[test]
    fn a_busy_queue_is_reported_not_waited_for() {
        let s = setup();
        let url = "https://www.hankyung.com/article/2025041500001";
        s.mock.on(url, 200, PAGE, &[]);
        fetcher(&s).article("https://www.hankyung.com/article/2025041500002"); // robots and one page: the queue is 3 s
        let mut f = fetcher(&s);
        f.options.max_wait = Some(Duration::from_secs(1));
        let record = f.article(url);
        assert_eq!(record["status"], "busy");
        assert_eq!(f.stopped(), None); // a busy queue is not a block
        assert!(!s.mock.urls().contains(&url.to_string()));
    }

    #[test]
    fn unusable_state_sends_nothing() {
        let s = setup();
        std::fs::write(s.dir.join("keys").join("hankyung.json"), "{broken").unwrap();
        let f = fetcher(&s);
        assert_eq!(f.article("https://www.hankyung.com/article/2025041500001")["status"], "state_unavailable");
        assert!(s.mock.urls().is_empty());
        assert_eq!(f.stopped().as_deref(), Some("state_unavailable"));
    }

    #[test]
    fn redirects_are_followed_only_within_the_rules() {
        let s = setup();
        let f = fetcher(&s);
        let a = "https://www.hankyung.com/article/2025041500001";
        s.mock.on(a, 301, "", &[("Location", "/article/2025041500002")]);
        s.mock.on("https://www.hankyung.com/article/2025041500002", 200, PAGE, &[]);
        let record = f.article(a);
        assert_eq!(
            (record["status"].as_str(), record["final_url"].as_str()),
            (Some("ok"), Some("https://www.hankyung.com/article/2025041500002"))
        );
        let b = "https://www.hankyung.com/article/2025041500003";
        s.mock.on(b, 302, "", &[("Location", "https://tracker.example/x")]);
        assert_eq!(f.article(b)["status"], "redirect_blocked");
        let c = "https://www.hankyung.com/article/2025041500004";
        s.mock.on(c, 302, "", &[("Location", "/print/1")]);
        assert_eq!(f.article(c)["status"], "redirect_blocked");
    }

    #[test]
    fn robots_failures_are_cached_and_same_host_redirects_followed() {
        let s = setup();
        s.mock.answers.borrow_mut().clear();
        s.mock.on(ROBOTS, 503, "", &[]);
        let f = fetcher(&s);
        for i in 0..3 {
            assert_eq!(
                f.article(&format!("https://www.hankyung.com/article/202504150000{i}"))["status"],
                "robots_unavailable"
            );
        }
        assert_eq!(s.mock.urls(), vec![ROBOTS]); // asked once, not once per page

        let s = setup();
        s.mock.answers.borrow_mut().clear();
        s.mock.on(ROBOTS, 301, "", &[("Location", "/robots-new.txt")]);
        s.mock.on("https://www.hankyung.com/robots-new.txt", 200, "User-agent: *\nDisallow: /article/\n", &[]);
        assert_eq!(fetcher(&s).article("https://www.hankyung.com/article/2025041500001")["status"], "robots");

        let s = setup();
        s.mock.answers.borrow_mut().clear();
        s.mock.on(ROBOTS, 301, "", &[("Location", "https://other.example/robots.txt")]);
        assert_eq!(
            fetcher(&s).article("https://www.hankyung.com/article/2025041500001")["status"],
            "robots_unavailable"
        );
    }

    #[test]
    fn a_page_whose_body_selectors_miss_is_no_body_not_ok() {
        let s = setup();
        let url = "https://www.hankyung.com/article/2025041500001";
        s.mock.on(url, 200, "<html><head><meta property=\"og:description\" content=\"설명\"></head><body><div class=x>본문</div></body></html>", &[]);
        let record = fetcher(&s).article(url);
        assert_eq!((record["status"].as_str(), record["description"].as_str()), (Some("no_body"), Some("설명")));
    }

    #[test]
    fn redirect_chains_are_bounded_and_https_never_goes_down_to_http() {
        let s = setup();
        let f = fetcher(&s);
        for i in 0..10 {
            s.mock.on(
                &format!("https://www.hankyung.com/article/20250415000{i}0"),
                302,
                "",
                &[("Location", &format!("/article/20250415000{}0", i + 1))],
            );
        }
        assert_eq!(f.article("https://www.hankyung.com/article/2025041500000")["status"], "redirect_blocked");
        assert_eq!(s.mock.urls().len(), 1 + MAX_REDIRECTS + 1);
        let s = setup();
        let url = "https://www.hankyung.com/article/2025041500001";
        s.mock.on(url, 301, "", &[("Location", "http://www.hankyung.com/article/2025041500001")]);
        assert_eq!(fetcher(&s).article(url)["status"], "redirect_blocked");
    }

    #[test]
    fn unreadable_robots_stops_the_run() {
        let s = setup();
        s.mock.answers.borrow_mut().clear();
        s.mock.on(ROBOTS, 503, "", &[]);
        let f = fetcher(&s);
        assert_eq!(f.article("https://www.hankyung.com/article/2025041500001")["status"], "robots_unavailable");
        assert_eq!(f.stopped().as_deref(), Some("robots_unavailable"));
    }

    #[test]
    fn bodies_cut_off_in_transit_count_as_failures() {
        let s = setup();
        let f = fetcher(&s);
        for i in 0..MAX_FAILURES {
            let url = format!("https://www.hankyung.com/article/202504150000{i}");
            s.mock
                .answers
                .borrow_mut()
                .insert(url.clone(), vec![Response { status: 200, incomplete: true, ..Default::default() }]);
            assert!(["failed", "cooldown"].contains(&f.article(&url)["status"].as_str().unwrap()));
        }
        assert!(s.state.cooldown("hankyung", &s.clock).unwrap().is_some());
    }

    #[test]
    fn an_euc_kr_page_is_decoded_by_its_declaration() {
        let s = setup();
        let url = "https://www.hankyung.com/article/2025041500001";
        let html = "<html><head><meta charset=\"euc-kr\"><meta property=\"og:title\" content=\"삼성전자 실적\"></head>\
                    <body><div class=\"article-body\">본문입니다</div></body></html>";
        let bytes = encoding_rs::EUC_KR.encode(html).0.into_owned();
        s.mock
            .answers
            .borrow_mut()
            .insert(url.into(), vec![Response { status: 200, body: bytes, ..Default::default() }]);
        let record = fetcher(&s).article(url);
        assert_eq!((record["status"].as_str(), record["title"].as_str()), (Some("ok"), Some("삼성전자 실적")));
        assert_eq!(record["encoding"], "EUC-KR");
    }

    #[test]
    fn formats_choose_common_fields_raw_or_both() {
        let s = setup();
        let url = "https://www.hankyung.com/article/2025041500001";
        s.mock.on(url, 200, PAGE, &[]);
        let mut f = fetcher(&s);
        f.options.format = Format::Normalized;
        let normalized = f.article(url);
        assert!(normalized.get("raw").is_none() && normalized.get("title").is_some());
        f.options.format = Format::Raw;
        let raw = f.article(url);
        assert!(
            raw.get("title").is_none()
                && raw.get("body").is_none()
                && raw["raw"]["page"]["meta"]["og:title"].is_array()
        );
    }

    #[test]
    fn feeds_use_conditional_get_and_items_carry_common_fields() {
        let s = setup();
        let feed = "https://www.hankyung.com/feed/finance";
        let rss = r#"<rss><channel><item><title>T</title><link>https://www.hankyung.com/article/2025041500001</link>
            <pubDate>Tue, 15 Apr 2025 23:36:40 +0900</pubDate><author>홍길동 기자 (hong@example.com)</author></item>
            <item><title>no link</title></item></channel></rss>"#;
        s.mock.on(feed, 200, rss, &[("ETag", "\"v1\"")]);
        s.mock.on(feed, 304, "", &[]);
        let f = fetcher(&s);
        let mut lines = vec![];
        f.feed(feed, &mut |v| lines.push(v));
        assert_eq!(lines.iter().map(|l| l["kind"].as_str().unwrap()).collect::<Vec<_>>(), vec!["feed_item", "feed"]);
        assert_eq!(
            (lines[0]["id"].as_str(), lines[0]["published_at"].as_str()),
            (Some("hankyung:2025041500001"), Some("2025-04-15T14:36:40+00:00"))
        );
        assert_eq!(lines[0]["authors"], json!(["홍길동"]));
        assert!(!lines[0]["raw"].to_string().contains("example.com"));
        assert_eq!((lines[1]["items"].as_u64(), lines[1]["dropped"].as_u64()), (Some(1), Some(1)));
        lines.clear();
        f.feed(feed, &mut |v| lines.push(v));
        assert_eq!(lines[0]["status"], "not_modified");
        let second = &s.mock.calls.borrow()[2].1;
        assert!(second.iter().any(|(k, v)| k == "If-None-Match" && v == "\"v1\""));
    }

    #[test]
    fn a_sitemap_lists_articles_with_ids_and_counts_old_ones() {
        let s = setup();
        let map = "https://www.hankyung.com/sitemap/daily-article/20250102.xml";
        let locs = [
            "https://www.hankyung.com/article/202412180001i",
            "https://www.hankyung.com/article/2025010200001",
            "https://www.hankyung.com/tag/x",
        ];
        s.mock.on(map, 200, &locs.iter().map(|l| format!("<url><loc>{l}</loc></url>")).collect::<String>(), &[]);
        let mut lines = vec![];
        fetcher(&s).sitemap(NaiveDate::from_ymd_opt(2025, 1, 2).unwrap(), &mut |v| lines.push(v));
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[0]["url"].as_str(), lines[0]["id"].as_str()),
            (Some(locs[1]), Some("hankyung:2025010200001"))
        );
        assert_eq!((lines[1]["urls"].as_u64(), lines[1]["old"].as_u64()), (Some(1), Some(1)));
    }
}
