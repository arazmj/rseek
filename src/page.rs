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

    fn parse_links(html: &String) -> Vec<String>{
        // Parse the HTML content
        let document = Html::parse_document(html);
        let selector = Selector::parse("a").unwrap();

        document.select(&selector)
            .filter_map(|element| element.value().attr("href").map(|s| s.to_string()))
            .collect()
    }

    fn parse_content(html: &String) -> String {
        let document = Html::parse_document(html);
        let body_selector = Selector::parse("body").unwrap();

        document
            .select(&body_selector)
            .flat_map(|element| element.text())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn parse_title(html: &String) -> Option<String> {
        let document = Html::parse_document(html);
        let selector = Selector::parse("title").unwrap();
        document.select(&selector).next().map(|element| element.inner_html())
    }

}