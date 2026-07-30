//! Markdown as a graph: files are modules, headings are the nested items,
//! and links are the edges. A link's target is stored verbatim as a call
//! path (`./other.md#section`, `#local`, `[[WikiPage]]`); the resolver's
//! Markdown branch turns it into an edge to a heading — or flags it broken.

use std::collections::HashMap;

use anyhow::Result;

use crate::model::{FileFacts, Item, RawCall, Receiver};

/// GitHub-style heading slug: lowercase, drop punctuation, spaces to hyphens.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.trim().chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if ch == ' ' || ch == '-' || ch == '_' {
            out.push('-');
        }
        // everything else (punctuation, markdown syntax) is dropped
    }
    // collapse runs of hyphens
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in out.chars() {
        if ch == '-' {
            if !prev_dash {
                slug.push('-');
            }
            prev_dash = true;
        } else {
            slug.push(ch);
            prev_dash = false;
        }
    }
    slug.trim_matches('-').to_string()
}

struct Heading {
    level: usize,
    item: Item,
}

pub fn extract(src: &str) -> Result<FileFacts> {
    let mut facts = FileFacts::default();
    let mut flat: Vec<Heading> = Vec::new();
    let mut in_fence = false;
    let mut fence_marker = "";
    let mut prev_line: Option<&str> = None;

    // Reference-style link definitions (`[label]: url`) can appear anywhere in
    // the file, so gather them all up front before harvesting link uses.
    let defs = collect_link_defs(src);

    // YAML frontmatter is metadata, not content. Its closing `---` sits directly
    // under a non-blank line, which is exactly the shape of a setext H2
    // underline, so leaving it in fabricates a heading out of the last metadata
    // key — and that phantom's slug becomes a link-resolution target, which can
    // mask a genuinely broken link. Frontmatter is near-universal (Docusaurus,
    // Jekyll, Obsidian, mdBook), so this is the common case.
    let skip_lines = frontmatter_lines(src);

    for (i, raw) in src.lines().enumerate() {
        if i < skip_lines {
            prev_line = None;
            continue;
        }
        let line = raw.trim_end();
        let trimmed = line.trim_start();

        // Fenced code blocks (``` or ~~~) — ignore their content entirely.
        if let Some(marker) = fence_open(trimmed) {
            if !in_fence {
                in_fence = true;
                fence_marker = marker;
            } else if trimmed.starts_with(fence_marker) {
                in_fence = false;
            }
            prev_line = Some(line);
            continue;
        }
        if in_fence {
            continue;
        }

        // Headings: ATX (`## Title`) and Setext (text underlined by === / ---).
        let heading = atx_heading(trimmed).or_else(|| setext_heading(trimmed, prev_line));
        if let Some((level, text)) = heading {
            let s = slug(&text);
            facts.defined.insert(s.clone());
            let mut item = Item {
                kind: "section".to_string(),
                signature: text.clone(),
                line: 0, // set below via line count
                doc: None,
                calls: Vec::new(),
                children: Vec::new(),
                arity: None,
                name: Some(s),
                raw_calls: Vec::new(),
            };
            extract_links(&text, &defs, &mut item.raw_calls); // links in the heading itself
            item.line = flat.len(); // placeholder; real line assigned next pass
            flat.push(Heading { level, item });
            prev_line = Some(line);
            continue;
        }

        // Body line under the current heading: harvest links, and grab the
        // first prose line as the section's doc.
        if let Some(h) = flat.last_mut() {
            // A `[label]: url` definition line is metadata, not prose or a link
            // use — don't harvest it and don't let it become the section doc.
            if link_def(line).is_none() {
                extract_links(line, &defs, &mut h.item.raw_calls);
                if h.item.doc.is_none() && !trimmed.is_empty() && !is_structural(trimmed) {
                    h.item.doc = Some(clip(&strip_inline(trimmed)));
                }
            }
        }
        prev_line = Some(line);
    }

    // Assign real 1-based line numbers by re-scanning for each heading in order.
    assign_lines(src, &mut flat);

    facts.items = nest(flat);
    Ok(facts)
}

