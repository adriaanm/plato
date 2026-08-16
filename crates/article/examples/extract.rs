//! Run the extractor over a saved page, from the command line:
//!
//! ```sh
//! curl -sL https://example.com/story -o /tmp/page.html
//! cargo run -p plato-article --example extract /tmp/page.html https://example.com/story
//! ```
//!
//! This is the debugging loop for "why does this article not open": the same
//! bytes the reader would fetch, the same call it would make, and the answer
//! -- or the refusal -- on stdout instead of an e-ink screen.

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(url)) = (args.next(), args.next()) else {
        eprintln!("usage: extract <saved-page.html> <original-url>");
        std::process::exit(2);
    };
    let raw = std::fs::read(&path).expect("readable file");
    match plato_article::extract(&raw, &url) {
        Ok(article) => {
            println!("title:  {}", article.title);
            println!("byline: {:?}", article.byline);
            println!("site:   {:?}", article.site);
            println!("html:   {} bytes", article.html.len());
            println!("---\n{}", article.html);
        }
        Err(err) => {
            eprintln!("refused: {err:#}");
            std::process::exit(1);
        }
    }
}
