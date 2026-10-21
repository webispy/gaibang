//! Common fields from the raw record. Each field has an ordered list of raw keys (the plug-in may replace it);
//! the first key that yields a usable value wins and is named in `provenance`. Keys:
//!
//! - `meta:<name>` — a <meta> tag's content (`og:title`, `article:published_time`, `news_keywords`, ...)
//! - `jsonld:<path>` — a dotted path in an Article-typed JSON-LD object (`datePublished`, `author.name`);
//!   `jsonld:breadcrumb` — the names of a BreadcrumbList
//! - `rss:<name>[@attr]` — a feed item's child (`pubDate`, `dc:creator`, `media:content@url`)
//! - `canonical`, `title_tag`, `url:path` (the URL's leading lower-case path segments, for categories)

use std::collections::BTreeMap;
use std::sync::{LazyLock, OnceLock};

use chrono::{DateTime, FixedOffset, NaiveDateTime, SecondsFormat, Utc};
use regex::Regex;
use scraper::Html;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extract::{RawItem, RawPage};

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|k| k.to_string()).collect()
}

/// Raw keys per common field. A plug-in's `[map]` replaces a field's list.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapSpec {
    pub title: Option<Vec<String>>,
    pub subtitle: Option<Vec<String>>,
    pub published_at: Option<Vec<String>>,
    pub updated_at: Option<Vec<String>>,
    pub authors: Option<Vec<String>>,
    pub description: Option<Vec<String>>,
    pub image_url: Option<Vec<String>>,
    pub canonical_url: Option<Vec<String>>,
}

