//! Each outlet's plug-in against a synthetic page shaped like the outlet's markup as sampled on 2026-10-20
//! (never a copied article): the common fields it must give, and where they come from.

use dongnyang::normalize::{self, Input, Normalized};
use dongnyang::source::{Plugin, Registry};
use scraper::Html;

fn article(registry: &Registry, id: &str, url: &str, html: &str) -> (Normalized, Option<String>) {
    let plugin = registry.get(id).unwrap();
    let document = Html::parse_document(html);
    let page = plugin.page(&document);
    let body = plugin.body(&document).map(|b| normalize::scrub_body(&b));
    let spec = plugin.spec();
    let input =
        Input { source: &spec.id, outlet: &spec.outlet, url, page: Some(&page), item: None, body: body.as_deref() };
    (normalize::normalize(&input, plugin.normalize_spec()), body)
}

fn feed(plugin: &dyn Plugin, xml: &str) -> Vec<Normalized> {
    let spec = plugin.spec();
    plugin
        .feed_items(xml)
        .unwrap()
        .iter()
        .map(|item| {
            let link = item["link"][0].as_str().unwrap().to_string();
            let input =
                Input { source: &spec.id, outlet: &spec.outlet, url: &link, page: None, item: Some(item), body: None };
            normalize::normalize(&input, plugin.normalize_spec())
        })
        .collect()
}

#[test]
fn yna_page() {
    let r = Registry::load(None).unwrap();
    let url = "https://www.yna.co.kr/view/AKR20261020000100001";
    let (n, body) = article(&r, "yna", url, include_str!("fixtures/yna_article.html"));
    assert_eq!(n.id, "yna:AKR20261020000100001");
    assert_eq!(n.title.as_deref(), Some("가상중공업, 입찰 제한 2년"));
    assert_eq!(n.published_at.as_deref(), Some("2026-10-20T11:39:37+00:00"));
    assert_eq!(n.authors, vec!["가나다"]);
    assert_eq!((n.section_path.clone(), n.categories.clone()), (vec!["산업".to_string()], vec!["산업".to_string()]));
    assert_eq!(n.keywords, vec!["가상중공업"]);
    assert_eq!(
        body.as_deref(),
        Some(
            "최고 수준 제재\n\n(서울=연합뉴스) 가나다 기자 = 가상중공업이 20일 입찰 제한 2년을 받았다.\n\n회사는 \"엄정히 받아들인다\"고 밝혔다."
        )
    );
    // the mobile copy is the same article
    let (amp, _) = article(&r, "yna", "https://www.yna.co.kr/amp/view/AKR20261020000100001", "<html></html>");
    assert_eq!(amp.id, n.id);
}

#[test]
fn mk_page_takes_the_korean_section_and_leaves_the_title_out_of_the_breadcrumb() {
    let r = Registry::load(None).unwrap();
    let (n, body) =
        article(&r, "mk", "https://www.mk.co.kr/news/business/12181999", include_str!("fixtures/mk_article.html"));
    assert_eq!((n.id.as_str(), n.title.as_deref()), ("mk:12181999", Some("가상증권, 항소심도 배상")));
    assert_eq!(n.categories, vec!["기업"]);
    assert_eq!(n.authors, vec!["라마바"]);
    assert_eq!(
        body.as_deref(),
        Some("가상증권이 항소심에서도 배상 판결을 받았다.\n\n재판부는 설명 의무를 다하지 않았다고 봤다.")
    );
}

#[test]
fn mk_feed_dates_with_a_colon_offset_and_the_outlet_as_author() {
    let r = Registry::load(None).unwrap();
    let items = feed(
        r.get("mk").unwrap(),
        "<rss><channel><item><no>12181027</no><title>T</title><link>https://www.mk.co.kr/news/stock/12181027</link>\
         <category>증권</category><author>매일경제</author><pubDate>Tue, 20 Oct 2026 20:14:09 +09:00</pubDate></item></channel></rss>",
    );
    assert_eq!(items[0].published_at.as_deref(), Some("2026-10-20T11:14:09+00:00"));
    assert!(items[0].authors.is_empty()); // the outlet is not a reporter
    assert_eq!(items[0].categories, vec!["증권"]);
}

#[test]
fn etnews_page_meta_written_content_first_and_its_site_keywords_are_not_kept() {
    let r = Registry::load(None).unwrap();
    let (n, body) = article(
        &r,
        "etnews",
        "https://www.etnews.com/20261020000999?SNS=00002",
        include_str!("fixtures/etnews_article.html"),
    );
    assert_eq!(n.id, "etnews:20261020000999");
    assert_eq!(n.canonical_url.as_deref(), Some("https://www.etnews.com/20261020000999"));
    assert_eq!(n.published_at.as_deref(), Some("2026-10-20T11:54:37+00:00")); // <meta content=… property=…>
    assert_eq!(n.authors, vec!["사아자"]);
    assert!(n.keywords.is_empty());
    assert_eq!(n.categories, vec!["economy"]);
    assert_eq!(
        body.as_deref(),
        Some("개인 매도세\n\n가상반도체 주가가 20일 급락했다.\n\n증권가는 목표가를 높였다.\n\n사아자 기자")
    );
}

