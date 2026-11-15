//! Shared invariant checks for the extractor fuzz targets.
//!
//! A fuzzer that only catches panics tests one of the two things that can go
//! wrong here. The other is a parse that succeeds and reports a span pointing
//! somewhere the definition is not — a wrong answer delivered with no
//! diagnostic, which is the failure class this project treats as most serious.
//! So every target asserts the span contract as well as merely surviving.

use code_context::model::{FileFacts, Item};

/// Assert every line span in `facts` is a range a reader can actually follow.
///
/// Spans are the contract an agent acts on: `ctx` hands back `line..end_line`
/// so a caller can read exactly the item instead of the whole file around it.
/// A span that starts at 0, runs backwards, or ends past the last line sends
/// that reader to the wrong place.
pub fn check(facts: &FileFacts, src: &str) {
    // tree-sitter rows are 0-based, and a trailing newline opens one further
    // row, so `split('\n')` is the bound that matches its indexing —
    // `lines()` would under-count by one on every file ending in a newline.
    let last_line = src.split('\n').count();
    for item in &facts.items {
        check_item(item, last_line);
    }
}

fn check_item(item: &Item, last_line: usize) {
    assert!(
        !item.kind.is_empty(),
        "item at line {} has an empty kind string; `view` dispatches on kind \
         by literal match, so an empty one silently vanishes from every view",
        item.line
    );
    check_span(item.line, item.end_line, last_line, "item", &item.kind);
    // `extract` runs before resolution, so edges live in `raw_calls`;
    // `calls` is still empty at this point.
    for call in &item.raw_calls {
        check_span(call.line, call.end_line, last_line, "call", &call.path);
    }
    for child in &item.children {
        check_item(child, last_line);
    }
}

fn check_span(line: usize, end_line: usize, last_line: usize, what: &str, label: &str) {
    assert!(line >= 1, "{what} `{label}`: line is 0, but spans are 1-based");
    assert!(
        end_line >= line,
        "{what} `{label}`: span runs backwards ({line}..{end_line})"
    );
    assert!(
        end_line <= last_line,
        "{what} `{label}`: span ends at line {end_line}, past the last line ({last_line})"
    );
}