impl MapSpec {
    fn get(&self, field: &str) -> Vec<String> {
        let own = match field {
            "title" => &self.title,
            "subtitle" => &self.subtitle,
            "published_at" => &self.published_at,
            "updated_at" => &self.updated_at,
            "authors" => &self.authors,
            "description" => &self.description,
            "image_url" => &self.image_url,
            _ => &self.canonical_url,
        };
        own.clone().unwrap_or_else(|| match field {
            "title" => keys(&["jsonld:headline", "meta:og:title", "rss:title", "title_tag"]),
            "subtitle" => vec![],
            "published_at" => keys(&[
                "jsonld:datePublished",
                "meta:article:published_time",
                "meta:og:published_time",
                "rss:pubDate",
                "rss:dc:date",
            ]),
            "updated_at" => keys(&["jsonld:dateModified", "meta:article:modified_time"]),
            "authors" => keys(&[
                "jsonld:author.name",
                "jsonld:author",
                "meta:author",
                "meta:article:author",
                "rss:dc:creator",
                "rss:author",
                "body:byline",
            ]),
            "description" => {
                keys(&["meta:og:description", "meta:description", "jsonld:description", "rss:description"])
            }
            "image_url" => keys(&[
                "meta:og:image",
                "jsonld:image.url",
                "jsonld:image",
                "rss:media:content@url",
                "rss:image",
                "rss:enclosure@url",
            ]),
            _ => keys(&["canonical", "meta:og:url"]),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CategorySpec {
    pub keys: Vec<String>,
    pub separators: Vec<String>,
    /// Whether a split value is a path (대분류|중분류) or a list of separate categories.
    pub hierarchical: bool,
    pub ignore: Vec<String>,
    /// How many leading lower-case URL path segments `url:path` gives (0: none).
    pub url_depth: usize,
}

impl Default for CategorySpec {
    fn default() -> CategorySpec {
        CategorySpec {
            keys: keys(&["jsonld:articleSection", "jsonld:breadcrumb", "meta:article:section", "rss:category"]),
            separators: keys(&["|", ">", "›"]), // not "/": "IT/과학" is one name
            hierarchical: true,
            ignore: vec![],
            url_depth: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct KeywordSpec {
    pub keys: Vec<String>,
    pub separators: Vec<String>,
    pub ignore: Vec<String>,
}

impl Default for KeywordSpec {
    fn default() -> KeywordSpec {
        KeywordSpec {
            keys: keys(&["jsonld:keywords", "meta:news_keywords"]),
            separators: keys(&[",", ";"]), // not "·": "한·미 정상회담" is one keyword
            ignore: vec![],
        }
    }
}

/// Per-outlet rules of the mapping (all optional in the plug-in's TOML).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NormalizeSpec {
    pub map: MapSpec,
    pub categories: CategorySpec,
    pub keywords: KeywordSpec,
    /// A regex with one group matched against the URL path: the article's own id (`source:<id>`).
    pub id_pattern: Option<String>,
    /// Trailing title decorations besides the outlet's name (" | 연합뉴스" is removed by default).
    pub title_suffixes: Vec<String>,
    /// Author values that are not people (the outlet's other names, desks).
    pub author_ignore: Vec<String>,
    /// Query parameters removed from the canonical URL.
    pub strip_params: Vec<String>,
    #[serde(skip)]
    pub(crate) compiled: Compiled,
}

/// Regexes built once per plug-in, on first use.
#[derive(Debug, Clone, Default)]
pub(crate) struct Compiled {
    id: OnceLock<Option<Regex>>,
    title_suffixes: OnceLock<Vec<Regex>>,
}

impl NormalizeSpec {
    fn id_regex(&self) -> Option<&Regex> {
        self.compiled.id.get_or_init(|| self.id_pattern.as_deref().and_then(|p| Regex::new(p).ok())).as_ref()
    }

    /// " | 한국경제", " - 매일경제": a separator with spaces on both sides, then the name, at the end. Longest
    /// first, so "제목 - 미래를 보는 창 - 전자신문" loses the whole tail. A bare "…: 한국경제" (the noun) stays.
    fn suffix_regexes(&self, outlet: &str) -> &[Regex] {
        self.compiled.title_suffixes.get_or_init(|| {
            let mut names: Vec<&str> =
                std::iter::once(outlet).chain(self.title_suffixes.iter().map(String::as_str)).collect();
            names.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
            names.iter().filter_map(|n| Regex::new(&format!(r"\s+[|\-–—]\s+{}\s*$", regex::escape(n))).ok()).collect()
        })
    }
}

macro_rules! regex {
    ($name:ident, $pattern:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pattern).unwrap());
    };
}

regex!(BYLINE, r"^(\S{2,4})\s?(기자|특파원|선임기자)\s*(\([^)]*\))?$");
regex!(TAGS, r"</?[A-Za-z][^>]*>");
regex!(
    EMAIL,
    r"[A-Za-z0-9._%+-]+@[A-Za-z][A-Za-z0-9-]*(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}|\[email\s*(?:&#160;|\s)?protected\]"
);
regex!(IMAGE_FILE, r"(?i)\.(png|jpe?g|gif|webp|svg|avif)$");
regex!(EMPTY_BRACKETS, r"\(\s*\)|\[\s*\]");
regex!(RFC822_COMMA, r"^([A-Za-z]{3}),(\S)");
regex!(COLON_OFFSET, r"([+-]\d\d):(\d\d)$");
regex!(LOGO, r"(?i)/logos?/|/logo[_.-]|[_-]logo\.");
regex!(AUTHOR_SPLIT, r"[,/·]|\s{2,}");
regex!(PARENS, r"\([^)]*\)");
regex!(ROLES, r"^((글|사진|영상|그래픽|정리|진행)\s*[·/]?\s*)+\s");
regex!(URL_SEGMENT, r"^[a-z][a-z0-9_-]*$");

const IGNORED_CATEGORIES: &[&str] = &["홈", "home", "전체", "뉴스", "최신뉴스", "기사", "news", "전체기사"];
const TRACKING: &[&str] =
    &["sns", "utm_source", "utm_medium", "utm_campaign", "utm_content", "utm_term", "fbclid", "gclid"];
const AUTHOR_TITLES: &[&str] =
    &["선임기자", "객원기자", "인턴기자", "수습기자", "전문기자", "기자", "특파원", "논설위원"];

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Normalized {
    pub id: String,
    pub canonical_url: Option<String>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub published_at: Option<String>,
    pub updated_at: Option<String>,
    pub authors: Vec<String>,
    pub section_path: Vec<String>,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    pub description: Option<String>,
    pub image_url: Option<String>,
    /// Field name to the raw key its value came from.
    pub provenance: BTreeMap<String, String>,
}

/// What normalization reads: the URL, a page's raw record and body, and/or a feed item's raw record.
pub struct Input<'a> {
    pub source: &'a str,
    pub outlet: &'a str,
    pub url: &'a str,
    pub page: Option<&'a RawPage>,
    pub item: Option<&'a RawItem>,
    pub body: Option<&'a str>,
}

fn is_article(object: &Value) -> bool {
    let types = match object.get("@type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    types.iter().any(|t| t.ends_with("Article"))
}

fn walk_path<'v>(value: &'v Value, path: &[&str], out: &mut Vec<&'v Value>) {
    match value {
        Value::Array(items) => items.iter().for_each(|v| walk_path(v, path, out)),
        _ if path.is_empty() => out.push(value),
        Value::Object(object) => {
            if let Some(next) = object.get(path[0]) {
                walk_path(next, &path[1..], out);
            }
        }
        _ => {}
    }
}

fn strings(values: Vec<&Value>) -> Vec<String> {
    values
        .into_iter()
        .filter_map(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .collect()
}

impl Input<'_> {
    /// The values of a JSON-LD key grouped by object (a list in one object is one group), other keys one group
    /// per value: a hierarchical outlet's `articleSection: ["경제", "일반경제"]` is one path.
    pub fn groups(&self, key: &str) -> Vec<Vec<String>> {
        match (key.split_once(':'), self.page) {
            (Some(("jsonld", name)), Some(page)) if name != "breadcrumb" => {
                let path: Vec<&str> = name.split('.').collect();
                page.jsonld
                    .iter()
                    .filter(|o| is_article(o))
                    .map(|object| {
                        let mut found = vec![];
                        walk_path(object, &path, &mut found);
                        strings(found)
                    })
                    .filter(|g| !g.is_empty())
                    .collect()
            }
            _ => self.values(key).into_iter().map(|v| vec![v]).collect(),
        }
    }

    /// Every value a raw key gives, in order.
    pub fn values(&self, key: &str) -> Vec<String> {
        let (kind, name) = key.split_once(':').unwrap_or((key, ""));
        match kind {
            "body" if name == "byline" => {
                // The last short paragraph ending in a reporter's title: "홍길동 기자", "사아자기자".
                let Some(body) = self.body else { return vec![] };
                body.rsplit("\n\n")
                    .take(4)
                    .find_map(|p| BYLINE.captures(p.trim()).map(|c| c[1].to_string()))
                    .into_iter()
                    .collect()
            }
            "meta" => self.page.and_then(|p| p.meta.get(&name.to_lowercase())).cloned().unwrap_or_default(),
            "jsonld" if name == "breadcrumb" => {
                let Some(page) = self.page else { return vec![] };
                let mut out = vec![];
                for object in
                    page.jsonld.iter().filter(|o| o.get("@type").and_then(Value::as_str) == Some("BreadcrumbList"))
                {
                    let mut names = vec![];
                    walk_path(object, &["itemListElement", "name"], &mut names);
                    walk_path(object, &["itemListElement", "item", "name"], &mut names);
                    let joined = strings(names).join("|");
                    if !joined.is_empty() {
                        out.push(joined);
                    }
                }
                out
            }
            "jsonld" => {
                let Some(page) = self.page else { return vec![] };
                let path: Vec<&str> = name.split('.').collect();
                let mut out = vec![];
                for object in page.jsonld.iter().filter(|o| is_article(o)) {
                    let mut found = vec![];
                    walk_path(object, &path, &mut found);
                    out.extend(strings(found));
                }
                out
            }
            "rss" => {
                let Some(item) = self.item else { return vec![] };
                let (child, attr) = match name.split_once('@') {
                    Some((child, attr)) => (child, Some(format!("@{attr}"))),
                    None => (name, None),
                };
                let Some(Value::Array(values)) = item.get(child) else { return vec![] };
                values
                    .iter()
                    .filter_map(|v| match (v, &attr) {
                        (Value::String(text), None) => Some(text.clone()),
                        (Value::Object(o), None) => o.get("#text").and_then(Value::as_str).map(str::to_string),
                        (Value::Object(o), Some(a)) => o.get(a).and_then(Value::as_str).map(str::to_string),
                        _ => None,
                    })
                    .collect()
            }
            "canonical" => self.page.and_then(|p| p.canonical.clone()).into_iter().collect(),
            "title_tag" => self.page.and_then(|p| p.title.clone()).into_iter().collect(),
            _ => vec![],
        }
    }
}

/// Text as a reader sees it: tags become spaces, entities are decoded, whitespace folded, e-mail addresses
/// removed (they are personal data and carry nothing for the cards).
pub fn clean_text(text: &str) -> String {
    // Only real tags go; "<인터뷰>" and a stray "<" are text (escaped before entities are decoded).
    let spaced = TAGS.replace_all(text, " ");
    let decoded: String = if spaced.contains('&') {
        Html::parse_fragment(&spaced.replace('<', "&lt;")).root_element().text().collect()
    } else {
        spaced.into_owned()
    };
    fold(&scrub_emails(&decoded))
}

/// E-mail addresses removed ("photo@2x.png" is a file name and stays).
pub fn scrub_emails(text: &str) -> String {
    EMAIL
        .replace_all(
            text,
            |c: &regex::Captures| if IMAGE_FILE.is_match(&c[0]) { c[0].to_string() } else { String::new() },
        )
        .into_owned()
}

/// A body without e-mail addresses (and the brackets they leave), its paragraphs tidied again.
pub fn scrub_body(body: &str) -> String {
    body.split("\n\n")
        .map(|p| fold(&EMPTY_BRACKETS.replace_all(&scrub_emails(p), "")))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Scrub e-mail addresses from every string in a raw record.
pub fn scrub_value(value: &mut Value) {
    match value {
        Value::String(s) => *s = scrub_emails(s),
        Value::Array(items) => items.iter_mut().for_each(scrub_value),
        Value::Object(object) => object.values_mut().for_each(scrub_value),
        _ => {}
    }
}

pub fn fold(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

const KST: i32 = 9 * 3600;

/// A moment with a time of day, as UTC. Date-only values are not moments and give None.
pub fn moment(text: &str) -> Option<String> {
    let text = text.trim();
    let utc = |m: DateTime<FixedOffset>| m.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Secs, false);
    if let Ok(m) = DateTime::parse_from_rfc3339(text) {
        return Some(utc(m));
    }
    let rfc822 = RFC822_COMMA.replace(text, "$1, $2"); // "Mon,19 Oct" (파이낸셜뉴스)
    let rfc822 = COLON_OFFSET.replace(&rfc822, "$1$2"); // "+09:00" (매일경제)
    if let Ok(m) = DateTime::parse_from_rfc2822(&rfc822) {
        return Some(utc(m));
    }
    for format in
        ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%z"]
    {
        if let Ok(m) = DateTime::parse_from_str(text, format) {
            return Some(utc(m));
        }
    }
    let kst = FixedOffset::east_opt(KST).unwrap(); // Korean outlets write local times without an offset
    for format in
        ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M", "%Y.%m.%d %H:%M:%S", "%Y.%m.%d %H:%M"]
    {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, format) {
            return naive.and_local_timezone(kst).single().map(utc);
        }
    }
    None
}

fn clean_title(text: &str, suffixes: &[Regex]) -> String {
    let mut title = clean_text(text);
    for re in suffixes {
        title = re.replace(&title, "").into_owned();
    }
    title
}

const DESKS: &[&str] = &["팀", "부", "데스크", "센터", "취재단", "편집국", "뉴스룸", "온라인", "디지털"];

fn clean_author(text: &str, outlet: &str, ignore: &[String]) -> Option<String> {
    let mut name = fold(text);
    if let Some((_, after)) = name.rsplit_once('=') {
        name = after.trim().to_string(); // "세종=라마바": the dateline
    }
    for title in AUTHOR_TITLES {
        if let Some(stripped) = name.strip_suffix(title) {
            name = stripped.trim().to_string();
            break;
        }
    }
    // "홍길동 한국경제": the outlet's name next to the reporter's
    name = name.split_whitespace().filter(|w| *w != outlet).collect::<Vec<_>>().join(" ");
    let lower = name.to_lowercase();
    let not_a_person = name.chars().count() < 2
        || name.chars().count() > 20
        || lower.starts_with("http")
        || lower.contains("www.")
        || lower.contains(".com")
        || lower.contains(".co.kr")
        || name.contains("뉴스")
        || DESKS.iter().any(|d| name.ends_with(d) && name.chars().count() > 3)
        || ignore.iter().any(|i| i == &name);
    (!not_a_person).then_some(name)
}

fn unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    values.into_iter().filter(|v| seen.insert(v.clone())).collect()
}

fn canonical(url: &str, base: &str, strip: &[String]) -> Option<String> {
    let absolute = if url.starts_with("https://") || url.starts_with("http://") {
        url.to_string()
    } else if url.starts_with('/') {
        let origin_end =
            base.find("://").map(|i| i + 3).and_then(|s| base[s..].find('/').map(|j| s + j)).unwrap_or(base.len());
        format!("{}{url}", &base[..origin_end])
    } else {
        return None;
    };
    let (path, query) = match absolute.split_once('?') {
        Some((p, q)) => (p.to_string(), q),
        None => return Some(absolute.split('#').next().unwrap_or("").to_string()),
    };
    let query = query.split('#').next().unwrap_or("");
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or("").to_lowercase();
            !name.is_empty()
                && !TRACKING.contains(&name.as_str())
                && !name.starts_with("utm_")
                && !strip.iter().any(|s| s.eq_ignore_ascii_case(&name))
        })
        .collect();
    Some(if kept.is_empty() { path } else { format!("{path}?{}", kept.join("&")) })
}

