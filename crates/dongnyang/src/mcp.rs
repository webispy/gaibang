//! dongnyang as an MCP server (Model Context Protocol, JSON-RPC 2.0 over stdio, one message a line), so that an
//! agent can read the outlets' feeds and articles under the same rules as the command line: robots.txt, one pace
//! per outlet shared with every other process through the state directory, cool-downs after a block. A call
//! reads one outlet's feeds or one article; a call that would wait longer than `max_wait` for the outlet's turn
//! answers `busy` instead. Nothing but MCP messages goes to stdout.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

use crate::clock::Clock;
use crate::fetch::{Fetcher, Format, Options};
use crate::http::Transport;
use crate::normalize::{self, Input};
use crate::source::Registry;
use crate::state::State;

pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
pub const MAX_ITEMS: usize = 100;
pub const DEFAULT_ITEMS: usize = 30;
pub const DEFAULT_BODY_CHARS: usize = 20_000;

pub struct Server<'a> {
    pub registry: &'a Registry,
    pub transport: &'a dyn Transport,
    pub state: &'a State,
    pub clock: &'a dyn Clock,
    pub user_agent: String,
    pub max_wait: Duration,
}

const INSTRUCTIONS: &str = "Korean news outlets read politely: list_sources for the outlets, read_feed for an outlet's \
latest items, read_article for one article's fields and body (only URLs on an outlet's hosts). Each outlet is read at \
most every few seconds by everyone on this machine; `busy` means try again shortly, `cooldown` means the outlet asked \
us to stop for a while, `robots` means its robots.txt does not allow the page.";

fn tools() -> Value {
    json!([
        {"name": "list_sources", "description": "The outlets this server reads: id, name, hosts, whether it has feeds and sitemaps.",
         "inputSchema": {"type": "object", "properties": {}},
         "annotations": {"readOnlyHint": true, "openWorldHint": false}},
        {"name": "read_feed", "description": "An outlet's latest items from its RSS feeds: title, url, id, published_at, description.",
         "inputSchema": {"type": "object", "required": ["source"], "properties": {
             "source": {"type": "string", "description": "Outlet id from list_sources, e.g. yna"},
             "limit": {"type": "integer", "minimum": 1, "maximum": MAX_ITEMS, "description": "At most this many items (newest first), default 30"}}},
         "annotations": {"readOnlyHint": true, "openWorldHint": true}},
        {"name": "read_article", "description": "One article page: title, times, authors, categories, keywords, description and body text. The URL must be on an outlet's hosts.",
         "inputSchema": {"type": "object", "required": ["url"], "properties": {
             "url": {"type": "string", "description": "The article's address"},
             "max_body_chars": {"type": "integer", "minimum": 0, "description": "Cut the body at this many characters, default 20000"}}},
         "annotations": {"readOnlyHint": true, "openWorldHint": true}},
        {"name": "article_id", "description": "The article id of a URL (the same article at another address gets the same id), without reading it.",
         "inputSchema": {"type": "object", "required": ["url"], "properties": {"url": {"type": "string"}}},
         "annotations": {"readOnlyHint": true, "openWorldHint": false}}
    ])
}

/// The fields an agent reads, without the outlet's raw records.
const ARTICLE_FIELDS: &[&str] = &[
    "status",
    "url",
    "final_url",
    "id",
    "source",
    "outlet",
    "title",
    "subtitle",
    "published_at",
    "updated_at",
    "authors",
    "section_path",
    "categories",
    "keywords",
    "description",
    "image_url",
    "body",
];
const ITEM_FIELDS: &[&str] = &["title", "url", "id", "published_at", "updated_at", "description", "categories"];

