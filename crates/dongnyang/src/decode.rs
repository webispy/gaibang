//! Bytes to text in the page's own encoding: a byte-order mark, then the Content-Type charset, then the
//! document's declaration (`<meta charset>`, `http-equiv`, or the XML declaration) in its first bytes, else UTF-8.

use encoding_rs::Encoding;
use regex::Regex;

/// Replacement characters above this share of the text mark it as badly decoded.
pub const BAD_SHARE: f64 = 0.01;

#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub text: String,
    pub encoding: &'static str,
    /// Share of U+FFFD in the text: the declared encoding was wrong or the bytes are damaged.
    pub bad_share: f64,
}

impl Decoded {
    pub fn is_bad(&self) -> bool {
        self.bad_share > BAD_SHARE
    }
}

fn charset_in(text: &str) -> Option<String> {
    let re = Regex::new(r#"(?i)(?:charset|encoding)\s*=\s*["']?([A-Za-z0-9_.:-]+)"#).unwrap();
    re.captures(text).map(|c| c[1].to_string())
}

pub fn decode(raw: &[u8], content_type: Option<&str>) -> Decoded {
    let declared = Encoding::for_bom(raw)
        .map(|(e, _)| e)
        .or_else(|| content_type.and_then(charset_in).and_then(|c| Encoding::for_label(c.as_bytes())))
        .or_else(|| {
            let head = String::from_utf8_lossy(&raw[..raw.len().min(4096)]);
            let declaration = Regex::new(r#"(?i)<\?xml[^>]*>|<meta[^>]*(?:charset|content-type)[^>]*>"#).unwrap();
            declaration
                .find(&head)
                .and_then(|m| charset_in(m.as_str()))
                .and_then(|c| Encoding::for_label(c.as_bytes()))
                // A document read as ASCII cannot be UTF-16 (HTML: such a declaration means UTF-8).
                .map(|e| if e == encoding_rs::UTF_16LE || e == encoding_rs::UTF_16BE { encoding_rs::UTF_8 } else { e })
        })
        .unwrap_or(encoding_rs::UTF_8);
    let (text, used, _) = declared.decode(raw);
    let total = text.chars().count().max(1);
    let bad = text.chars().filter(|&c| c == '\u{fffd}').count();
    Decoded { text: text.into_owned(), encoding: used.name(), bad_share: bad as f64 / total as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn euc_kr(text: &str) -> Vec<u8> {
        encoding_rs::EUC_KR.encode(text).0.into_owned()
    }

    #[test]
    fn the_header_charset_is_used() {
        let raw = euc_kr("<p>가상 기업</p>");
        let decoded = decode(&raw, Some("text/html; charset=EUC-KR"));
        assert_eq!((decoded.text.as_str(), decoded.encoding, decoded.is_bad()), ("<p>가상 기업</p>", "EUC-KR", false));
    }

    #[test]
    fn a_meta_or_xml_declaration_is_used_without_a_header() {
        let mut page = b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=euc-kr\">".to_vec();
        page.extend(euc_kr("</head><body>삼성전자</body>"));
        assert!(decode(&page, Some("text/html")).text.contains("삼성전자"));
        let mut feed = b"<?xml version=\"1.0\" encoding=\"EUC-KR\"?><rss>".to_vec();
        feed.extend(euc_kr("<title>코스피</title></rss>"));
        assert!(decode(&feed, None).text.contains("코스피"));
    }

    #[test]
    fn a_utf16_declaration_in_ascii_markup_means_utf8() {
        let page = "<meta charset=\"utf-16\"><p>한글</p>".as_bytes();
        assert_eq!(decode(page, None).encoding, "UTF-8");
    }

    #[test]
    fn utf8_is_the_default_and_a_wrong_guess_is_flagged() {
        assert_eq!(decode("한글".as_bytes(), None).encoding, "UTF-8");
        assert!(decode(&euc_kr("한글 기사 본문입니다"), None).is_bad());
    }
}
