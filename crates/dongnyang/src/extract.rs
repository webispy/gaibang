//! What a page or a feed item says about itself, as the outlet wrote it (the `raw` record): every <meta> tag,
//! every JSON-LD object, the canonical link and <title> of a page; every direct child of an RSS <item>, with
//! its namespace prefix and attributes. Values are unescaped text; nothing is interpreted here.

use std::collections::BTreeMap;

use quick_xml::events::Event;
use scraper::{Html, Selector};
use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RawPage {
    /// `property`, `name` or `itemprop` (lowercased) to every `content` given for it, in page order.
    pub meta: BTreeMap<String, Vec<String>>,
    /// JSON-LD objects, `@graph` and arrays flattened.
    pub jsonld: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// JSON-LD blocks that did not parse.
    #[serde(skip_serializing_if = "is_zero")]
    pub jsonld_errors: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn flatten(value: Value, out: &mut Vec<Value>) {
    match value {
        Value::Array(items) => items.into_iter().for_each(|v| flatten(v, out)),
        Value::Object(mut object) => match object.remove("@graph") {
            Some(graph) => flatten(graph, out),
            None => out.push(Value::Object(object)),
        },
        _ => {}
    }
}

pub fn page(document: &Html) -> RawPage {
    let mut raw = RawPage::default();
    for element in document.select(&Selector::parse("meta[content]").unwrap()) {
        let attrs = element.value();
        let Some(key) = attrs.attr("property").or_else(|| attrs.attr("name")).or_else(|| attrs.attr("itemprop")) else {
            continue;
        };
        let value = attrs.attr("content").unwrap_or("").trim().to_string();
        raw.meta.entry(key.trim().to_lowercase()).or_default().push(value);
    }
    for script in document.select(&Selector::parse(r#"script[type="application/ld+json"]"#).unwrap()) {
        let text: String = script.text().collect();
        // Korean outlets often put raw line breaks inside strings, which strict JSON refuses: retry without them.
        let parsed = serde_json::from_str::<Value>(text.trim())
            .or_else(|_| serde_json::from_str::<Value>(&text.trim().replace(|c: char| c.is_control(), " ")));
        match parsed {
            Ok(value) => flatten(value, &mut raw.jsonld),
            Err(_) => raw.jsonld_errors += 1,
        }
    }
    raw.canonical = document
        .select(&Selector::parse(r#"link[rel="canonical"][href]"#).unwrap())
        .next()
        .and_then(|l| l.value().attr("href"))
        .map(|h| h.trim().to_string());
    raw.title = document
        .select(&Selector::parse("title").unwrap())
        .next()
        .map(|t| t.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "));
    raw
}

/// One RSS item's direct children: qualified name ("title", "dc:creator", "media:content") to its values; a
/// value is the text, or an object of `@attribute`s (plus `#text` when there is text).
pub type RawItem = Map<String, Value>;

/// Items of an RSS 2.0 document. A DTD is refused (no entity expansion).
pub fn feed_items(text: &str) -> Result<Vec<RawItem>, String> {
    // A DTD can only come before the root element; text after it may mention "<!ENTITY" harmlessly.
    let root = text.find("<rss").or_else(|| text.find("<rdf")).or_else(|| text.find("<feed")).unwrap_or(text.len());
    let prolog = text[..text.floor_char_boundary(root)].to_uppercase();
    if prolog.contains("<!DOCTYPE") || prolog.contains("<!ENTITY") {
        return Err("feed declares a DTD".into());
    }
    let mut reader = quick_xml::Reader::from_str(text);
    let mut items = vec![];
    let (mut item, mut depth, mut seen_root): (Option<RawItem>, usize, bool) = (None, 0, false);
    // The child being read: its name, attributes and text so far.
    let mut child: Option<(String, Map<String, Value>, String)> = None;
    let attributes = |e: &quick_xml::events::BytesStart| -> Map<String, Value> {
        e.attributes()
            .flatten()
            .map(|a| {
                let key = format!("@{}", a.key.as_ref());
                let value =
                    a.normalized_value(quick_xml::XmlVersion::default()).map(|v| v.to_string()).unwrap_or_default();
                (key, json!(value))
            })
            .collect()
    };
    let push = |item: &mut RawItem, name: String, attrs: Map<String, Value>, text: String| {
        let text = text.trim().to_string();
        let value = if attrs.is_empty() {
            json!(text)
        } else {
            let mut object = attrs;
            if !text.is_empty() {
                object.insert("#text".into(), json!(text));
            }
            Value::Object(object)
        };
        match item.entry(name).or_insert_with(|| json!([])) {
            Value::Array(values) => values.push(value),
            _ => unreachable!(),
        }
    };
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                seen_root = true;
                depth += 1;
                let name = e.name().as_ref().to_string();
                if item.is_none() && name == "item" {
                    item = Some(Map::new());
                    depth = 1;
                } else if item.is_some() && depth == 2 {
                    child = Some((name, attributes(&e), String::new()));
                }
            }
            Ok(Event::Empty(e)) => {
                seen_root = true;
                if let Some(item) = item.as_mut()
                    && depth == 1
                {
                    push(item, e.name().as_ref().to_string(), attributes(&e), String::new());
                }
            }
            Ok(Event::Text(t)) => {
                if let Some((_, _, text)) = child.as_mut().filter(|_| depth == 2) {
                    text.push_str(t.as_ref());
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if let Some((_, _, text)) = child.as_mut().filter(|_| depth == 2) {
                    match r.resolve_char_ref() {
                        Ok(Some(c)) => text.push(c),
                        _ => {
                            let entity = format!("&{};", r.as_ref());
                            text.push_str(&quick_xml::escape::unescape(&entity).unwrap_or(entity.clone().into()));
                        }
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if let Some((_, _, text)) = child.as_mut().filter(|_| depth == 2) {
                    text.push_str(&t.into_inner());
                }
            }
            Ok(Event::End(_)) => {
                if item.is_some() {
                    if depth == 2
                        && let (Some(item), Some((name, attrs, text))) = (item.as_mut(), child.take())
                    {
                        push(item, name, attrs, text);
                    }
                    if depth == 1 {
                        items.push(item.take().unwrap());
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("not XML: {e}")),
            _ => {}
        }
    }
    if !seen_root {
        return Err("not XML".into());
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_meta_jsonld_canonical_and_title() {
        let html = r#"<html><head><title> T  | 언론 </title><link rel="canonical" href="https://h/a/1">
            <meta property="og:title" content="제목"><meta name="Author" content="가">
            <meta property="article:section" content="경제"><meta property="article:section" content="증권">
            <script type="application/ld+json">{"@graph":[{"@type":"NewsArticle","headline":"H"},{"@type":"BreadcrumbList"}]}</script>
            <script type="application/ld+json">[{"@type":"Organization"}]</script>
            <script type="application/ld+json">{broken</script></head></html>"#;
        let raw = page(&Html::parse_document(html));
        assert_eq!(raw.meta["article:section"], vec!["경제", "증권"]);
        assert_eq!(raw.meta["author"], vec!["가"]);
        assert_eq!(raw.jsonld.len(), 3);
        assert_eq!(raw.jsonld[0]["headline"], "H");
        assert_eq!(
            (raw.canonical.as_deref(), raw.title.as_deref(), raw.jsonld_errors),
            (Some("https://h/a/1"), Some("T | 언론"), 1)
        );
    }

    #[test]
    fn feed_items_keep_direct_children_with_prefixes_and_attributes() {
        let xml = r#"<rss><channel><title>채널</title><item><title><![CDATA[ 제목 &amp; ]]></title>
            <link>https://h/1</link><dc:creator>가</dc:creator><category>경제</category><category>증권</category>
            <media:content url="https://h/1.jpg" type="image/jpeg"/><media:group><media:title>안쪽</media:title></media:group>
            <description>a &amp; b</description></item><item><title>둘</title></item></channel></rss>"#;
        let items = feed_items(xml).unwrap();
        assert_eq!(items.len(), 2);
        let item = &items[0];
        assert_eq!(item["title"], json!(["제목 &amp;"])); // CDATA is taken as written
        assert_eq!(item["description"], json!(["a & b"]));
        assert_eq!(item["dc:creator"], json!(["가"]));
        assert_eq!(item["category"], json!(["경제", "증권"]));
        assert_eq!(item["media:content"][0]["@url"], "https://h/1.jpg");
        assert!(!item.contains_key("media:title")); // not a direct child
        assert!(!items[1].contains_key("link"));
    }

    #[test]
    fn korean_text_across_the_first_kilobyte_does_not_panic() {
        let xml = format!(
            "<rss><channel><title>{}</title><item><title>제목</title></item></channel></rss>",
            "가".repeat(500)
        );
        assert_eq!(feed_items(&xml).unwrap().len(), 1);
    }

    #[test]
    fn jsonld_with_raw_line_breaks_in_strings_is_still_read() {
        let html =
            "<script type=\"application/ld+json\">{\"@type\":\"NewsArticle\",\"headline\":\"첫 줄\n둘째 줄\"}</script>";
        let raw = page(&Html::parse_document(html));
        assert_eq!((raw.jsonld.len(), raw.jsonld_errors), (1, 0));
    }

    #[test]
    fn entity_text_after_the_root_is_not_a_dtd() {
        let xml = "<rss><channel><item><title>&lt;!ENTITY 설명</title><link>https://h/1</link></item></channel></rss>";
        assert_eq!(feed_items(xml).unwrap().len(), 1);
    }

    #[test]
    fn a_dtd_or_no_xml_is_refused() {
        assert!(feed_items("<!DOCTYPE rss [<!ENTITY x 'y'>]><rss/>").is_err());
        assert!(feed_items("").is_err());
    }
}
