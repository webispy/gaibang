//! robots.txt rules as RFC 9309 matches them: the group naming our product token (else `*`), `*` and `$` in
//! patterns, the longest matching rule wins and Allow wins a tie. Crawl-delay is not in the RFC; it is honoured
//! up to MAX_CRAWL_DELAY seconds. Patterns and paths are compared percent-decoded. Anything unreadable errs on the
//! side of not fetching.

use regex::Regex;

pub const MAX_CRAWL_DELAY: f64 = 60.0;

#[derive(Debug, Clone, Default)]
pub struct Robots {
    /// (allow, pattern length, compiled pattern; None when it does not compile)
    rules: Vec<(bool, usize, Option<Regex>)>,
    /// The chosen groups' Crawl-delay, if any (capped).
    pub crawl_delay: Option<f64>,
}

#[derive(Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<(bool, String)>,
    delay: Option<f64>,
}

impl Robots {
    /// Rules for `agent` (a product token such as "dongnyang").
    pub fn parse(text: &str, agent: &str) -> Robots {
        let agent = agent.to_lowercase();
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut groups: Vec<Group> = Vec::new();
        let mut current = Group::default();
        let mut in_rules = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("");
            let Some((key, value)) = line.split_once(':') else { continue };
            let (key, value) = (key.trim().to_lowercase(), value.trim());
            match key.as_str() {
                "user-agent" => {
                    if in_rules {
                        groups.push(std::mem::take(&mut current));
                        in_rules = false;
                    }
                    if !value.is_empty() {
                        current.agents.push(value.to_lowercase());
                    }
                }
                "allow" | "disallow" if !current.agents.is_empty() => {
                    in_rules = true;
                    if !value.is_empty() {
                        current.rules.push((key == "allow", value.to_string()));
                    }
                }
                "crawl-delay" if !current.agents.is_empty() => {
                    in_rules = true;
                    if let Ok(delay) = value.parse::<f64>()
                        && delay.is_finite()
                        && delay >= 0.0
                    {
                        current.delay = Some(delay);
                    }
                }
                _ => {}
            }
        }
        if !current.agents.is_empty() {
            groups.push(current);
        }
        let mine: Vec<&Group> =
            groups.iter().filter(|g| g.agents.iter().any(|a| a != "*" && agent.starts_with(a.as_str()))).collect();
        let chosen =
            if mine.is_empty() { groups.iter().filter(|g| g.agents.iter().any(|a| a == "*")).collect() } else { mine };
        let rules =
            chosen.iter().flat_map(|g| g.rules.iter()).map(|(allow, p)| (*allow, p.len(), compile(p))).collect();
        let crawl_delay = chosen.iter().filter_map(|g| g.delay).reduce(f64::max).map(|d| d.min(MAX_CRAWL_DELAY));
        Robots { rules, crawl_delay }
    }

    /// Everything allowed (no robots.txt: RFC 9309 §2.3.1.3).
    pub fn allow_all() -> Robots {
        Robots::default()
    }

    /// Whether `path_and_query` ("/a/b?x=1") may be fetched.
    pub fn allows(&self, path_and_query: &str) -> bool {
        let path = if path_and_query.is_empty() { "/" } else { path_and_query };
        if path == "/robots.txt" {
            return true;
        }
        let path = percent_decode(path);
        let mut best: Option<(usize, bool)> = None;
        for (allow, length, regex) in &self.rules {
            // A pattern that does not compile counts as matching a Disallow and never as an Allow.
            let hit = regex.as_ref().map(|r| r.is_match(&path)).unwrap_or(!*allow);
            if hit {
                let candidate = (*length, *allow);
                if best.is_none_or(|b| candidate > b) {
                    best = Some(candidate);
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

fn compile(pattern: &str) -> Option<Regex> {
    let anchored = pattern.ends_with('$');
    let body = percent_decode(pattern.trim_end_matches('$'));
    let regex = body.split('*').map(regex::escape).collect::<Vec<_>>().join(".*");
    Regex::new(&format!("^{regex}{}", if anchored { "$" } else { "" })).ok()
}

/// %XX sequences as the characters they stand for (RFC 9309 §2.2.2 compares paths this way).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = ((bytes[i + 1] as char).to_digit(16), (bytes[i + 2] as char).to_digit(16))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "User-agent: GPTBot\nDisallow: /\n\nUser-agent: *\nDisallow: /print/\nAllow: /print/ok$\n\
                        Disallow: /*.pdf$\nCrawl-delay: 2\n";

    #[test]
    fn star_group_applies_and_named_bots_do_not_leak() {
        let robots = Robots::parse(TEXT, "dongnyang");
        assert!(robots.allows("/article/1"));
        assert!(!robots.allows("/print/1"));
        assert!(robots.allows("/print/ok"));
        assert!(!robots.allows("/a/b.pdf"));
        assert!(robots.allows("/a/b.pdf?x"));
        assert_eq!(robots.crawl_delay, Some(2.0));
    }

    #[test]
    fn our_own_group_wins_over_star() {
        let robots = Robots::parse("User-agent: *\nDisallow: /\nUser-agent: dongnyang\nAllow: /\n", "dongnyang");
        assert!(robots.allows("/anything"));
    }

    #[test]
    fn longest_rule_wins_and_allow_wins_a_tie() {
        let robots = Robots::parse("User-agent: *\nDisallow: /a\nAllow: /a\nDisallow: /ab\n", "x");
        assert!(robots.allows("/a"));
        assert!(!robots.allows("/abc"));
    }

    #[test]
    fn crawl_delay_is_capped_and_robots_txt_is_always_allowed() {
        let robots = Robots::parse("User-agent: *\nDisallow: /\nCrawl-delay: 60\n", "x");
        assert_eq!(robots.crawl_delay, Some(MAX_CRAWL_DELAY));
        assert!(robots.allows("/robots.txt"));
        assert!(!robots.allows("/"));
        assert_eq!(Robots::parse("User-agent: *\nCrawl-delay: inf\n", "x").crawl_delay, None);
    }

    #[test]
    fn empty_disallow_allows_everything() {
        assert!(Robots::parse("User-agent: *\nDisallow:\n", "x").allows("/x"));
        assert!(Robots::allow_all().allows("/x"));
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_first_group() {
        assert!(!Robots::parse("\u{feff}User-agent: *\nDisallow: /\n", "dongnyang").allows("/x"));
    }

    #[test]
    fn percent_encoded_paths_match_their_characters() {
        let robots = Robots::parse("User-agent: *\nDisallow: /검색\nDisallow: /a%2Fb\n", "x");
        assert!(!robots.allows("/%EA%B2%80%EC%83%89?q=1"));
        assert!(!robots.allows("/a/b"));
        assert!(robots.allows("/%EA%B8%B0%EC%82%AC"));
    }

    #[test]
    fn an_empty_user_agent_line_is_not_our_group() {
        let robots = Robots::parse("User-agent:\nAllow: /\n\nUser-agent: *\nDisallow: /\n", "dongnyang");
        assert!(!robots.allows("/x"));
    }
}
