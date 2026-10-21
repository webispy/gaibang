# gaibang

[![CI](https://github.com/webispy/gaibang/actions/workflows/ci.yml/badge.svg)](https://github.com/webispy/gaibang/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/webispy/gaibang)](https://github.com/webispy/gaibang/releases/latest)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

Polite data gatherers for Korean news and market data, written in Rust. Each tool prints one JSON line per thing
it reads and keeps no database — storing is the caller's job.

The name comes from **개방 (丐幇, Gaibang)**, the beggars' sect of wuxia fiction whose members gather news from
every corner of the land. Like them, these tools *ask* rather than take: they follow robots.txt, keep a slow pace per
site, and back off when refused.

| Tool | What it does | Docs (Korean) |
|---|---|---|
| **dongnyang** (동냥, "begging") | Reads news outlets' RSS feeds, sitemaps and article pages — once each, slowly — and maps every outlet's fields to one common schema. Also runs as an MCP server for agents. | [docs/dongnyang.md](docs/dongnyang.md), [output](docs/dongnyang-output.md) |
| **tagubong** (타구봉, the sect's staff) | Calls official APIs with your own key. Keys come only from the environment and never appear in output, errors or logs. | [docs/tagubong.md](docs/tagubong.md) |

Both tools share one state directory per machine, so every process — cron jobs, a dashboard, an agent — waits in
the same queue per site or API and honours the same cool-downs.

## Features

- **Polite by construction** — robots.txt (RFC 9309) for your own product token, at least 3 s between requests per
  outlet (longer if `Crawl-delay` says so), and a shared cool-down after a block signal: a Cloudflare challenge,
  429, repeated 403s, or repeated timeouts and 5xx. None of this can be turned off.
- **Read once** — the same address is read once per run, and a closed output stops the run before the next request,
  so nothing is fetched and thrown away.
- **Fail closed** — if the shared state cannot be read or written, nothing is sent.
- **Common schema** — title, times, authors, categories, keywords, body and more, mapped from JSON-LD, meta tags and
  RSS, with the outlet's own fields kept as `raw`. E-mail addresses are removed.
- **Plug-ins** — one TOML file per outlet; add your own with `--sources DIR` without rebuilding.
- **Keys stay secret** — tagubong takes keys from the environment only, puts them where each API wants them (query,
  path or header), and scrubs them from everything it prints.

### Supported sources

| dongnyang (outlets) | tagubong (APIs) |
|---|---|
| 연합뉴스 `yna`, 한국경제 `hankyung`, 매일경제 `mk`, 전자신문 `etnews`, 조선비즈 `chosunbiz`, 파이낸셜뉴스 `fnnews`, 이투데이 `etoday` | FRED, 한국투자증권 (KIS), 토스증권, 한국은행 ECOS, 공공데이터포털, 열린국회정보, 법제처, KRX |

## Install

Prebuilt binaries for Linux (x86_64, aarch64; static musl) and macOS (Apple silicon) are on the
[releases page](https://github.com/webispy/gaibang/releases/latest), with a `SHA256SUMS` file to check them against.

Or build from source (Rust 2024 edition):

```bash
cargo install --git https://github.com/webispy/gaibang dongnyang tagubong
```

## Quick start

Say who you are in the User-Agent (robots.txt is read for its first product token), and give every call the same
absolute state directory.

```bash
STATE=$HOME/.local/state/gaibang
UA="myresearch/1.0 (+https://example.org/contact)"

dongnyang sources                                                    # the outlets
dongnyang feed --source yna --state-dir $STATE --user-agent "$UA"    # latest items of an outlet
dongnyang article --source yna --state-dir $STATE --user-agent "$UA" < urls.txt

FRED_API_KEY=... tagubong fred observations --series DGS10 --start 2026-01-01 \
  --state-dir $STATE --user-agent "$UA"
```

Every line is a JSON object, and the last one is a `summary`. Exit codes: `0` done, `2` bad usage or setup, `3`
output closed, `4` stopped early (cool-down, refused key, time limit).

## Use from an agent (MCP)

`dongnyang mcp` is a stdio [Model Context Protocol](https://modelcontextprotocol.io) server with four tools:
`list_sources`, `read_feed`, `read_article` and `article_id`. It follows the same rules as the command line, and a
call that would wait longer than `--max-wait` (30 s by default) for its turn answers `busy` instead of blocking.

```bash
claude mcp add dongnyang -- dongnyang mcp --state-dir $HOME/.local/state/gaibang --user-agent "$UA"
```

## Development

```
crates/core        shared: the state directory (request slots, cool-downs, credentials), clock, HTTP
crates/dongnyang   the crawler; one plug-in per outlet in sources/*.toml
crates/tagubong    the API caller; one module per API (or a row in keyed::SPECS)
```

```bash
cargo test     # no network and no real keys: local servers and fake transports only
```

Parsers are tested on hand-written HTML that imitates each outlet's markup; no copies of articles are kept in the
repository. Before adding an outlet, check that its robots.txt and terms allow automated reading.

Pushing a `v*` tag runs the tests and publishes the binaries with `SHA256SUMS` to the releases page.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as
defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