fn pick(value: &Value, fields: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for field in fields {
        if let Some(v) = value.get(*field).filter(|v| !v.is_null()) {
            out.insert(field.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

fn result(value: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
    json!({"content": [{"type": "text", "text": text}], "structuredContent": value, "isError": is_error})
}

impl Server<'_> {
    fn options(&self) -> Options {
        Options {
            user_agent: self.user_agent.clone(),
            max_wait: Some(self.max_wait),
            format: Format::Normalized,
            ..Options::default()
        }
    }

    fn call(&self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "list_sources" => {
                let sources: Vec<Value> = self
                    .registry
                    .ids()
                    .map(|id| {
                        let spec = self.registry.get(id).unwrap().spec();
                        json!({"id": id, "outlet": spec.outlet, "hosts": spec.hosts, "feeds": spec.feeds.len(),
                               "sitemap": spec.sitemap.is_some()})
                    })
                    .collect();
                Ok(json!({"sources": sources}))
            }
            "read_feed" => {
                let source = args["source"].as_str().ok_or("source is required")?;
                let plugin =
                    self.registry.get(source).ok_or_else(|| format!("no such source: {source} (see list_sources)"))?;
                let limit = args["limit"].as_u64().map(|n| n as usize).unwrap_or(DEFAULT_ITEMS).clamp(1, MAX_ITEMS);
                let fetcher = Fetcher::new(plugin, self.transport, self.state, self.clock, self.options());
                let (mut items, mut feeds) = (vec![], vec![]);
                for feed in &plugin.spec().feeds {
                    if fetcher.stopped().is_some() {
                        break;
                    }
                    fetcher.feed(feed, &mut |line| match line["kind"].as_str() {
                        Some("feed_item") => items.push(line),
                        Some("feed") => feeds.push(json!({"url": line["url"], "status": line["status"]})),
                        _ => {}
                    });
                }
                items.sort_by(|a, b| b["published_at"].as_str().cmp(&a["published_at"].as_str()));
                let mut seen = std::collections::HashSet::new();
                let items: Vec<Value> = items
                    .into_iter()
                    .filter(|i| seen.insert(i["id"].as_str().unwrap_or_default().to_string()))
                    .take(limit)
                    .map(|i| pick(&i, ITEM_FIELDS))
                    .collect();
                Ok(json!({"source": source, "items": items, "feeds": feeds, "stopped": fetcher.stopped()}))
            }
            "read_article" => {
                let url = args["url"].as_str().ok_or("url is required")?;
                let plugin =
                    self.registry.for_url(url).ok_or("the URL is not on an outlet's hosts (see list_sources)")?;
                let fetcher = Fetcher::new(plugin, self.transport, self.state, self.clock, self.options());
                let mut article = pick(&fetcher.article(url), ARTICLE_FIELDS);
                let max = args["max_body_chars"].as_u64().map(|n| n as usize).unwrap_or(DEFAULT_BODY_CHARS);
                if let Some(body) = article["body"].as_str().filter(|b| b.chars().count() > max) {
                    let cut: String = body.chars().take(max).collect();
                    article["body"] = json!(cut);
                    article["body_cut"] = json!(true);
                }
                Ok(article)
            }
            "article_id" => {
                let url = args["url"].as_str().ok_or("url is required")?;
                let plugin =
                    self.registry.for_url(url).ok_or("the URL is not on an outlet's hosts (see list_sources)")?;
                let spec = plugin.spec();
                let input = Input { source: &spec.id, outlet: &spec.outlet, url, page: None, item: None, body: None };
                Ok(
                    json!({"url": url, "source": spec.id, "id": normalize::normalize(&input, plugin.normalize_spec()).id}),
                )
            }
            _ => Err(format!("no such tool: {name}")),
        }
    }

    /// The answer to one message, or None for a notification (and for a response sent to us).
    pub fn handle(&self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned()?;
        let method = message["method"].as_str()?;
        let params = &message["params"];
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let error =
            |code: i64, text: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}});
        Some(match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or_default();
                let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
                reply(json!({"protocolVersion": version, "capabilities": {"tools": {"listChanged": false}},
                             "serverInfo": {"name": "dongnyang", "version": env!("CARGO_PKG_VERSION")},
                             "instructions": INSTRUCTIONS}))
            }
            "ping" => reply(json!({})),
            "tools/list" => reply(json!({"tools": tools()})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                match self.call(name, &params["arguments"]) {
                    Ok(value) => reply(result(value, false)),
                    Err(text) => reply(result(json!({"error": text}), true)),
                }
            }
            _ => error(-32601, "method not found"),
        })
    }

    /// Serve until the input closes.
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let answer = match serde_json::from_str::<Value>(&line) {
                Ok(message) => self.handle(&message),
                Err(_) => {
                    Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}))
                }
            };
            if let Some(answer) = answer {
                writeln!(output, "{answer}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}
