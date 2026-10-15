use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use scraper::{ElementRef, Html, Node};

pub fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

pub fn safe_url(url: &str) -> bool {
    !url.chars().any(char::is_whitespace)
        && reqwest::Url::parse(url)
            .is_ok_and(|u| matches!(u.scheme(), "https" | "http") && u.host_str().is_some())
}

pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn markdown(text: &str) -> String {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let events = Parser::new_ext(text, options).map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let dest_url = if safe_url(&dest_url) {
                dest_url
            } else {
                CowStr::from("")
            };
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            })
        }
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let dest_url = if safe_url(&dest_url) {
                dest_url
            } else {
                CowStr::from("")
            };
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            })
        }
        Event::SoftBreak => Event::HardBreak,
        other => other,
    });
    let mut output = String::new();
    html::push_html(&mut output, events);
    output
}

#[derive(Default)]
struct Rich {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
}

impl Rich {
    fn text(&mut self, text: &str, style: Style) {
        let text = safe_text(text);
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.newline();
            }
            if !part.is_empty() {
                self.spans.push(Span::styled(part.to_owned(), style));
            }
        }
    }
    fn newline(&mut self) {
        self.lines.push(Line::from(std::mem::take(&mut self.spans)));
    }
    fn br(&mut self, paragraph: bool) {
        if !self.spans.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
        if paragraph && self.lines.last().is_some_and(|l| !l.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }
    fn walk(&mut self, element: ElementRef<'_>, mut style: Style) {
        let tag = element.value().name();
        if ["script", "style", "template", "head"].contains(&tag) {
            return;
        }
        match tag {
            "b" | "strong" | "h1" | "h2" | "h3" => style = style.add_modifier(Modifier::BOLD),
            "i" | "em" => style = style.add_modifier(Modifier::ITALIC),
            "s" | "del" => style = style.add_modifier(Modifier::CROSSED_OUT),
            "code" | "pre" => style = style.fg(Color::Yellow),
            "a" => style = style.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED),
            _ => (),
        }
        let block = [
            "p",
            "div",
            "li",
            "h1",
            "h2",
            "h3",
            "pre",
            "blockquote",
            "tr",
        ]
        .contains(&tag);
        if tag == "br" {
            self.newline();
            return;
        }
        if block {
            self.br(false);
        }
        if tag == "li" {
            self.text("• ", style);
        }
        for child in element.children() {
            match child.value() {
                Node::Text(text) => self.text(text, style),
                Node::Element(_) => {
                    if let Some(element) = ElementRef::wrap(child) {
                        self.walk(element, style);
                    }
                }
                _ => (),
            }
        }
        if tag == "a"
            && let Some(url) = element.attr("href").filter(|url| safe_url(url))
        {
            let label = element.text().collect::<String>();
            if label != url {
                self.text(&format!(" ({url})"), style);
            }
        }
        if block {
            self.br(["p", "pre", "blockquote"].contains(&tag));
        }
    }
}

pub fn rich(body: &str) -> Vec<Line<'static>> {
    let html = Html::parse_fragment(body);
    let mut rich = Rich::default();
    rich.walk(html.root_element(), Style::default());
    rich.br(false);
    while rich.lines.last().is_some_and(|l| l.spans.is_empty()) {
        rich.lines.pop();
    }
    rich.lines
}

pub fn plain(body: &str) -> String {
    rich(body)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn message_body(text: &str) -> String {
    // API bodies may be HTML or plain text. Plain text must not be interpreted as tags.
    let looks_html = regex::Regex::new(r"</?[A-Za-z][^>]*>")
        .unwrap()
        .is_match(text);
    if looks_html {
        text.to_owned()
    } else {
        escape(text).replace('\n', "<br>")
    }
}

pub fn preview(text: &str, limit: usize) -> String {
    let text = safe_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    format!(
        "{}…",
        text.chars()
            .take(limit.saturating_sub(1))
            .collect::<String>()
            .trim_end()
    )
}

pub fn relative(at: i64, now: i64) -> String {
    if at <= 0 {
        return String::new();
    }
    match (now - at).max(0) {
        n if n < 45 => "just now".into(),
        n if n < 3600 => format!("{}m", ((n + 30) / 60).max(1)),
        n if n < 86400 => format!("{}h", (n + 1800) / 3600),
        n => format!("{}d", (n + 43200) / 86400),
    }
}

pub fn links(body: &str) -> Vec<String> {
    let html = Html::parse_fragment(body);
    let selector = scraper::Selector::parse("a[href]").unwrap();
    let mut urls: Vec<String> = html
        .select(&selector)
        .filter_map(|el| el.attr("href"))
        .filter(|url| safe_url(url))
        .map(str::to_owned)
        .collect();
    let plain = plain(body);
    let regex = regex::Regex::new(r#"https?://[^\s<>\"']+"#).unwrap();
    urls.extend(
        regex
            .find_iter(&plain)
            .map(|m| m.as_str().trim_end_matches(['.', ',', ')', ']']).to_owned())
            .filter(|u| safe_url(u)),
    );
    urls.sort();
    urls.dedup();
    urls
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats_email_html_and_links() {
        let text = plain(
            "<div>Hello <b>Ada</b></div><p>Next &amp; last<br><a href='https://example.com'>link</a></p>",
        );
        assert!(text.contains("Hello Ada\nNext & last\nlink (https://example.com)"));
        assert!(!plain("<script>bad()</script><p>fine</p>").contains("bad"));
        assert!(
            rich("<b>bold</b>")[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }
    #[test]
    fn markdown_is_safe_and_keeps_code() {
        let body = markdown("**bold**\n`x < y`\n<script>oops</script>\n[bad](javascript:alert)");
        assert!(body.contains("<strong>bold</strong>"));
        assert!(body.contains("<code>x &lt; y</code>"));
        assert!(!body.contains("<script>"));
        assert!(!body.contains("href=\"javascript:"));
        assert!(!safe_url("https://a\n--bad"));
    }
    #[test]
    fn unicode_previews_do_not_split_characters() {
        assert_eq!(preview("日本語テスト", 4), "日本語…");
        assert!(!safe_text("hi\x1b\x07").contains('\x1b'));
        assert_eq!(plain("one<br><br>two"), "one\n\ntwo");
        assert_eq!(plain(&message_body("one\n\ntwo")), "one\n\ntwo");
    }
}
