//! Outlets as plug-ins. Most are a TOML file (sources/<id>.toml: feeds, sitemap, article URL pattern, body
//! selectors, field mapping, pace); an outlet whose markup needs code gets a `Plugin` implementation registered
//! in `code_plugin`, built on the same spec. Built-in specs are compiled in; `--sources DIR` adds or replaces them.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{Datelike, NaiveDate};
use regex::Regex;
use scraper::Html;
use serde::Deserialize;

use crate::extract::{self, RawItem, RawPage};
use crate::normalize::{CategorySpec, KeywordSpec, MapSpec, NormalizeSpec};
use crate::parse::{self, Sitemap};

/// The slowest pace any outlet may be read at, whatever its spec or the command line say.
pub const MIN_INTERVAL_FLOOR_SECS: f64 = 3.0;

const BUILT_IN: &[(&str, &str)] = &[
    ("chosunbiz", include_str!("../sources/chosunbiz.toml")),
    ("etnews", include_str!("../sources/etnews.toml")),
    ("etoday", include_str!("../sources/etoday.toml")),
    ("fnnews", include_str!("../sources/fnnews.toml")),
    ("hankyung", include_str!("../sources/hankyung.toml")),
    ("mk", include_str!("../sources/mk.toml")),
    ("yna", include_str!("../sources/yna.toml")),
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub id: String,
    pub outlet: String,
    pub hosts: Vec<String>,
    /// Sources sharing one pace and one cool-down (several outlets behind one CDN); the id by default.
    pub pace_group: Option<String>,
    #[serde(default = "default_interval")]
    pub min_interval_secs: f64,
    #[serde(default)]
    pub feeds: Vec<String>,
    pub article_path: String,
    #[serde(default)]
    pub skip_url_prefixes: Vec<String>,
    pub sitemap: Option<SitemapSpec>,
    pub article: ArticleSpec,
    pub id_pattern: Option<String>,
    #[serde(default)]
    pub title_suffixes: Vec<String>,
    #[serde(default)]
    pub author_ignore: Vec<String>,
    #[serde(default)]
    pub strip_params: Vec<String>,
    #[serde(default)]
    pub map: MapSpec,
    #[serde(default)]
    pub categories: CategorySpec,
    #[serde(default)]
    pub keywords: KeywordSpec,
}

