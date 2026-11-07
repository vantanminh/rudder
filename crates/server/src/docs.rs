//! The user guides, built into the panel so the running version answers for itself.
//!
//! Agents search them through the panel's own address (the `docs` routes, which are also MCP
//! tools), so what they read always matches the installed version and needs no other host.

use std::sync::OnceLock;

use rust_embed::Embed;

/// Only the top-level guides. Subfolders (stories, decisions, intakes) and the notes that
/// describe how the project itself is developed are not user documentation.
#[derive(Embed)]
#[folder = "../../docs"]
#[include = "*.md"]
#[exclude = "*/*"]
#[exclude = "HARNESS.md"]
#[exclude = "ARCHITECTURE.md"]
#[exclude = "CONTEXT_RULES.md"]
#[exclude = "FEATURE_INTAKE.md"]
#[exclude = "TEST_MATRIX.md"]
struct Guides;

/// One guide.
pub struct Guide {
    /// File name without `.md`, for example `cli`.
    pub name: String,
    pub title: String,
    pub content: String,
}

/// One part of a guide under a heading; the unit that search ranks.
struct Section {
    guide: usize,
    heading: String,
    text: String,
    lower: String,
}

struct Index {
    guides: Vec<Guide>,
    sections: Vec<Section>,
}

/// A ranked match.
pub struct Hit {
    pub name: String,
    pub title: String,
    pub heading: String,
    pub snippet: String,
    pub score: u32,
}

fn index() -> &'static Index {
    static INDEX: OnceLock<Index> = OnceLock::new();
    INDEX.get_or_init(|| {
        build(Guides::iter().filter_map(|path| {
            let file = Guides::get(&path)?;
            let name = path.strip_suffix(".md")?.to_owned();
            let content = String::from_utf8(file.data.into_owned()).ok()?;
            Some((name, content))
        }))
    })
}

fn build(files: impl Iterator<Item = (String, String)>) -> Index {
    let mut guides: Vec<Guide> = files
        .map(|(name, content)| Guide {
            title: title_of(&name, &content),
            name,
            content,
        })
        .collect();
    guides.sort_by(|a, b| a.name.cmp(&b.name));
    let mut sections = Vec::new();
    for (guide_index, guide) in guides.iter().enumerate() {
        let mut heading = guide.title.clone();
        let mut text = String::new();
        let mut in_code = false;
        for line in guide.content.lines() {
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
            }
            let is_heading = !in_code && line.starts_with('#');
            if is_heading {
                push_section(&mut sections, guide_index, &heading, &mut text);
                heading = line.trim_start_matches('#').trim().to_owned();
            } else {
                text.push_str(line);
                text.push('\n');
            }
        }
        push_section(&mut sections, guide_index, &heading, &mut text);
    }
    Index { guides, sections }
}

fn push_section(sections: &mut Vec<Section>, guide: usize, heading: &str, text: &mut String) {
    let body = text.trim();
    if !body.is_empty() {
        sections.push(Section {
            guide,
            heading: heading.to_owned(),
            text: body.to_owned(),
            lower: body.to_lowercase(),
        });
    }
    text.clear();
}

fn title_of(name: &str, content: &str) -> String {
    content
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .map(|title| title.trim().to_owned())
        .unwrap_or_else(|| name.to_owned())
}

/// Every guide, ordered by name.
pub fn list() -> &'static [Guide] {
    &index().guides
}

/// A guide by file name without `.md`.
pub fn get(name: &str) -> Option<&'static Guide> {
    index().guides.iter().find(|guide| guide.name == name)
}

/// Words of the query, lower case, without one-letter noise.
fn terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '/')
        .filter(|word| word.chars().count() >= 2)
    {
        if !out.iter().any(|existing| existing == word) {
            out.push(word.to_owned());
        }
    }
    out.truncate(12);
    out
}

