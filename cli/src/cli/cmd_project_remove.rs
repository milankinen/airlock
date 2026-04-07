use std::io::Write;

use crate::{cli, project};

pub fn run(path: Option<&str>, yes: bool) -> i32 {
    let project = match project::load(path) {
        Ok(p) => p,
        Err(e) => {
            cli::error!("{e:#}");
            return 1;
        }
    };

    if !project.cache_dir.exists() {
        cli::error!("no project found for {}", project.cwd.display());
        return 1;
    }

    if project.is_running() {
        cli::error!("project is running, stop it first");
        return 1;
    }

    if !yes {
        eprint!("Remove project for {}? [y/N] ", project.cwd.display());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err()
            || !input.trim().eq_ignore_ascii_case("y")
        {
            println!("Aborted.");
            return 0;
        }
    }

    if let Err(e) = std::fs::remove_dir_all(&project.cache_dir) {
        cli::error!("failed to remove project: {e}");
        return 1;
    }

    println!("Removed project for {}", project.cwd.display());
    0
}
