//! Article bodies, sitemaps and URL parts, from text. No network here.

use quick_xml::events::Event;
use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;

pub const MAX_BODY_CHARS: usize = 50_000;
/// What a page read gives changed: stored pages are told apart by it. Starts at 2 (1 was an earlier collector).
pub const PARSER_VERSION: u32 = 2;

/// (host, path with query) of an http(s) URL; the fragment is dropped.
pub fn host_and_path(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = path.split('#').next().unwrap_or("/");
    let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
    (!host.is_empty()).then(|| (host.to_lowercase(), path))
}

pub fn cut(text: String, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => text[..i].to_string(),
        None => text,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SitemapEntry {
    pub url: String,
    pub lastmod: Option<String>,
}

/// A sitemap (`<urlset>`: pages) or a sitemap index (`<sitemapindex>`: more sitemaps).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sitemap {
    pub index: bool,
    pub entries: Vec<SitemapEntry>,
}

/// <loc> (plain or CDATA) and <lastmod> of each entry, in order, each address once (the first lastmod wins).
pub fn sitemap(text: &str) -> Result<Sitemap, String> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut result = Sitemap::default();
    let mut seen = std::collections::HashSet::new();
    let (mut field, mut loc, mut lastmod) = (None::<bool>, String::new(), String::new());
    let mut root = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                match e.local_name().as_ref() {
                    "sitemapindex" => result.index = true,
                    "loc" => field = Some(true),
                    "lastmod" => field = Some(false),
                    "url" | "sitemap" => (loc, lastmod) = (String::new(), String::new()),
                    _ => field = None,
                }
                root = true;
            }
            Ok(Event::Text(t)) => match field {
                Some(true) => loc.push_str(t.as_ref()),
                Some(false) => lastmod.push_str(t.as_ref()),
                None => {}
            },
            Ok(Event::GeneralRef(r)) if field == Some(true) => {
                let entity = format!("&{};", r.as_ref());
                loc.push_str(&quick_xml::escape::unescape(&entity).unwrap_or(entity.clone().into()));
            }
            Ok(Event::CData(t)) => match field {
                Some(true) => loc.push_str(&t.into_inner()),
                Some(false) => lastmod.push_str(&t.into_inner()),
                None => {}
            },
            Ok(Event::End(e)) => {
                field = None;
                if matches!(e.local_name().as_ref(), "url" | "sitemap") {
                    let url = loc.trim().to_string();
                    if !url.is_empty() && seen.insert(url.clone()) {
                        let lastmod = Some(lastmod.trim().to_string()).filter(|m| !m.is_empty());
                        result.entries.push(SitemapEntry { url, lastmod });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("not XML: {e}")),
            _ => {}
        }
    }
    if !root {
        return Err("not XML".into());
    }
    Ok(result)
}

const BLOCKS: &[&str] =
    &["p", "div", "li", "h1", "h2", "h3", "h4", "h5", "blockquote", "section", "article", "header", "footer", "tr"];

/// The text of the first element matching a `body` selector (selectors tried in order), without `drop`
/// elements, as paragraphs separated by a blank line. Walks with an explicit stack: no recursion limit.
pub fn body(document: &Html, body: &[String], drop: &[String]) -> Option<String> {
    let drop: Vec<Selector> = drop.iter().filter_map(|s| Selector::parse(s).ok()).collect();
    let root = body.iter().filter_map(|s| Selector::parse(s).ok()).find_map(|s| document.select(&s).next())?;
    enum Step<'a> {
        Enter(ego_tree::NodeRef<'a, Node>),
        Close,
    }
    let mut out = String::new();
    let mut stack: Vec<Step> = root.children().rev().map(Step::Enter).collect();
    while let Some(step) = stack.pop() {
        let node = match step {
            Step::Close => {
                out.push('\n');
                continue;
            }
            Step::Enter(node) => node,
        };
        match node.value() {
            Node::Text(text) => out.push_str(text),
            Node::Element(e) => {
                let Some(element) = ElementRef::wrap(node) else { continue };
                if drop.iter().any(|s| s.matches(&element)) {
                    continue;
                }
                if e.name() == "br" {
                    out.push('\n');
                    continue;
                }
                let block = BLOCKS.contains(&e.name());
                if block {
                    out.push('\n');
                    stack.push(Step::Close);
                }
                stack.extend(node.children().rev().map(Step::Enter));
            }
            _ => {}
        }
    }
    let paragraphs: Vec<String> =
        out.split('\n').map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|l| !l.is_empty()).collect();
    Some(cut(paragraphs.join("\n\n"), MAX_BODY_CHARS)).filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sitemaps_keep_order_drop_repeats_and_read_cdata() {
        let raw = "<urlset><url><loc> https://h/a?x=1&amp;y=2 </loc><lastmod>2025-01-01</lastmod></url>\
            <url><loc><![CDATA[https://h/b]]></loc><lastmod><![CDATA[2026-03-13T15:41:14+09:00]]></lastmod></url>\
            <url><loc>https://h/b</loc><lastmod>2027-01-01</lastmod></url></urlset>";
        let map = sitemap(raw).unwrap();
        assert!(!map.index);
        let urls: Vec<_> = map.entries.iter().map(|e| (e.url.as_str(), e.lastmod.as_deref())).collect();
        assert_eq!(
            urls,
            vec![("https://h/a?x=1&y=2", Some("2025-01-01")), ("https://h/b", Some("2026-03-13T15:41:14+09:00"))]
        );
    }

    #[test]
    fn a_sitemap_index_lists_sitemaps() {
        let map = sitemap("<sitemapindex><sitemap><loc><![CDATA[https://h/s-1.xml]]></loc></sitemap></sitemapindex>")
            .unwrap();
        assert!(map.index);
        assert_eq!(map.entries[0].url, "https://h/s-1.xml");
        assert!(sitemap("").is_err());
    }

    #[test]
    fn host_and_path_split() {
        assert_eq!(host_and_path("https://WWW.H.com/a/b?c#d"), Some(("www.h.com".into(), "/a/b?c".into())));
        assert_eq!(host_and_path("https://h.com"), Some(("h.com".into(), "/".into())));
        assert_eq!(host_and_path("ftp://h.com/x"), None);
    }

    #[test]
    fn body_paragraphs_without_dropped_parts() {
        let html = Html::parse_document(
            "<div class='b'>첫째<br>둘째<script>x</script><figure>사진</figure><p>셋째</p><ul><li>넷째</li></ul></div>",
        );
        let text = body(&html, &[".missing".into(), ".b".into()], &["script".into(), "figure".into()]);
        assert_eq!(text.as_deref(), Some("첫째\n\n둘째\n\n셋째\n\n넷째"));
    }

    #[test]
    fn deeply_nested_markup_does_not_crash() {
        let html = format!("<div class='b'>{}글{}</div>", "<span>".repeat(100_000), "</span>".repeat(100_000));
        let document = Html::parse_document(&html);
        assert_eq!(body(&document, &[".b".into()], &[]).as_deref(), Some("글"));
    }
}
