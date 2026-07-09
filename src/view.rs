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
            "mod" => {
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
            "class" | "mod" => {
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
        Lang::Rust => it.signature.starts_with("pub"),
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
    }
}

fn clear_calls(it: &mut Item) {
    it.calls.clear();
    for c in &mut it.children {
        clear_calls(c);
    }
}
