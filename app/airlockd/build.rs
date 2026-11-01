//! Embeds the Markdown pages of the user manual, so the `airlock` MCP server
//! can give them to agents. Other files of the manual (images) stay out:
//! they are large and do not help an agent.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/manual/src");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut pages = Vec::new();
    collect(&root, &root, &mut pages);
    pages.sort();

    let mut out =
        String::from("/// Manual pages: path relative to the manual root, and content.\n");
    out.push_str("pub(crate) static PAGES: &[(&str, &str)] = &[\n");
    for (name, path) in &pages {
        println!("cargo:rerun-if-changed={}", path.display());
        let _ = writeln!(
            out,
            "    ({name:?}, include_str!({:?})),",
            path.display().to_string()
        );
    }
    out.push_str("];\n");
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("manual.rs");
    std::fs::write(dest, out).unwrap();
}

/// Find the Markdown files under `dir`, with their paths relative to `root`.
fn collect(root: &Path, dir: &Path, pages: &mut Vec<(String, PathBuf)>) {
    // A missing manual gives an empty list, not a build error.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            println!("cargo:rerun-if-changed={}", path.display());
            collect(root, &path, pages);
        } else if path.extension().is_some_and(|e| e == "md") {
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            pages.push((name, path.canonicalize().unwrap()));
        }
    }
}
