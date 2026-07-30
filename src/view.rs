use clap::ValueEnum;

use crate::model::{Graph, Item, Lang};

/// Feature-breakpointed detail levels: each step up adds a category of
/// information, so token spend scales with informational need.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum View {
    /// Modules, deps, re-exports, and type names only (~cheapest).
    Skeleton,
    /// + public signatures, struct fields, enum variants. No call edges.
    Interface,
    /// + private items and call edges.
    Full,
}

impl View {
    pub fn name(self) -> &'static str {
        match self {
            View::Skeleton => "skeleton",
            View::Interface => "interface",
            View::Full => "full",
        }
    }
}

/// Rendered size of a single module, measured rather than estimated.
fn rendered_size(m: &crate::model::Module) -> usize {
    let mut s = String::new();
    crate::render::module_md(m, &mut s);
    s.len()
}

/// How pruning shrank a graph.
pub struct PruneStats {
    /// Modules dropped in total.
    pub omitted: usize,
    /// Docs dropped specifically because prose hit its share ceiling — they
    /// would otherwise have been central enough to keep.
    pub prose_capped: usize,
}

/// Keep the `keep` most central **code** modules, plus the most central docs
/// that fit within `max_prose_share` of the result. Returns what was dropped.
///
/// `keep` counts code, not total modules: prose rides along on top of it. The
/// caller sizes the output by measuring the rendered text, so admitting docs
/// simply shifts which `keep` fits, and the budget still holds exactly. In a repo
/// with no code at all, `keep` is a plain module count and no ceiling applies.
///
/// This is the rung below `Skeleton`: when even the coarsest view blows the token
/// budget, detail can no longer be reduced, so whole modules have to go. Dropping
/// the least-central ones keeps the parts of the graph everything else depends
/// on, which is what "orient me" actually needs.
///
/// Two properties hold **by construction**, which is why selection and capping
/// are one pass rather than prune-then-filter:
///
/// 1. **Code is never crowded out.** Code modules claim slots in centrality order
///    first, so a map of a repo containing any code always contains code. Docs
///    cross-link and earn real PageRank — and modules importing only external
///    packages are rank *sinks* that can outrank them — so which way a naive
///    ranking falls is luck, and a "codebase map" that is all prose is the worst
///    output this tool can produce.
/// 2. **Prose cannot exceed its share.** Docs keep their rank; they just cannot
///    spend more than `max_prose_share` of the output. Since only prose is
///    squeezed, the code total is fixed and the ceiling has a closed form:
///    `prose ≤ share × code / (1 − share)`.
///
/// A repo with no code at all is exempt from the ceiling — there is nothing to
/// balance against, and emptying the map would be worse than an unbalanced one.
///
/// Sizes are measured with the Markdown renderer, so under `--format json` the
/// share is approximate while the ordering is identical. Whole modules are
/// dropped; items inside a kept module are never truncated.
pub fn prune_to_central(g: &mut Graph, keep: usize, max_prose_share: f64) -> PruneStats {
    let n = g.modules.len();
    if keep >= n {
        return PruneStats { omitted: 0, prose_capped: 0 };
    }
    let share = if (0.0..1.0).contains(&max_prose_share) {
        max_prose_share
    } else {
        0.0
    };

    let sizes: Vec<usize> = g.modules.iter().map(rendered_size).collect();
    let is_prose: Vec<bool> = g.modules.iter().map(|m| m.lang == Lang::Markdown).collect();
    let any_code = is_prose.iter().any(|&p| !p);
    let order = crate::query::centrality_order(g);

    let mut keep_flag = vec![false; n];
    let mut prose_capped = 0usize;

    if !any_code {
        // Nothing to balance against: plain centrality order, no ceiling.
        for &i in order.iter().take(keep) {
            keep_flag[i] = true;
        }
    } else {
        // Pass 1 — `keep` counts CODE modules. Code claims its slots first in
        // centrality order, which is what guarantees a code map contains code.
        let mut admitted = 0usize;
        let mut code_bytes = 0usize;
        let mut deferred_prose: Vec<usize> = Vec::new();
        for &i in &order {
            if is_prose[i] {
                deferred_prose.push(i);
            } else if admitted < keep {
                keep_flag[i] = true;
                admitted += 1;
                code_bytes += sizes[i];
            }
        }

        // Pass 2 — docs ride along on top, in centrality order, up to their
        // share of the result. There is no slot limit here: the caller sizes the
        // map by measuring the rendered text, so admitting prose simply shifts
        // which `keep` fits. A doc too large for the remaining allowance is
        // skipped rather than blocking the smaller ones behind it.
        let limit = (share * code_bytes as f64 / (1.0 - share)) as usize;
        let mut prose_bytes = 0usize;
        for &i in &deferred_prose {
            if prose_bytes + sizes[i] <= limit {
                keep_flag[i] = true;
                prose_bytes += sizes[i];
            } else {
                prose_capped += 1;
            }
        }
    }

    // Retain in original order so the map's layout is unchanged by pruning.
    let mut idx = 0;
    g.modules.retain(|_| {
        let k = keep_flag[idx];
        idx += 1;
        k
    });
    PruneStats { omitted: n - g.modules.len(), prose_capped }
}