fn default_interval() -> f64 {
    MIN_INTERVAL_FLOOR_SECS
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SitemapSpec {
    /// A URL with `{date}` formatted by `date_format` (chrono strftime): one sitemap, or an index of sitemaps.
    pub url: String,
    pub date_format: String,
    /// What one URL covers.
    #[serde(default)]
    pub period: Period,
    /// Older pages are not served (or answer 403): neither sitemaps nor articles before it are read.
    pub first_day: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    #[default]
    Day,
    Month,
    Year,
}

impl Period {
    /// The first day of the period holding `day`.
    pub fn start(self, day: NaiveDate) -> NaiveDate {
        match self {
            Period::Day => day,
            Period::Month => day.with_day(1).unwrap_or(day),
            Period::Year => NaiveDate::from_ymd_opt(day.year(), 1, 1).unwrap_or(day),
        }
    }

    /// The start of the period before the one starting at `start`.
    pub fn previous(self, start: NaiveDate) -> Option<NaiveDate> {
        Some(self.start(start.pred_opt()?))
    }

    fn last_day(self, start: NaiveDate) -> NaiveDate {
        match self {
            Period::Day => start,
            Period::Month => {
                start.checked_add_months(chrono::Months::new(1)).and_then(|d| d.pred_opt()).unwrap_or(start)
            }
            Period::Year => NaiveDate::from_ymd_opt(start.year(), 12, 31).unwrap_or(start),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArticleSpec {
    /// CSS selectors tried in order; the first matching element is the body.
    pub body: Vec<String>,
    /// CSS selectors of elements inside the body to leave out (scripts, figures, ad slots, related news).
    #[serde(default)]
    pub drop: Vec<String>,
    /// The article id in the URL starts with its date (yyyymmdd), so articles before `sitemap.first_day`
    /// can be skipped without a request.
    #[serde(default)]
    pub id_date_prefix: bool,
}

pub trait Plugin {
    fn spec(&self) -> &Spec;

    fn normalize_spec(&self) -> &NormalizeSpec;

    fn pace_key(&self) -> &str {
        self.spec().pace_group.as_deref().unwrap_or(&self.spec().id)
    }

    fn feed_items(&self, text: &str) -> Result<Vec<RawItem>, String> {
        extract::feed_items(text)
    }

    fn page(&self, document: &Html) -> RawPage {
        extract::page(document)
    }

    fn body(&self, document: &Html) -> Option<String> {
        parse::body(document, &self.spec().article.body, &self.spec().article.drop)
    }

    /// Article entries of a sitemap, or the outlet's own sitemaps of an index.
    fn sitemap(&self, text: &str) -> Result<Sitemap, String> {
        let mut map = parse::sitemap(text)?;
        let own = |url: &str| parse::host_and_path(url).is_some_and(|(h, _)| self.spec().hosts.contains(&h));
        map.entries.retain(|e| if map.index { own(&e.url) } else { self.is_article(&e.url) });
        Ok(map)
    }

    fn is_article(&self, url: &str) -> bool;

    /// Whether the URL's own date says it is older than the outlet serves.
    fn too_old(&self, url: &str) -> bool;

    /// The sitemap URL of the period holding `day`, unless the whole period is older than the outlet serves.
    fn sitemap_url(&self, day: NaiveDate) -> Option<String>;

    fn period(&self) -> Option<Period> {
        self.spec().sitemap.as_ref().map(|s| s.period)
    }
}

pub struct SpecPlugin {
    spec: Spec,
    normalize: NormalizeSpec,
    article_path: Regex,
    first_day: Option<NaiveDate>,
}

impl SpecPlugin {
    pub fn new(spec: Spec) -> Result<SpecPlugin, String> {
        let id = &spec.id;
        let article_path = Regex::new(&spec.article_path).map_err(|e| format!("{id}: article_path: {e}"))?;
        if let Some(pattern) = &spec.id_pattern {
            let re = Regex::new(pattern).map_err(|e| format!("{id}: id_pattern: {e}"))?;
            if re.captures_len() != 2 {
                return Err(format!("{id}: id_pattern needs exactly one group"));
            }
        }
        for selector in spec.article.body.iter().chain(&spec.article.drop) {
            scraper::Selector::parse(selector).map_err(|e| format!("{id}: selector {selector:?}: {e}"))?;
        }
        if spec.hosts.is_empty() {
            return Err(format!("{id}: no hosts"));
        }
        if !spec.min_interval_secs.is_finite() || spec.min_interval_secs < MIN_INTERVAL_FLOOR_SECS {
            return Err(format!("{id}: min_interval_secs must be at least {MIN_INTERVAL_FLOOR_SECS}"));
        }
        let first_day = match &spec.sitemap {
            Some(s) => {
                Some(NaiveDate::parse_from_str(&s.first_day, "%Y-%m-%d").map_err(|e| format!("{id}: first_day: {e}"))?)
            }
            None => None,
        };
        let normalize = NormalizeSpec {
            map: spec.map.clone(),
            categories: spec.categories.clone(),
            keywords: spec.keywords.clone(),
            id_pattern: spec.id_pattern.clone(),
            title_suffixes: spec.title_suffixes.clone(),
            author_ignore: spec.author_ignore.clone(),
            strip_params: spec.strip_params.clone(),
            ..Default::default()
        };
        Ok(SpecPlugin { spec, normalize, article_path, first_day })
    }
}

impl Plugin for SpecPlugin {
    fn spec(&self) -> &Spec {
        &self.spec
    }

    fn normalize_spec(&self) -> &NormalizeSpec {
        &self.normalize
    }

    fn is_article(&self, url: &str) -> bool {
        let Some((host, path)) = parse::host_and_path(url) else { return false };
        let path = path.split('?').next().unwrap_or("");
        self.spec.hosts.iter().any(|h| h == &host)
            && !self.spec.skip_url_prefixes.iter().any(|p| url.starts_with(p.as_str()))
            && self.article_path.is_match(path)
    }

    fn too_old(&self, url: &str) -> bool {
        let (Some(first_day), true) = (self.first_day, self.spec.article.id_date_prefix) else { return false };
        let Some((_, path)) = parse::host_and_path(url) else { return false };
        let id = path.split('?').next().unwrap_or("").rsplit('/').next().unwrap_or("");
        id.get(..8).and_then(|d| NaiveDate::parse_from_str(d, "%Y%m%d").ok()).is_some_and(|d| d < first_day)
    }

    fn sitemap_url(&self, day: NaiveDate) -> Option<String> {
        let (sitemap, first_day) = (self.spec.sitemap.as_ref()?, self.first_day?);
        let start = sitemap.period.start(day);
        (sitemap.period.last_day(start) >= first_day)
            .then(|| sitemap.url.replace("{date}", &start.format(&sitemap.date_format).to_string()))
    }
}

/// Outlets whose markup needs code: wrap the spec in a dedicated Plugin here.
fn code_plugin(spec: Spec) -> Result<Box<dyn Plugin>, String> {
    Ok(Box::new(SpecPlugin::new(spec)?))
}

pub struct Registry {
    plugins: BTreeMap<String, Box<dyn Plugin>>,
}

impl Registry {
    /// Built-in specs, then every `*.toml` in `dir` (same id replaces).
    pub fn load(dir: Option<&Path>) -> Result<Registry, String> {
        let mut texts: Vec<(String, String)> = BUILT_IN.iter().map(|(id, t)| (id.to_string(), t.to_string())).collect();
        if let Some(dir) = dir {
            let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let mut paths: Vec<_> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect();
            paths.sort();
            for path in paths {
                let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                texts.push((path.display().to_string(), text));
            }
        }
        let mut plugins: BTreeMap<String, Box<dyn Plugin>> = BTreeMap::new();
        for (name, text) in texts {
            let spec: Spec = toml::from_str(&text).map_err(|e| format!("{name}: {e}"))?;
            plugins.insert(spec.id.clone(), code_plugin(spec)?);
        }
        // A block is per host (IP): sources reading one host must share one pace and one cool-down.
        let mut keys: BTreeMap<&str, &str> = BTreeMap::new();
        for plugin in plugins.values() {
            for host in &plugin.spec().hosts {
                if let Some(other) = keys.insert(host, plugin.pace_key())
                    && other != plugin.pace_key()
                {
                    return Err(format!(
                        "{host} is read by pace keys {other} and {}: give them one pace_group",
                        plugin.pace_key()
                    ));
                }
            }
        }
        Ok(Registry { plugins })
    }

    pub fn get(&self, id: &str) -> Option<&dyn Plugin> {
        self.plugins.get(id).map(|p| p.as_ref())
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.plugins.keys().map(String::as_str)
    }

    /// The plug-in whose hosts include the URL's host (hosts are not shared between pace keys, see `load`;
    /// the first by id when several sources of one pace group list it).
    pub fn for_url(&self, url: &str) -> Option<&dyn Plugin> {
        let (host, _) = parse::host_and_path(url)?;
        self.plugins.values().find(|p| p.spec().hosts.contains(&host)).map(|p| p.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_hankyung_spec_loads_and_classifies_urls() {
        let registry = Registry::load(None).unwrap();
        let hankyung = registry.get("hankyung").unwrap();
        assert!(hankyung.is_article("https://www.hankyung.com/article/2025010330187"));
        assert!(hankyung.is_article("https://www.hankyung.com/article/2025010330187?utm=x"));
        assert!(!hankyung.is_article("https://www.hankyung.com/tag/x"));
        assert!(!hankyung.is_article("https://evil.example/article/2025010330187"));
        assert!(hankyung.too_old("https://www.hankyung.com/article/202412183989i"));
        assert!(!hankyung.too_old("https://www.hankyung.com/article/2025010330187"));
        let day = NaiveDate::from_ymd_opt(2025, 4, 15).unwrap();
        assert_eq!(hankyung.sitemap_url(day).unwrap(), "https://www.hankyung.com/sitemap/daily-article/20250415.xml");
        assert!(hankyung.sitemap_url(NaiveDate::from_ymd_opt(2024, 12, 31).unwrap()).is_none());
    }

    #[test]
    fn two_sources_on_one_host_must_share_a_pace_group() {
        let dir = std::env::temp_dir().join(format!("dongnyang-specs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = include_str!("../sources/hankyung.toml").replace("id = \"hankyung\"", "id = \"hankyung2\"");
        std::fs::write(dir.join("copy.toml"), &copy).unwrap();
        assert!(Registry::load(Some(&dir)).err().unwrap().contains("www.hankyung.com"));
        std::fs::write(
            dir.join("copy.toml"),
            copy.replace("id = \"hankyung2\"", "id = \"hankyung2\"\npace_group = \"hankyung\""),
        )
        .unwrap();
        assert!(Registry::load(Some(&dir)).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bad_specs_are_refused_with_their_name() {
        let base = include_str!("../sources/hankyung.toml");
        for (from, to, error) in [
            ("article_path = ", "article_path = '(' #", "article_path"),
            ("min_interval_secs = 3.0", "min_interval_secs = 1.0", "min_interval_secs"),
            ("id_pattern = ", "id_pattern = '/x' #", "id_pattern"),
        ] {
            let spec: Spec = toml::from_str(&base.replace(from, to)).unwrap();
            assert!(SpecPlugin::new(spec).err().unwrap().contains(error), "{error}");
        }
    }
}