pub fn normalize(input: &Input, spec: &NormalizeSpec) -> Normalized {
    let mut out = Normalized::default();
    let mut provenance = BTreeMap::new();
    let first = |field: &str, convert: &dyn Fn(&str) -> Option<String>| -> Option<(String, String)> {
        spec.map
            .get(field)
            .iter()
            .find_map(|key| input.values(key).iter().find_map(|v| convert(v)).map(|v| (v, key.clone())))
    };
    let text = |v: &str| Some(clean_text(v)).filter(|t| !t.is_empty());
    let mut set = |field: &str, found: Option<(String, String)>| {
        found.map(|(value, key)| {
            provenance.insert(field.to_string(), key);
            value
        })
    };
    // Titles: decorations are taken off what the page shows (og:title, <title>, RSS), never off JSON-LD's headline.
    let suffixes = spec.suffix_regexes(input.outlet);
    out.title = set(
        "title",
        spec.map.get("title").iter().find_map(|key| {
            let rules: &[Regex] = if key.starts_with("jsonld:") { &[] } else { suffixes };
            input.values(key).iter().map(|v| clean_title(v, rules)).find(|t| !t.is_empty()).map(|t| (t, key.clone()))
        }),
    );
    out.subtitle = set("subtitle", first("subtitle", &text));
    out.published_at = set("published_at", first("published_at", &moment));
    out.updated_at = set("updated_at", first("updated_at", &moment));
    out.description = set("description", first("description", &text));
    out.image_url = set(
        "image_url",
        first("image_url", &|v| {
            // a site logo is not the article's image
            Some(v.trim().to_string())
                .filter(|u| (u.starts_with("https://") || u.starts_with("http://")) && !LOGO.is_match(u))
        }),
    );
    out.canonical_url =
        set("canonical_url", first("canonical_url", &|v| canonical(v.trim(), input.url, &spec.strip_params)));

    // Authors: the first key that names at least one person.
    for key in spec.map.get("authors") {
        let names: Vec<String> = input
            .values(&key)
            .iter()
            .flat_map(|v| {
                // "(서울=연합뉴스) 홍길동 기자", "홍길동 (hong@…)", "글·사진 홍길동 기자"
                let v = PARENS.replace_all(&scrub_emails(v), " ").into_owned();
                let v = ROLES.replace(&v, "").into_owned();
                AUTHOR_SPLIT
                    .split(&v)
                    .filter_map(|n| clean_author(n, input.outlet, &spec.author_ignore))
                    .collect::<Vec<_>>()
            })
            .collect();
        if !names.is_empty() {
            out.authors = unique(names);
            provenance.insert("authors".into(), key);
            break;
        }
    }

    // Categories: every key, every value; split into paths; noise left out.
    let c = &spec.categories;
    let title = out.title.clone().unwrap_or_default();
    let keep = |part: &str| {
        let lower = part.to_lowercase();
        !part.is_empty()
            && !part.chars().all(|ch| ch.is_ascii_digit())
            && part != input.outlet
            && part != title
            && part.chars().count() <= 20 // a breadcrumb's last step is often the headline, a little changed
            && !IGNORED_CATEGORIES.contains(&lower.as_str())
            && !c.ignore.iter().any(|i| i == part)
    };
    let mut paths: Vec<(Vec<String>, String)> = vec![];
    for key in &c.keys {
        let groups: Vec<Vec<String>> = if key == "url:path" {
            url_categories(input.url, c.url_depth).into_iter().map(|v| vec![v]).collect()
        } else if c.hierarchical {
            input.groups(key)
        } else {
            input.values(key).into_iter().map(|v| vec![v]).collect()
        };
        for group in groups {
            let mut parts: Vec<String> = group.iter().map(|v| clean_text(v)).collect();
            for sep in &c.separators {
                parts = parts.iter().flat_map(|p| p.split(sep.as_str()).map(|s| s.trim().to_string())).collect();
            }
            let parts: Vec<String> = parts.into_iter().filter(|p| keep(p)).collect();
            if parts.is_empty() {
                continue;
            }
            if c.hierarchical || key == "url:path" {
                paths.push((parts, key.clone()));
            } else {
                paths.extend(parts.into_iter().map(|p| (vec![p], key.clone())));
            }
        }
    }
    if let Some((path, key)) = paths.iter().fold(None::<&(Vec<String>, String)>, |best, p| match best {
        Some(b) if b.0.len() >= p.0.len() => Some(b),
        _ => Some(p),
    }) {
        out.section_path = path.clone();
        provenance.insert("section_path".into(), key.clone());
    }
    out.categories = unique(paths.iter().flat_map(|(p, _)| p.iter().cloned()));
    if !out.categories.is_empty() {
        provenance.insert("categories".into(), unique(paths.iter().map(|(_, k)| k.clone())).join(","));
    }

    // Keywords: every key, split, noise left out.
    let k = &spec.keywords;
    let mut keyword_keys = vec![];
    let mut words = vec![];
    for key in &k.keys {
        for value in input.values(key) {
            let mut parts = vec![clean_text(&value)];
            for sep in &k.separators {
                parts = parts
                    .iter()
                    .flat_map(|p| p.split(sep.as_str()).map(|s| s.trim().trim_start_matches('#').to_string()))
                    .collect();
            }
            for part in parts {
                if !part.is_empty() && part.chars().count() <= 40 && part != input.outlet && !k.ignore.contains(&part) {
                    words.push(part);
                    if !keyword_keys.contains(key) {
                        keyword_keys.push(key.clone());
                    }
                }
            }
        }
    }
    out.keywords = unique(words);
    if !keyword_keys.is_empty() {
        provenance.insert("keywords".into(), keyword_keys.join(","));
    }

    // The article's own id: from the URL, else the canonical URL; without a match, the URL itself with tracking
    // parameters removed (the same for feed, sitemap and page lines of one address).
    let id = spec.id_regex().and_then(|re| {
        [Some(input.url), out.canonical_url.as_deref()].into_iter().flatten().find_map(|u| {
            let path = crate::parse::host_and_path(u)?.1;
            let path = path.split('?').next().unwrap_or("");
            re.captures(path).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())
        })
    });
    let fallback = || canonical(input.url, input.url, &spec.strip_params).unwrap_or_else(|| input.url.to_string());
    out.id = format!("{}:{}", input.source, id.unwrap_or_else(fallback));
    out.provenance = provenance;
    out
}

