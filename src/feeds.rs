//! News (the website's RSS feed) and patch notes (the account site's public pages).

use anyhow::{Context, Result, anyhow};
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;

pub const NEWS_URL: &str = "https://monstersandmemories.com/updates?format=rss";
pub const PATCH_NOTES_URL: &str = "https://account.monstersandmemories.com/community/patch_notes";

pub fn patch_note_url(id: u32) -> String {
    format!("{PATCH_NOTES_URL}/{id}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsItem {
    pub title: String,
    /// Display form of the RSS `pubDate`, e.g. `29 Sep 2026`.
    pub date: String,
    pub link: String,
}

pub fn parse_news(xml: &str) -> Result<Vec<NewsItem>> {
    #[derive(Deserialize)]
    struct Rss {
        channel: Channel,
    }
    #[derive(Deserialize)]
    struct Channel {
        #[serde(default)]
        item: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        title: Option<String>,
        link: Option<String>,
        #[serde(rename = "pubDate")]
        pub_date: Option<String>,
    }

    let rss: Rss = quick_xml::de::from_str(xml).context("news feed is not valid RSS")?;
    Ok(rss
        .channel
        .item
        .into_iter()
        .filter_map(|item| {
            Some(NewsItem {
                title: item.title?.trim().to_string(),
                date: item
                    .pub_date
                    .as_deref()
                    .map(short_rfc2822_date)
                    .unwrap_or_default(),
                link: item.link?.trim().to_string(),
            })
        })
        .collect())
}

/// `Tue, 29 Sep 2026 16:08:22 +0000` -> `29 Sep 2026`; anything else is returned unchanged.
fn short_rfc2822_date(date: &str) -> String {
    let parts: Vec<&str> = date.split_whitespace().collect();
    match parts.as_slice() {
        [_weekday, day, month, year, ..] => format!("{day} {month} {year}"),
        _ => date.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchNoteSummary {
    pub id: u32,
    /// Full title as published, e.g. `0.30.2.4 - Module 1 Patch 7`.
    pub title: String,
    /// Date as published, e.g. `October 04, 2026`.
    pub date: String,
}

impl PatchNoteSummary {
    /// The title without its build number (`Module 1 Patch 7`).
    pub fn name(&self) -> &str {
        split_version(&self.title).1
    }

    /// `2026-10-04`, or the published date unchanged if it cannot be read.
    pub fn short_date(&self) -> String {
        iso_date(&self.date).unwrap_or_else(|| self.date.clone())
    }

    /// The patch notes list row: `2026-10-04 - Module 1 Patch 7`.
    pub fn list_label(&self) -> String {
        format!("{} - {}", self.short_date(), self.name())
    }
}

/// Splits `0.30.2.4 - Module 1 Patch 7` into the build number and the rest. Only a prefix made of
/// digits and dots counts as a build number, so other titles are returned whole.
fn split_version(title: &str) -> (Option<&str>, &str) {
    match title.split_once(" - ") {
        Some((head, rest))
            if !head.is_empty()
                && head.contains('.')
                && head.chars().all(|c| c.is_ascii_digit() || c == '.') =>
        {
            (Some(head), rest.trim())
        }
        _ => (None, title),
    }
}

/// `October 04, 2026` -> `2026-10-04`.
fn iso_date(date: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let mut parts = date.split_whitespace();
    let month_name = parts.next()?;
    let month = MONTHS.iter().position(|m| *m == month_name)? + 1;
    let day: u32 = parts.next()?.strip_suffix(',')?.parse().ok()?;
    let year: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=31).contains(&day) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchNote {
    pub title: String,
    pub date: String,
    pub sections: Vec<PatchSection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchSection {
    pub heading: String,
    pub items: Vec<PatchItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchItem {
    /// Nesting level from the list's `data-depth` attribute; 0 is top level.
    pub depth: usize,
    pub text: String,
}

fn selector(css: &'static str) -> Selector {
    Selector::parse(css).unwrap_or_else(|e| panic!("invalid built-in selector {css:?}: {e}"))
}

/// Whitespace-collapsed text content of an element.
fn text_of(element: ElementRef<'_>) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_patch_note_index(html: &str) -> Result<Vec<PatchNoteSummary>> {
    let doc = Html::parse_document(html);
    let entry = selector(r#"a[id^="patch-note-"]"#);
    let title = selector("h2");
    let date = selector("p");
    let notes: Vec<PatchNoteSummary> = doc
        .select(&entry)
        .filter_map(|a| {
            let id = a.value().id()?.strip_prefix("patch-note-")?.parse().ok()?;
            Some(PatchNoteSummary {
                id,
                title: a.select(&title).next().map(text_of)?,
                date: a.select(&date).next().map(text_of).unwrap_or_default(),
            })
        })
        .collect();
    if notes.is_empty() {
        return Err(anyhow!(
            "no patch notes found on the page; its layout may have changed"
        ));
    }
    Ok(notes)
}

pub fn parse_patch_note(html: &str) -> Result<PatchNote> {
    let doc = Html::parse_document(html);
    let content = doc
        .select(&selector("#patch-note-content"))
        .next()
        .context("patch note not found on the page")?;
    let title = content
        .select(&selector("h1"))
        .next()
        .map(text_of)
        .unwrap_or_default();
    let date = content
        .select(&selector("p"))
        .next()
        .map(text_of)
        .unwrap_or_default();

    let toggle = selector("button[aria-controls]");
    let heading = selector("h2");
    let list_item = selector("li");
    let sections = content
        .select(&toggle)
        .filter_map(|button| {
            let body_id = button.value().attr("aria-controls")?;
            let body = content
                .select(&selector("div[id]"))
                .find(|div| div.value().id() == Some(body_id))?;
            let items = body
                .select(&list_item)
                .map(|li| PatchItem {
                    depth: list_depth(li),
                    text: text_of(li),
                })
                .filter(|item| !item.text.is_empty())
                .collect();
            Some(PatchSection {
                heading: button
                    .select(&heading)
                    .next()
                    .map(text_of)
                    .unwrap_or_default(),
                items,
            })
        })
        .collect();
    Ok(PatchNote {
        title,
        date,
        sections,
    })
}

/// Depth of a list item, read from the nearest enclosing `<ul data-depth>`.
fn list_depth(li: ElementRef<'_>) -> usize {
    li.ancestors()
        .filter_map(ElementRef::wrap)
        .find_map(|el| el.value().attr("data-depth"))
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEWS: &str = include_str!("../tests/fixtures/news.rss");
    const INDEX: &str = include_str!("../tests/fixtures/patch_notes_index.html");
    const NOTE_55: &str = include_str!("../tests/fixtures/patch_note_55.html");
    const NOTE_30: &str = include_str!("../tests/fixtures/patch_note_30.html");

    #[test]
    fn parses_news_feed() {
        let items = parse_news(NEWS).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0].title,
            "Necromancer, Ranger, and Shadow Knight Deep Dives and Traits Preview"
        );
        assert_eq!(items[0].date, "29 Sep 2026");
        assert!(
            items[0]
                .link
                .starts_with("https://monstersandmemories.com/updates/")
        );
    }

    #[test]
    fn parses_patch_note_index() {
        let notes = parse_patch_note_index(INDEX).unwrap();
        assert_eq!(notes.len(), 3);
        assert_eq!(
            notes[0],
            PatchNoteSummary {
                id: 55,
                title: "0.30.2.4 - Module 1 Patch 7".into(),
                date: "October 04, 2026".into(),
            }
        );
        assert_eq!(split_version(&notes[0].title).0, Some("0.30.2.4"));
        assert_eq!(notes[0].list_label(), "2026-10-04 - Module 1 Patch 7");
    }

    #[test]
    fn list_labels_keep_titles_without_a_build_number() {
        let note = |title: &str, date: &str| PatchNoteSummary {
            id: 1,
            title: title.into(),
            date: date.into(),
        };
        assert_eq!(
            note(
                "0.30.2.1 - Module 1 Patch 4 - Release Patch!",
                "September 30, 2026"
            )
            .list_label(),
            "2026-09-30 - Module 1 Patch 4 - Release Patch!"
        );
        let plain = note("Hotfix - Login queue", "May 1, 2026");
        assert_eq!(split_version(&plain.title).0, None);
        assert_eq!(plain.list_label(), "2026-05-01 - Hotfix - Login queue");
        assert_eq!(
            note("Notes", "sometime soon").list_label(),
            "sometime soon - Notes"
        );
        assert_eq!(iso_date("Octember 04, 2026"), None);
        assert_eq!(iso_date("October 40, 2026"), None);
    }

    #[test]
    fn parses_patch_note_sections_and_depths() {
        let note = parse_patch_note(NOTE_55).unwrap();
        assert_eq!(note.title, "0.30.2.4 - Module 1 Patch 7");
        assert_eq!(note.date, "October 04, 2026");
        assert_eq!(note.sections[0].heading, "Fixes");
        assert_eq!(
            note.sections[0].items[1].text,
            "Fixed a possible memory leak on the client"
        );
        assert_eq!(note.sections[1].heading, "UI");
        let depths: Vec<usize> = note
            .sections
            .iter()
            .flat_map(|s| &s.items)
            .map(|i| i.depth)
            .collect();
        assert!(depths.contains(&2) && depths.contains(&3));
    }

    #[test]
    fn parses_older_patch_note_layout() {
        let note = parse_patch_note(NOTE_30).unwrap();
        assert_eq!(note.sections.len(), 1);
        assert!(note.sections[0].items.iter().all(|i| i.depth >= 1));
    }

    #[test]
    fn missing_patch_note_is_an_error() {
        assert!(parse_patch_note("<html><body><main></main></body></html>").is_err());
    }
}