#[test]
fn chosunbiz_page_drops_ad_slots_and_takes_categories_from_the_url() {
    let r = Registry::load(None).unwrap();
    let url = "https://biz.chosun.com/industry/company/2026/10/20/47A2BJJQMREV7IKHBAAWB7NS5E/";
    let (n, body) = article(&r, "chosunbiz", url, include_str!("fixtures/chosunbiz_article.html"));
    assert_eq!(n.id, "chosunbiz:47A2BJJQMREV7IKHBAAWB7NS5E");
    assert_eq!(
        (n.published_at.as_deref(), n.updated_at.as_deref()),
        (Some("2026-10-20T11:32:28+00:00"), Some("2026-10-20T11:32:31+00:00"))
    );
    assert_eq!(n.authors, vec!["차카타"]);
    assert_eq!(n.section_path, vec!["industry", "company"]);
    assert_eq!(body.as_deref(), Some("가상방산이 20일 입찰 제한 2년을 받았다.\n\n방사청은 무관용 원칙을 밝혔다."));
}

#[test]
fn hankyung_page() {
    let r = Registry::load(None).unwrap();
    let (n, body) = article(
        &r,
        "hankyung",
        "https://www.hankyung.com/article/2025103012345",
        include_str!("fixtures/hankyung_article.html"),
    );
    assert_eq!(
        (n.id.as_str(), n.title.as_deref()),
        ("hankyung:2025103012345", Some("가상 기업, 3분기 영업이익 1조2천억원 \"사상 최대\""))
    );
    assert_eq!(n.published_at.as_deref(), Some("2025-10-29T23:15:00+00:00"));
    assert_eq!(
        body.as_deref(),
        Some(
            "가상 기업은 30일 3분기 영업이익이 1조2천억원으로 집계됐다고 밝혔다.\n\n매출은 15조원으로 전년보다 8% 늘었다.\n\n\
             회사 관계자는 \"4분기에도 수요가 이어질 것\"이라고 말했다."
        )
    ); // the same text an earlier collector extracted from this file
}

#[test]
fn fnnews_page_path_byline_and_no_logo_as_image() {
    let r = Registry::load(None).unwrap();
    let (n, body) = article(
        &r,
        "fnnews",
        "https://www.fnnews.com/news/202503040930000001",
        include_str!("fixtures/fnnews_article.html"),
    );
    assert_eq!(n.title.as_deref(), Some("가상전자, 신규 공장 착공 '2027년 가동'"));
    assert_eq!(n.section_path, vec!["경제", "일반경제"]);
    assert_eq!((n.authors.clone(), n.provenance["authors"].as_str()), (vec!["가상".to_string()], "body:byline"));
    assert_eq!(n.image_url, None);
    assert_eq!(
        body.as_deref(),
        Some(
            "투자 규모 2조원\n\n[파이낸셜뉴스] 가상전자는 4일 신규 공장을 착공했다고 밝혔다.\n\n공장은 2027년 가동한다.\n\n가상기자"
        )
    );
    let plugin = r.get("fnnews").unwrap();
    assert!(plugin.too_old("https://www.fnnews.com/news/202412311415408250"));
}

#[test]
fn fnnews_feed_reporter_and_image_come_from_rss() {
    let r = Registry::load(None).unwrap();
    let items = feed(
        r.get("fnnews").unwrap(),
        "<rss><channel><item><title><![CDATA[T]]></title><link>https://www.fnnews.com/news/202610191639470236</link>\
         <category>경제|일반경제</category><author>moon@example.com 사아자</author>\
         <pubDate>Mon,19 Oct 2026 22:23:21 +0900</pubDate><image>https://image.fnnews.com/r/1_l.jpg</image></item></channel></rss>",
    );
    let n = &items[0];
    assert_eq!(n.published_at.as_deref(), Some("2026-10-19T13:23:21+00:00"));
    assert_eq!(n.authors, vec!["사아자"]);
    assert_eq!(n.section_path, vec!["경제", "일반경제"]);
    assert_eq!(n.image_url.as_deref(), Some("https://image.fnnews.com/r/1_l.jpg"));
}

