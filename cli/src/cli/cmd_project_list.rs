//! `ez project list` — list all known projects.

use crate::{cli, project};

/// Display row for a single project.
struct Row {
    id: String,
    cwd: String,
    running: bool,
    image: String,
    ago: Option<String>,
}

/// Print all projects with abbreviated IDs, paths, images, and status.
pub fn run() -> i32 {
    let projects_dir = match crate::cache::cache_dir() {
        Ok(d) => d.join("projects"),
        Err(e) => {
            cli::error!("cannot access cache: {e}");
            return 1;
        }
    };

    if !projects_dir.exists() {
        println!("No projects found.");
        return 0;
    }

    let Ok(entries) = std::fs::read_dir(&projects_dir) else {
        println!("No projects found.");
        return 0;
    };

    // Collect all projects first so we can compute abbreviated IDs
    let rows: Vec<Row> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let dir = e.path();
            let id = project::project_id(&dir);
            let cwd = std::fs::read_to_string(dir.join("cwd"))
                .map_or_else(|_| "?".into(), |s| s.trim().to_string());
            let running = project::is_running(&dir);
            let image = project::image(&dir).unwrap_or_else(|| "?".into());
            let ago = project::last_run_ago(&dir);
            Row {
                id,
                cwd,
                running,
                image,
                ago,
            }
        })
        .collect();

    if rows.is_empty() {
        println!("No projects found.");
        return 0;
    }

    let hashes: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let abbrev = project::min_unique_prefix_len(&hashes);

    for row in &rows {
        let short_id = cli::dim(&row.id[..abbrev]);
        let status = if row.running {
            cli::red(" (running)")
        } else {
            String::new()
        };
        let ago = row
            .ago
            .as_deref()
            .map(|a| format!(" ({a})"))
            .unwrap_or_default();
        println!("{short_id}");
        println!("  path:  {}{status}", row.cwd);
        println!("  image: {}{ago}", row.image);
    }

    0
}
