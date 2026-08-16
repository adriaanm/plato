//! Undo CSS-grid scaffolding before readability scoring.
//!
//! The failure this prevents, seen on a real recipe page (Squarespace, but
//! Bootstrap-style grids fail the same way): the article's body is written as
//! sibling text blocks, and the page's layout engine wraps *some* of them in
//! an extra `row > col` pair -- the instructions shared a row with an image
//! column, the ingredient list did not. Readability picks the deep, prose-rich
//! block as its top candidate and then looks for the rest of the article among
//! that block's *siblings*; the ingredient list, one level shallower, is an
//! aunt, and no score can save a node the join never visits. The reader showed
//! a ragù with no ingredients.
//!
//! The grid is pure presentation -- a `div class="row"` holding a
//! `div class="col-8"` says where things sit, never what they are -- so
//! removing it loses nothing and returns the body to what its author wrote:
//! blocks, side by side. Two conditions keep the unwrapping honest. The class
//! list must name a place in a grid ([`is_grid_token`]), and the element must
//! carry no text of its own -- a wrapper that says anything is not a wrapper.

use dom_query::Document;

/// True for a class token that names a place in a layout grid rather than a
/// kind of content: `row`, `col`, `col-8`, `sqs-col-8`, `span-8`,
/// `columns-12`, `grid-12`. A vendor prefix (`sqs-`) and a width suffix
/// (`-12`) dress the same five words up in every grid system; strip both and
/// compare. Bare `span` is *not* one -- without a width it is more likely the
/// HTML element's name doing duty as a class.
fn is_grid_token(token: &str) -> bool {
    let token = token.to_ascii_lowercase();
    let token = token.strip_prefix("sqs-").unwrap_or(&token);
    let (stem, has_width) = match token.rsplit_once('-') {
        Some((stem, width)) if !width.is_empty()
            && width.bytes().all(|b| b.is_ascii_digit()) => (stem, true),
        _ => (token, false),
    };
    match stem {
        "row" | "col" | "column" | "columns" | "grid" => true,
        "span" => has_width,
        _ => false,
    }
}

/// Whether this `div`'s class marks it as grid scaffolding safe to unwrap.
fn is_grid_wrapper(node: &dom_query::NodeRef) -> bool {
    let class = node.attr_or("class", "");
    if !class.split_ascii_whitespace().any(is_grid_token) {
        return false;
    }
    // Only a pure wrapper goes: direct text means the div is content wearing
    // a grid class, and content is exactly what must not be rearranged.
    !node.children_it(false)
         .any(|child| child.is_text() && !child.text().trim().is_empty())
}

/// Unwrap every layout-grid `div`, keeping its children in place. Returns the
/// flattened document, or `None` when there was no grid to flatten -- so the
/// caller can hand the original string through untouched.
pub fn flatten_grid(html: &str) -> Option<String> {
    let doc = Document::from(html);
    let wrappers: Vec<_> = doc.select("div[class]")
                              .nodes()
                              .iter()
                              .filter(|node| is_grid_wrapper(node))
                              .cloned()
                              .collect();
    if wrappers.is_empty() {
        return None;
    }
    // Node ids stay valid across mutation, so one upfront selection covers
    // nested wrappers too: unwrapping a row relocates its cols, and the cols'
    // own turns still find them, one level higher.
    for wrapper in &wrappers {
        // Bound before the match: the iterator holds the tree borrowed, and
        // `unwrap_node` needs it back.
        let first_child = wrapper.children_it(false).next();
        // `unwrap_node` removes the *parent* of the node it is called on.
        match first_child {
            Some(child) => child.unwrap_node(),
            // An empty wrapper has no child to speak for it; it just goes.
            None => wrapper.remove_from_parent(),
        }
    }
    Some(doc.html().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_tokens_are_recognized_with_prefix_and_width() {
        for yes in ["row", "col", "col-8", "sqs-row", "sqs-col-8", "span-8",
                    "columns-12", "sqs-grid-12", "Row"] {
            assert!(is_grid_token(yes), "{yes:?} should be a grid token");
        }
        for no in ["span", "sidebar", "content", "colophon", "rowdy",
                   "col8", "article-8", ""] {
            assert!(!is_grid_token(no), "{no:?} should not be a grid token");
        }
    }

    #[test]
    fn wrappers_unwrap_and_content_stays() {
        let html = r#"<html><body><div class="row">
            <div class="col-8"><p>kept</p></div>
            <div class="col-4"><p>also kept</p></div>
        </div></body></html>"#;
        let flat = flatten_grid(html).unwrap();
        assert!(flat.contains("<p>kept</p>"));
        assert!(flat.contains("<p>also kept</p>"));
        assert!(!flat.contains("row"));
        assert!(!flat.contains("col-8"));
    }

    #[test]
    fn a_div_with_its_own_text_is_content_not_scaffolding() {
        let html = r#"<html><body><div class="row">words of its own</div></body></html>"#;
        assert!(flatten_grid(html).is_none());
    }

    #[test]
    fn a_gridless_page_flattens_to_none() {
        assert!(flatten_grid("<html><body><div class=\"post\"><p>hi</p></div></body></html>")
                    .is_none());
    }
}