/// The URL's leading lower-case path segments ("/industry/company/2026/10/20/X" → ["industry", "company"]).
fn url_categories(url: &str, depth: usize) -> Option<String> {
    let path = crate::parse::host_and_path(url)?.1;
    let parts: Vec<&str> =
        path.split(['/', '?']).filter(|s| !s.is_empty()).take_while(|s| URL_SEGMENT.is_match(s)).take(depth).collect();
    (!parts.is_empty()).then(|| parts.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(meta: &[(&str, &str)], jsonld: Vec<Value>) -> RawPage {
        let mut raw = RawPage::default();
        for (k, v) in meta {
            raw.meta.entry(k.to_string()).or_default().push(v.to_string());
        }
        raw.jsonld = jsonld;
        raw
    }

    fn input<'a>(url: &'a str, page: Option<&'a RawPage>, item: Option<&'a RawItem>) -> Input<'a> {
        Input { source: "src", outlet: "연합뉴스", url, page, item, body: None }
    }

    #[test]
    fn moments_in_the_forms_outlets_use() {
        assert_eq!(moment("2026-10-20T11:32:28.905Z").as_deref(), Some("2026-10-20T11:32:28+00:00"));
        assert_eq!(moment("Mon,19 Oct 2026 22:23:21 +0900").as_deref(), Some("2026-10-19T13:23:21+00:00"));
        assert_eq!(moment("Tue, 20 Oct 2026 20:14:09 +09:00").as_deref(), Some("2026-10-20T11:14:09+00:00"));
        assert_eq!(moment("2026-10-20 17:46:00+0900").as_deref(), Some("2026-10-20T08:46:00+00:00"));
        assert_eq!(moment("2026-10-20 17:46").as_deref(), Some("2026-10-20T08:46:00+00:00"));
        assert_eq!(moment("2026-10-20"), None); // a date is not a moment
    }

    #[test]
    fn jsonld_wins_over_meta_and_rss_and_provenance_says_so() {
        let raw = page(
            &[("article:published_time", "2026-10-20T11:00:00+09:00"), ("og:title", "제목 | 연합뉴스")],
            vec![
                json!({"@type": "NewsArticle", "datePublished": "2026-10-20T10:00:00+09:00", "author": [{"name": "가나다"}]}),
            ],
        );
        let out = normalize(&input("https://h/view/AKR1", Some(&raw), None), &NormalizeSpec::default());
        assert_eq!(out.published_at.as_deref(), Some("2026-10-20T01:00:00+00:00"));
        assert_eq!(out.provenance["published_at"], "jsonld:datePublished");
        assert_eq!(out.title.as_deref(), Some("제목")); // the outlet's name is not part of the title
        assert_eq!(out.authors, vec!["가나다"]);
    }

    #[test]
    fn a_plug_in_can_send_a_field_elsewhere() {
        // 이투데이's RSS pubDate is the time of the last edit.
        let mut item = RawItem::new();
        item.insert("pubDate".into(), json!(["Tue, 20 Oct 2026 17:46:00 +0900"]));
        let mut spec = NormalizeSpec::default();
        spec.map.published_at = Some(vec!["jsonld:datePublished".into()]);
        spec.map.updated_at = Some(vec!["jsonld:dateModified".into(), "rss:pubDate".into()]);
        let out = normalize(&input("https://h/1", None, Some(&item)), &spec);
        assert_eq!((out.published_at, out.updated_at.as_deref()), (None, Some("2026-10-20T08:46:00+00:00")));
    }

    #[test]
    fn authors_lose_datelines_titles_emails_and_non_people() {
        let mut item = RawItem::new();
        item.insert(
            "author".into(),
            json!([
                "세종=라마바 (gana@example.co.kr)",
                "moon@example.com 사아자",
                "차카타 기자, 파하가기자",
                "www.etnews.com",
                "연합뉴스"
            ]),
        );
        let out = normalize(&input("https://h/1", None, Some(&item)), &NormalizeSpec::default());
        assert_eq!(out.authors, vec!["라마바", "사아자", "차카타", "파하가"]);
    }

    #[test]
    fn categories_split_into_paths_and_lose_noise() {
        let mut item = RawItem::new();
        item.insert("category".into(), json!(["경제|일반경제", " 정책 "]));
        let raw = page(
            &[("article:section", "경제"), ("dd:category", "61")],
            vec![
                json!({"@type": "BreadcrumbList", "itemListElement": [{"name": "홈"}, {"name": "기업"}, {"name": "기사 제목"}]}),
                json!({"@type": "NewsArticle", "headline": "기사 제목"}),
            ],
        );
        let out = normalize(&input("https://h/1", Some(&raw), Some(&item)), &NormalizeSpec::default());
        assert_eq!(out.section_path, vec!["경제", "일반경제"]);
        assert_eq!(out.categories, vec!["기업", "경제", "일반경제", "정책"]);
        assert_eq!(out.provenance["categories"], "jsonld:breadcrumb,meta:article:section,rss:category");
    }

    #[test]
    fn a_flat_outlet_and_url_path_categories() {
        let mut spec = NormalizeSpec::default();
        spec.categories.hierarchical = false;
        spec.categories.keys = vec!["rss:category".into(), "url:path".into()];
        spec.categories.url_depth = 2;
        let mut item = RawItem::new();
        item.insert("category".into(), json!(["증권|코스닥"]));
        let out = normalize(&input("https://h/industry/company/2026/10/20/ABC/", None, Some(&item)), &spec);
        assert_eq!(out.section_path, vec!["industry", "company"]);
        assert_eq!(out.categories, vec!["증권", "코스닥", "industry", "company"]);
    }

    #[test]
    fn a_jsonld_section_list_is_one_path_and_a_logo_is_no_image() {
        let raw = page(
            &[("og:image", "https://static.h/resources/img/logo/h_og.jpg")],
            vec![
                json!({"@type": "NewsArticle", "articleSection": ["경제", "일반경제"], "image": {"url": "https://img.h/1.jpg"}}),
            ],
        );
        let out = normalize(&input("https://h/1", Some(&raw), None), &NormalizeSpec::default());
        assert_eq!(out.section_path, vec!["경제", "일반경제"]);
        assert_eq!(out.image_url.as_deref(), Some("https://img.h/1.jpg"));
    }

    #[test]
    fn the_byline_at_the_end_of_the_body_names_the_reporter() {
        let raw = page(&[], vec![json!({"@type": "NewsArticle", "author": {"name": "연합뉴스"}})]);
        let body = "첫 문단.\n\n둘째 문단.\n\n사아자기자\n\n※ 저작권자 © 무단전재 금지";
        let out =
            normalize(&Input { body: Some(body), ..input("https://h/1", Some(&raw), None) }, &NormalizeSpec::default());
        assert_eq!((out.authors, out.provenance["authors"].as_str()), (vec!["사아자".to_string()], "body:byline"));
    }

    #[test]
    fn title_suffixes_need_spaces_and_go_longest_first_but_not_from_jsonld() {
        let spec =
            NormalizeSpec { title_suffixes: vec!["미래를 보는 창 - 전자신문".into()], ..Default::default() };
        let raw = page(&[("og:title", "제목 - 미래를 보는 창 - 전자신문")], vec![]);
        let one = |raw: &RawPage, outlet: &'static str| {
            normalize(&Input { outlet, ..input("https://h/1", Some(raw), None) }, &NormalizeSpec { ..spec.clone() })
                .title
        };
        assert_eq!(one(&raw, "전자신문").as_deref(), Some("제목"));
        let noun = page(&[("og:title", "IMF 전망: 한국경제"), ("twitter:title", "x")], vec![]);
        assert_eq!(one(&noun, "한국경제").as_deref(), Some("IMF 전망: 한국경제"));
        let glued = page(&[("og:title", "관세 폭탄-한국경제")], vec![]);
        assert_eq!(one(&glued, "한국경제").as_deref(), Some("관세 폭탄-한국경제"));
        let headline = page(&[], vec![json!({"@type": "NewsArticle", "headline": "미국 - 연합뉴스"})]);
        assert_eq!(one(&headline, "연합뉴스").as_deref(), Some("미국 - 연합뉴스"));
    }

    #[test]
    fn authors_from_datelines_desks_roles_and_outlet_words() {
        let mut item = RawItem::new();
        item.insert(
            "author".into(),
            json!([
                "(서울=연합뉴스) 홍길동 기자",
                "디지털뉴스팀",
                "김철수 연합뉴스 기자",
                "글·사진 이영희 기자",
                "온라인뉴스부"
            ]),
        );
        let out = normalize(&input("https://h/1", None, Some(&item)), &NormalizeSpec::default());
        assert_eq!(out.authors, vec!["홍길동", "김철수", "이영희"]);
        let raw = page(&[], vec![json!({"@type": "NewsArticle", "author": "박민수 기자"})]);
        assert_eq!(
            normalize(&input("https://h/1", Some(&raw), None), &NormalizeSpec::default()).authors,
            vec!["박민수"]
        );
    }

    #[test]
    fn a_byline_with_a_parenthesised_email_is_found() {
        let body = scrub_body("본문입니다.\n\n홍길동 기자(hong@example.com)");
        assert_eq!(body, "본문입니다.\n\n홍길동 기자");
        let out =
            normalize(&Input { body: Some(&body), ..input("https://h/1", None, None) }, &NormalizeSpec::default());
        assert_eq!(out.authors, vec!["홍길동"]);
    }

    #[test]
    fn compound_keywords_and_slashed_categories_stay_whole() {
        let raw = page(&[("news_keywords", "한·미 정상회담, 삼성·LG"), ("article:section", "IT/과학")], vec![]);
        let out = normalize(&input("https://h/1", Some(&raw), None), &NormalizeSpec::default());
        assert_eq!(out.keywords, vec!["한·미 정상회담", "삼성·LG"]);
        assert_eq!(out.section_path, vec!["IT/과학"]);
    }

    #[test]
    fn image_file_names_with_at_signs_survive_and_text_markers_stay() {
        assert_eq!(scrub_emails("https://img/photo@2x.png kim@example.com"), "https://img/photo@2x.png ");
        assert_eq!(clean_text("<인터뷰> 가 < 나 &amp; <b>다</b>"), "<인터뷰> 가 < 나 & 다");
    }

    #[test]
    fn keywords_split_and_site_wide_ones_can_be_left_out() {
        let raw = page(&[("news_keywords", "해외플랜트,수주; #원전"), ("keywords", "전자신문,뉴스")], vec![]);
        let out = normalize(&input("https://h/1", Some(&raw), None), &NormalizeSpec::default());
        assert_eq!(out.keywords, vec!["해외플랜트", "수주", "원전"]); // meta keywords are not read by default
    }

    #[test]
    fn ids_come_from_the_url_and_canonical_urls_lose_tracking() {
        let raw = page(&[("og:url", "https://h/20261020000418?SNS=00002&page=2")], vec![]);
        let spec = NormalizeSpec { id_pattern: Some(r"^/(\d{14})$".into()), ..Default::default() };
        let out = normalize(&input("https://h/20261020000418?SNS=1", Some(&raw), None), &spec);
        assert_eq!(out.canonical_url.as_deref(), Some("https://h/20261020000418?page=2"));
        assert_eq!(out.id, "src:20261020000418");
        let amp = normalize(
            &input("https://h/amp/view/AKR20261013029300003", None, None),
            &NormalizeSpec { id_pattern: Some(r"/view/(AKR\d+)".into()), ..Default::default() },
        );
        assert_eq!(amp.id, "src:AKR20261013029300003");
        assert_eq!(
            normalize(&input("https://h/x?utm_source=a", None, None), &NormalizeSpec::default()).id,
            "src:https://h/x"
        );
    }

    #[test]
    fn text_is_cleaned_of_tags_entities_and_emails() {
        assert_eq!(clean_text("<p>a</p><p>b &amp; c</p> kim@example.com [email&#160;protected]"), "a b & c");
    }
}
