use scraper::{Html, Selector};

pub struct Page {
    pub title: Option<String>,
    pub content: String,
    pub hrefs: Vec<String>,
}

impl Page {
    pub fn new(html: String) -> Self {
        Self {
            title: Self::parse_title(&html),
            content: Self::parse_content(&html),
            hrefs: Self::parse_links(&html),
        }
    }

    fn parse_links(html: &str) -> Vec<String> {
        let document = Html::parse_document(html);
        let selector = Selector::parse("a").unwrap();

        document
            .select(&selector)
            .filter_map(|element| element.value().attr("href").map(|s| s.to_string()))
            .collect()
    }

    fn parse_content(html: &str) -> String {
        let document = Html::parse_document(html);
        let body_selector = Selector::parse("body").unwrap();

        document
            .select(&body_selector)
            .flat_map(|element| element.text())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn parse_title(html: &str) -> Option<String> {
        let document = Html::parse_document(html);
        let selector = Selector::parse("title").unwrap();
        document
            .select(&selector)
            .next()
            .map(|element| element.inner_html())
    }
}

#[cfg(test)]
mod tests {
    use super::Page;

    #[test]
    fn new_parses_title_content_and_links_from_html() {
        let html = r#"
            <!doctype html>
            <html>
                <head><title>Rust Search</title></head>
                <body>
                    <h1>Welcome</h1>
                    <p>Find fast pages.</p>
                    <a href="https://example.com/one">One</a>
                </body>
            </html>
        "#;

        let page = Page::new(html.to_string());

        assert_eq!(page.title.as_deref(), Some("Rust Search"));
        assert!(page.content.contains("Welcome"));
        assert!(page.content.contains("Find fast pages."));
        assert_eq!(page.hrefs, vec!["https://example.com/one"]);
    }

    #[test]
    fn empty_and_malformed_html_do_not_panic() {
        let empty = Page::new(String::new());
        assert_eq!(empty.title, None);
        assert!(empty.content.is_empty());
        assert!(empty.hrefs.is_empty());

        let malformed = Page::new("<html><body><p>Broken<a href=\"/still-found\">link".to_string());
        assert!(malformed.content.contains("Broken"));
        assert!(malformed.hrefs.iter().any(|href| href == "/still-found"));
    }

    #[test]
    fn new_extracts_multiple_anchor_hrefs() {
        let html = r#"
            <html><body>
                <a href="https://example.com/a">A</a>
                <a href="/relative/path">B</a>
                <a href="mailto:test@example.com">Mail</a>
            </body></html>
        "#;

        let page = Page::new(html.to_string());

        assert_eq!(
            page.hrefs,
            vec![
                "https://example.com/a".to_string(),
                "/relative/path".to_string(),
                "mailto:test@example.com".to_string(),
            ]
        );
    }

    #[test]
    fn new_handles_missing_title() {
        let page = Page::new("<html><body>No title here</body></html>".to_string());

        assert_eq!(page.title, None);
        assert_eq!(page.content, "No title here");
    }
}
