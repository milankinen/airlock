//! Pages of the airlock user manual for agents.
//!
//! Gives the Markdown pages of the manual that the build embeds. Images of
//! the manual are not part of it.

include!(concat!(env!("OUT_DIR"), "/manual.rs"));

/// Get the names of all manual pages, as paths relative to the manual root.
pub(crate) fn names() -> Vec<&'static str> {
    PAGES.iter().map(|(name, _)| *name).collect()
}

/// Get the content of the manual page `name`, or `None` if no page has
/// this name.
pub(crate) fn page(name: &str) -> Option<&'static str> {
    let name = name.trim().trim_start_matches("./");
    PAGES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, text)| *text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the manual has only Markdown pages and includes the network
    /// configuration page that the `upsert_rule` tool refers to.
    ///   1. List the page names
    ///   2. Check that all are Markdown and that the network page is there
    ///   3. Get the network page and an unknown page
    #[test]
    fn manual_has_only_markdown_pages_with_network_page() {
        let names = names();
        let is_md = |n: &&str| {
            std::path::Path::new(n)
                .extension()
                .is_some_and(|e| e == "md")
        };
        assert!(names.iter().all(is_md), "{names:?}");
        assert!(names.contains(&"configuration/network.md"));
        assert!(
            page("configuration/network.md")
                .unwrap()
                .contains("[network")
        );
        assert!(page("logo.png").is_none());
    }
}