pub fn apply(g: &mut Graph, view: View) {
    if view == View::Full {
        return;
    }
    for m in &mut g.modules {
        let items = std::mem::take(&mut m.items);
        m.items = transform(items, view, m.lang);
    }
}

fn transform(items: Vec<Item>, view: View, lang: Lang) -> Vec<Item> {
    items
        .into_iter()
        .filter_map(|it| transform_item(it, view, lang))
        .collect()
}

fn transform_item(mut it: Item, view: View, lang: Lang) -> Option<Item> {
    it.calls.clear();
    match view {
        View::Full => Some(it),
        View::Skeleton => match it.kind.as_str() {
            "struct" | "enum" | "trait" | "class" | "interface" | "type" => {
                if let Some(n) = &it.name {
                    it.signature = format!("{} {}", it.kind, n);
                }
                it.children.clear();
                Some(it)
            }
            "mod" | "section" => {
                it.children = transform(std::mem::take(&mut it.children), view, lang);
                Some(it)
            }
            _ => None,
        },
        View::Interface => match it.kind.as_str() {
            "fn" | "def" => is_public(&it, lang).then_some(it),
            "trait" => {
                // Trait methods have no `pub`; they ARE the interface.
                for c in &mut it.children {
                    clear_calls(c);
                }
                Some(it)
            }
            "class" | "mod" | "section" => {
                it.children = transform(std::mem::take(&mut it.children), view, lang);
                Some(it)
            }
            "impl" => {
                it.children = transform(std::mem::take(&mut it.children), view, lang);
                // An inherent impl with no public methods is implementation
                // detail; a trait impl is interface even when empty here.
                let trait_impl = it.signature.contains(" for ");
                (!it.children.is_empty() || trait_impl).then_some(it)
            }
            _ => is_public(&it, lang).then_some(it),
        },
    }
}

pub fn is_public(it: &Item, lang: Lang) -> bool {
    match lang {
        // `pub(crate)` / `pub(super)` / `pub(in path)` are restricted, not public:
        // treating them as API surface both pads `--view interface` and makes
        // `changed --api` cry breaking-change over crate-internal edits.
        Lang::Rust => it.signature.starts_with("pub") && !it.signature.starts_with("pub("),
        Lang::Python => match &it.name {
            // Dunders (__init__, __call__) are interface despite the underscore.
            Some(n) => !n.starts_with('_') || (n.starts_with("__") && n.ends_with("__")),
            None => true,
        },
        // Exported top-level items are prefixed `export`; a non-exported
        // top-level declaration starts with a declaration keyword. Anything
        // else (a public class method like `compute()`) is interface.
        Lang::TypeScript => {
            let s = it.signature.as_str();
            s.starts_with("export")
                || ![
                    "function ",
                    // `async function foo()` is a top-level declaration too, and
                    // without it here a non-exported async function reads as API.
                    "async function ",
                    "async ",
                    "const ",
                    "class ",
                    "abstract ",
                    "interface ",
                    "type ",
                    "enum ",
                ]
                .iter()
                .any(|k| s.starts_with(k))
        }
        // Every heading is part of the document's structure.
        Lang::Markdown => true,
    }
}

fn clear_calls(it: &mut Item) {
    it.calls.clear();
    for c in &mut it.children {
        clear_calls(c);
    }
}
