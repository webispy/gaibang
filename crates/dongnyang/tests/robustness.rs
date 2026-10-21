//! Network input of any shape must not crash a run (a panic loses the rest of the batch): every prefix of
//! Korean markup, and bytes that are not text, go through every parser of every plug-in.

use dongnyang::source::Registry;
use scraper::Html;

const FEED: &str = "<?xml version=\"1.0\"?><rss><channel><title>채널 제목입니다</title><item><title><![CDATA[삼성전자, \
    3분기 실적]]></title><link>https://www.yna.co.kr/view/AKR20261020173400504</link><pubDate>Mon,19 Oct 2026 22:23:21 \
    +0900</pubDate><dc:creator>홍길동</dc:creator><media:content url=\"https://img/1.jpg\"/><description>&lt;p&gt;첫 문단 \
    &amp; 둘째&lt;/p&gt;</description></item></channel></rss>";
const PAGE: &str = "<html><head><meta charset=\"utf-8\"><meta property=\"og:title\" content=\"제목 | 연합뉴스\">\
    <script type=\"application/ld+json\">{\"@type\":\"NewsArticle\",\"articleSection\":[\"경제\",\"증권\"],\
    \"author\":[{\"name\":\"홍길동 기자\"}]}</script></head><body><div class=\"story-news\" itemprop=\"articleBody\">\
    <p>본문 첫째 문단입니다.</p><p>홍길동 기자</p></div></body></html>";
const SITEMAP: &str = "<urlset><url><loc><![CDATA[https://www.fnnews.com/news/202501081415408250]]></loc>\
    <lastmod>2025-01-08</lastmod></url></urlset>";

#[test]
fn every_prefix_of_korean_markup_parses_without_panicking() {
    let registry = Registry::load(None).unwrap();
    for id in registry.ids() {
        let plugin = registry.get(id).unwrap();
        for text in [FEED, PAGE, SITEMAP] {
            let ends: Vec<usize> = (0..=text.len()).filter(|i| text.is_char_boundary(*i)).collect();
            for &end in &ends {
                let part = &text[..end];
                let _ = plugin.feed_items(part);
                let _ = plugin.sitemap(part);
                let document = Html::parse_document(part);
                let _ = plugin.page(&document);
                let _ = plugin.body(&document);
            }
        }
    }
}

#[test]
fn bytes_that_are_not_text_decode_and_parse() {
    let junk: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let decoded = dongnyang::decode::decode(&junk, Some("text/html; charset=euc-kr"));
    let registry = Registry::load(None).unwrap();
    let plugin = registry.get("yna").unwrap();
    let _ = plugin.feed_items(&decoded.text);
    let _ = plugin.body(&Html::parse_document(&decoded.text));
}