#[test]
fn etoday_page_and_its_feed_time_is_an_edit_time() {
    let r = Registry::load(None).unwrap();
    let (n, body) = article(
        &r,
        "etoday",
        "https://www.etoday.co.kr/news/view/2700001",
        include_str!("fixtures/etoday_article.html"),
    );
    assert_eq!(n.id, "etoday:2700001");
    assert_eq!(n.published_at.as_deref(), Some("2025-11-05T06:00:00+00:00"));
    assert_eq!(n.authors, vec!["타파하"]);
    assert_eq!(n.categories, vec!["경제"]); // the numeric dd:category is not read
    assert_eq!(n.keywords, vec!["가상화학", "흑자전환", "석유화학"]);
    assert_eq!(
        body.as_deref(),
        Some(
            "영업이익 300억원\n\n매출 1조원\n\n가상화학은 5일 3분기 영업이익이 300억원이라고 밝혔다.\n\n전년 같은 기간에는 적자였다."
        )
    );
    let items = feed(
        r.get("etoday").unwrap(),
        "<rss><channel><item><title>T</title><link>https://www.etoday.co.kr/news/view/2637772</link>\
         <category> 정책 </category><pubDate>Tue, 20 Oct 2026 17:46:00 +0900</pubDate><dc:creator>세종=라마바</dc:creator>\
         </item></channel></rss>",
    );
    assert_eq!(
        (items[0].published_at.as_deref(), items[0].updated_at.as_deref()),
        (None, Some("2026-10-20T08:46:00+00:00"))
    );
    assert_eq!(items[0].authors, vec!["라마바"]);
}

#[test]
fn every_built_in_source_loads_with_an_id_pattern_and_a_floor_pace() {
    let r = Registry::load(None).unwrap();
    assert_eq!(r.ids().count(), 7);
    for id in r.ids() {
        let spec = r.get(id).unwrap().spec();
        assert!(!spec.feeds.is_empty() || spec.sitemap.is_some(), "{id}");
        assert!(spec.id_pattern.is_some(), "{id}");
        assert!(spec.min_interval_secs >= dongnyang::source::MIN_INTERVAL_FLOOR_SECS, "{id}");
    }
}

#[test]
fn sitemap_urls_follow_each_outlets_period() {
    use chrono::NaiveDate;
    let r = Registry::load(None).unwrap();
    let day = NaiveDate::from_ymd_opt(2025, 3, 17).unwrap();
    assert_eq!(r.get("etoday").unwrap().sitemap_url(day).unwrap(), "https://www.etoday.co.kr/rss/web_sitemap?q=202503");
    assert_eq!(
        r.get("fnnews").unwrap().sitemap_url(day).unwrap(),
        "https://www.fnnews.com/resource/view_xml/sitemap_index/news-sitemap-index-2025.xml"
    );
    assert!(r.get("etoday").unwrap().sitemap_url(NaiveDate::from_ymd_opt(2024, 12, 31).unwrap()).is_none());
    assert!(r.get("yna").unwrap().sitemap_url(day).is_none());
}

#[test]
fn ids_agree_across_url_forms_for_every_outlet() {
    let r = Registry::load(None).unwrap();
    let forms = [
        (
            "yna",
            "https://www.yna.co.kr/view/AKR20261020000100001",
            "https://www.yna.co.kr/amp/view/AKR20261020000100001?input=1",
        ),
        (
            "hankyung",
            "https://www.hankyung.com/article/2025103012345",
            "https://www.hankyung.com/article/2025103012345?utm_source=x",
        ),
        (
            "mk",
            "https://www.mk.co.kr/news/business/12181999",
            "https://www.mk.co.kr/news/business/12181999?utm_campaign=x",
        ),
        ("etnews", "https://www.etnews.com/20261020000999", "https://www.etnews.com/20261020000999?SNS=00002"),
        (
            "chosunbiz",
            "https://biz.chosun.com/industry/company/2026/10/20/47A2BJJQMREV7IKHBAAWB7NS5E/",
            "https://biz.chosun.com/industry/company/2026/10/20/47A2BJJQMREV7IKHBAAWB7NS5E",
        ),
        (
            "fnnews",
            "https://www.fnnews.com/news/202503040930000001",
            "https://www.fnnews.com/news/202503040930000001?utm_medium=x",
        ),
        ("etoday", "https://www.etoday.co.kr/news/view/2700001", "https://www.etoday.co.kr/news/view/2700001?ref=main"),
    ];
    for (id, a, b) in forms {
        let (x, _) = article(&r, id, a, "<html></html>");
        let (y, _) = article(&r, id, b, "<html></html>");
        assert_eq!(x.id, y.id, "{id}");
        assert!(!x.id.contains("http"), "{id}: {}", x.id);
    }
}

#[test]
fn a_large_sitemap_is_parsed_and_listed_quickly() {
    let r = Registry::load(None).unwrap();
    let plugin = r.get("fnnews").unwrap();
    let xml: String = (0..50_000)
        .map(|i| format!("<url><loc><![CDATA[https://www.fnnews.com/news/2025010814{:08}]]></loc></url>", i))
        .collect::<Vec<_>>()
        .join("");
    let started = std::time::Instant::now();
    let map = plugin.sitemap(&format!("<urlset>{xml}</urlset>")).unwrap();
    let spec = plugin.spec();
    for entry in &map.entries {
        let input =
            Input { source: &spec.id, outlet: &spec.outlet, url: &entry.url, page: None, item: None, body: None };
        normalize::normalize(&input, plugin.normalize_spec());
    }
    assert_eq!(map.entries.len(), 50_000);
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "{:?}", started.elapsed());
}