/// Ranks sections by how well they match `query`. Headings and guide titles count more than
/// body text, and a section that contains every word beats one that contains some.
pub fn search(query: &str, limit: usize) -> Vec<Hit> {
    search_in(index(), query, limit)
}

fn search_in(index: &Index, query: &str, limit: usize) -> Vec<Hit> {
    let words = terms(query);
    if words.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<Hit> = Vec::new();
    for section in &index.sections {
        let guide = &index.guides[section.guide];
        let heading = section.heading.to_lowercase();
        let title = guide.title.to_lowercase();
        let name = guide.name.to_lowercase();
        let mut score = 0u32;
        let mut matched = 0usize;
        for word in &words {
            let in_body = section.lower.matches(word.as_str()).count().min(5) as u32;
            let in_heading = heading.contains(word.as_str());
            let in_title = title.contains(word.as_str()) || name == *word;
            if in_body > 0 || in_heading || in_title {
                matched += 1;
            }
            score += in_body + if in_heading { 6 } else { 0 } + if in_title { 3 } else { 0 };
        }
        if matched == 0 {
            continue;
        }
        if matched == words.len() {
            score += 10 * words.len() as u32;
        }
        let phrase = query.trim().to_lowercase();
        if words.len() > 1 && section.lower.contains(&phrase) {
            score += 15;
        }
        hits.push(Hit {
            name: guide.name.clone(),
            title: guide.title.clone(),
            heading: section.heading.clone(),
            snippet: snippet(&section.text, &words),
            score,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.heading.cmp(&b.heading))
    });
    hits.truncate(limit);
    hits
}

/// About 320 characters around the first word that appears in `text`.
fn snippet(text: &str, words: &[String]) -> String {
    const WIDTH: usize = 320;
    let lower = text.to_lowercase();
    let at = words
        .iter()
        .filter_map(|word| lower.find(word.as_str()))
        .min()
        .unwrap_or(0);
    // `lower` can differ in byte length from `text` for a few characters; clamp on boundaries.
    let mut start = at.saturating_sub(WIDTH / 4).min(text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + WIDTH).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let mut out = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if start > 0 {
        out.insert(0, '…');
    }
    if end < text.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        build(
            [
                (
                    "cli".to_owned(),
                    "# Command line client\n\n## Sign in\n\nUse `rudder login --device` for headless hosts.\n\n## Push an image\n\nRun `rudder push` to upload.\n".to_owned(),
                ),
                (
                    "cache".to_owned(),
                    "# HTTP cache\n\n## Purge\n\nPurge a domain to drop cached objects.\n\n```sh\n# not a heading\n```\n".to_owned(),
                ),
            ]
            .into_iter(),
        )
    }

    #[test]
    fn heading_match_ranks_first() {
        let hits = search_in(&sample(), "sign in", 5);
        assert_eq!(hits[0].name, "cli");
        assert_eq!(hits[0].heading, "Sign in");
    }

    #[test]
    fn finds_words_in_the_body() {
        let hits = search_in(&sample(), "purge domain", 5);
        assert_eq!(hits[0].name, "cache");
        assert!(hits[0].snippet.contains("Purge a domain"));
    }

    #[test]
    fn code_comments_are_not_headings() {
        let index = sample();
        assert!(index.sections.iter().all(|s| s.heading != "not a heading"));
    }

    #[test]
    fn empty_or_unmatched_query_returns_nothing() {
        assert!(search_in(&sample(), "   ", 5).is_empty());
        assert!(search_in(&sample(), "zzzzqq", 5).is_empty());
    }

    #[test]
    fn limit_is_respected_and_snippet_is_bounded() {
        let hits = search_in(&sample(), "rudder", 1);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.chars().count() <= 330);
    }

    #[test]
    fn the_built_in_guides_are_present() {
        assert!(get("cli").is_some());
        assert!(get("mcp").is_some());
        assert!(get("HARNESS").is_none());
        assert!(!search("rudder login", 3).is_empty());
    }
}