/// Re-walk the source assigning each heading its 1-based line. Headings are
/// unique per (order), so we match sequentially.
fn assign_lines(src: &str, flat: &mut [Heading]) {
    let mut idx = 0;
    let mut in_fence = false;
    let mut fence_marker = "";
    let mut prev: Option<&str> = None;
    // Must skip exactly what `extract` skipped, or every line number shifts.
    let skip_lines = frontmatter_lines(src);
    for (n, raw) in src.lines().enumerate() {
        if idx >= flat.len() {
            break;
        }
        if n < skip_lines {
            prev = None;
            continue;
        }
        let trimmed = raw.trim_start().trim_end();
        if let Some(marker) = fence_open(trimmed) {
            if !in_fence {
                in_fence = true;
                fence_marker = marker;
            } else if trimmed.starts_with(fence_marker) {
                in_fence = false;
            }
            prev = Some(raw);
            continue;
        }
        if in_fence {
            continue;
        }
        if atx_heading(trimmed).is_some() {
            flat[idx].item.line = n + 1;
            idx += 1;
        } else if setext_heading(trimmed, prev).is_some() {
            flat[idx].item.line = n; // the text line, one above the underline
            idx += 1;
        }
        prev = Some(raw);
    }
}

/// Turn the flat, level-tagged heading list into a nested item tree.
fn nest(flat: Vec<Heading>) -> Vec<Item> {
    let mut iter = flat.into_iter().peekable();
    take(&mut iter, 0)
}

fn take(
    iter: &mut std::iter::Peekable<std::vec::IntoIter<Heading>>,
    parent_level: usize,
) -> Vec<Item> {
    let mut out = Vec::new();
    while let Some(h) = iter.peek() {
        if h.level <= parent_level {
            break;
        }
        let Heading { level, mut item } = iter.next().unwrap();
        item.children = take(iter, level);
        out.push(item);
    }
    out
}

fn atx_heading(line: &str) -> Option<(usize, String)> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None; // `#foo` is not a heading
    }
    let text = rest.trim().trim_end_matches('#').trim();
    if text.is_empty() {
        return None;
    }
    Some((hashes, text.to_string()))
}

/// Lines occupied by a leading YAML frontmatter block, or 0 if there is none.
///
/// Only a `---` on the very first line opens frontmatter. If no closing delimiter
/// is found the block is treated as absent rather than swallowing the document.
fn frontmatter_lines(src: &str) -> usize {
    let mut lines = src.lines();
    if lines.next().map(str::trim) != Some("---") {
        return 0;
    }
    // The block must LOOK like YAML, or a document opening with a thematic
    // break gets everything up to the next `---` deleted — including real
    // headings, whose slugs then go unregistered and turn working links into
    // reported-broken ones. That is worse than the phantom heading this guards
    // against, so be strict: every line up to the delimiter must be a `key:`
    // entry, a list item, an indented continuation, a comment, or blank — and
    // the block may not be empty.
    let mut body = 0usize;
    for (i, l) in lines.enumerate() {
        let t = l.trim();
        if t == "---" || t == "..." {
            return if body > 0 { i + 2 } else { 0 };
        }
        if t.is_empty() {
            // A blank line immediately after `---` means a thematic break, not
            // frontmatter; later blanks inside a real block are tolerated.
            if body == 0 {
                return 0;
            }
            continue;
        }
        if t.starts_with('#') {
            // A comment is fine; an ATX heading is not YAML.
            if t.starts_with("# ") || t.starts_with("## ") {
                return 0;
            }
            continue;
        }
        let yamlish = t.starts_with('-')
            || l.starts_with(' ')
            || l.starts_with('\t')
            || t.split_once(':').is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || "_-.\"'".contains(c))
            });
        if !yamlish {
            return 0;
        }
        body += 1;
    }
    0
}

fn setext_heading(line: &str, prev: Option<&str>) -> Option<(usize, String)> {
    let prev = prev?.trim();
    if prev.is_empty() {
        return None;
    }
    let level = if !line.is_empty() && line.chars().all(|c| c == '=') {
        1
    } else if line.len() >= 2 && line.chars().all(|c| c == '-') {
        2
    } else {
        return None;
    };
    // Don't mistake a prior heading / list marker for setext content.
    if prev.starts_with('#') || prev.starts_with('-') || prev.starts_with('|') {
        return None;
    }
    Some((level, prev.to_string()))
}

fn fence_open(trimmed: &str) -> Option<&'static str> {
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Lines that aren't prose worth using as a doc summary.
fn is_structural(t: &str) -> bool {
    t.starts_with('|')
        || t.starts_with('>')
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("<!--")
        || t.chars().all(|c| c == '-' || c == '=' || c == '*' || c == ' ')
}

/// Extract link targets from a line: inline `[t](url)`, images `![t](url)`,
/// wiki `[[Page]]`, and reference-style `[t][ref]` / `[t][]` / `[ref]`
/// resolved against `defs`. Inline code spans are skipped.
fn extract_links(line: &str, defs: &HashMap<String, String>, out: &mut Vec<RawCall>) {
    let b: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut in_code = false;
    while i < b.len() {
        match b[i] {
            '`' => {
                in_code = !in_code;
                i += 1;
            }
            _ if in_code => i += 1,
            '[' if i + 1 < b.len() && b[i + 1] == '[' => {
                if let Some(close) = find_seq(&b, i + 2, &[']', ']']) {
                    let inner: String = b[i + 2..close].iter().collect();
                    let target = inner.split('|').next().unwrap_or("").trim();
                    if !target.is_empty() {
                        push_link(&format!("[[{target}]]"), out);
                    }
                    i = close + 2;
                } else {
                    i += 1;
                }
            }
            '[' => {
                if let Some((url, next)) = bracket_link(&b, i, defs) {
                    push_link(&url, out);
                    i = next;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
}

/// Resolve a `[...]`-opened link, trying inline, then reference, then shortcut
/// forms. Returns the target URL and the index just past the whole construct.
fn bracket_link(b: &[char], open: usize, defs: &HashMap<String, String>) -> Option<(String, usize)> {
    let rb = find_char(b, open + 1, ']')?;
    // Inline: `[text](url "title")`.
    if b.get(rb + 1) == Some(&'(') {
        let rp = find_char(b, rb + 2, ')')?;
        let raw: String = b[rb + 2..rp].iter().collect();
        let url = raw.split_whitespace().next().unwrap_or("").to_string();
        return Some((url, rp + 1));
    }
    // Reference: `[text][label]` or collapsed `[text][]` (label = text).
    if b.get(rb + 1) == Some(&'[') {
        let rb2 = find_char(b, rb + 2, ']')?;
        let label: String = b[rb + 2..rb2].iter().collect();
        let text: String = b[open + 1..rb].iter().collect();
        let key = if label.trim().is_empty() {
            text.trim().to_lowercase()
        } else {
            label.trim().to_lowercase()
        };
        return defs.get(&key).map(|url| (url.clone(), rb2 + 1));
    }
    // Shortcut: `[label]` used on its own, resolved only if a def exists.
    let text: String = b[open + 1..rb].iter().collect();
    let key = text.trim().to_lowercase();
    defs.get(&key).map(|url| (url.clone(), rb + 1))
}

/// Collect every reference-style link definition (`[label]: url`) in the file,
/// keyed by lowercased label (matching CommonMark's case-insensitive labels).
fn collect_link_defs(src: &str) -> HashMap<String, String> {
    let mut defs = HashMap::new();
    for line in src.lines() {
        if let Some((label, url)) = link_def(line) {
            defs.entry(label.to_lowercase()).or_insert(url);
        }
    }
    defs
}

/// Parse a single reference-definition line `[label]: url "optional title"`;
/// returns the (label, url) or None if the line isn't a definition.
fn link_def(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix('[')?;
    let close = rest.find("]:")?;
    let label = &rest[..close];
    if label.is_empty() || label.contains('[') {
        return None; // empty or a `[[wiki]]`-style bracket, not a link label
    }
    let url = rest[close + 2..]
        .split_whitespace()
        .next()?
        .trim_matches(|c| c == '<' || c == '>');
    if url.is_empty() {
        return None;
    }
    Some((label.to_string(), url.to_string()))
}

fn push_link(url: &str, out: &mut Vec<RawCall>) {
    let url = url.trim();
    if !url.is_empty() {
        out.push(RawCall {
            path: url.to_string(),
            recv: Receiver::Free,
        });
    }
}

fn find_char(b: &[char], from: usize, c: char) -> Option<usize> {
    (from..b.len()).find(|&i| b[i] == c)
}

fn find_seq(b: &[char], from: usize, seq: &[char]) -> Option<usize> {
    (from..b.len().saturating_sub(seq.len() - 1)).find(|&i| b[i..i + seq.len()] == *seq)
}

/// Strip common inline markdown so a doc line reads as plain prose.
fn strip_inline(s: &str) -> String {
    s.replace(['*', '`', '_'], "").trim().to_string()
}

fn clip(s: &str) -> String {
    if s.chars().count() > 120 {
        let mut out: String = s.chars().take(117).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_match_github_style() {
        assert_eq!(slug("Hello, World!"), "hello-world");
        assert_eq!(slug("API  Reference"), "api-reference");
        assert_eq!(slug("`ctx` design notes"), "ctx-design-notes");
    }

    #[test]
    fn headings_nest_by_level() {
        let src = "# Top\n\n## A\n\ntext\n\n## B\n\n### B1\n";
        let items = extract(src).unwrap().items;
        assert_eq!(items.len(), 1); // one H1
        assert_eq!(items[0].signature, "Top");
        assert_eq!(items[0].children.len(), 2); // A, B
        assert_eq!(items[0].children[1].children.len(), 1); // B1 under B
    }

    #[test]
    fn links_become_edges_code_fences_ignored() {
        let src = "# T\n\nSee [other](./other.md#sec) and [[Wiki]].\n\n```\n[not](a-link.md)\n```\n";
        let calls = &extract(src).unwrap().items[0].raw_calls;
        let paths: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(paths.contains(&"./other.md#sec"), "{paths:?}");
        assert!(paths.contains(&"[[Wiki]]"), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("a-link")), "fenced link leaked: {paths:?}");
    }

    #[test]
    fn first_prose_line_is_the_doc() {
        let src = "# Title\n\nThis is the summary.\n\nMore.\n";
        assert_eq!(
            extract(src).unwrap().items[0].doc.as_deref(),
            Some("This is the summary.")
        );
    }

    #[test]
    fn reference_style_links_resolve_via_defs() {
        let src = "\
# T

See the [design doc][design] and the [guide][] and a [shortcut].

[design]: ./design.md#goals
[guide]: ./guide.md
[shortcut]: https://example.com
";
        let paths: Vec<String> = extract(src).unwrap().items[0]
            .raw_calls
            .iter()
            .map(|c| c.path.clone())
            .collect();
        assert!(paths.contains(&"./design.md#goals".to_string()), "{paths:?}");
        assert!(paths.contains(&"./guide.md".to_string()), "{paths:?}");
        assert!(paths.contains(&"https://example.com".to_string()), "{paths:?}");
        // The definition lines themselves must not leak in as prose or links.
        assert_eq!(paths.len(), 3, "{paths:?}");
    }

    #[test]
    fn unresolved_reference_label_is_not_a_link() {
        let src = "# T\n\nText with [dangling] brackets and a task - [ ] item.\n";
        assert!(extract(src).unwrap().items[0].raw_calls.is_empty());
    }

    #[test]
    fn definition_line_is_not_the_doc() {
        let src = "# T\n\n[ref]: ./x.md\n\nReal summary here.\n";
        assert_eq!(
            extract(src).unwrap().items[0].doc.as_deref(),
            Some("Real summary here.")
        );
    }

    #[test]
    fn yaml_frontmatter_does_not_fabricate_a_heading() {
        let src = "---\ntitle: My Page\ntags: [a, b]\n---\n\n# Real Heading\n\n## Sub\n";
        let items = extract(src).unwrap().items;
        let sigs: Vec<&str> = items.iter().map(|i| i.signature.as_str()).collect();
        assert_eq!(sigs, ["Real Heading"], "frontmatter must not become a heading");
        assert_eq!(items[0].signature, "Real Heading");
        // Line numbers must still refer to the original file, not a stripped copy.
        assert_eq!(items[0].line, 6);
        assert_eq!(items[0].children[0].line, 8);
    }

    #[test]
    fn unterminated_frontmatter_does_not_swallow_the_document() {
        let src = "---\ntitle: no closing delimiter\n\n# Still A Heading\n";
        let items = extract(src).unwrap().items;
        assert!(
            items.iter().any(|i| i.signature == "Still A Heading"),
            "an unclosed block must be treated as absent"
        );
    }

    #[test]
    fn setext_headings_supported() {
        let src = "Big Title\n=========\n\nSection\n-------\n";
        let items = extract(src).unwrap().items;
        assert_eq!(items[0].signature, "Big Title");
        assert_eq!(items[0].children[0].signature, "Section");
    }
}
