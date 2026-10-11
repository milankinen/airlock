//! The `airlock self-update` command.
//!
//! Updates the airlock binary to the latest release, or only tells if a
//! newer release is available. Development builds cannot update.

use clap::Args;

use crate::cli;
use crate::self_update::{self, ReleaseSource, Version};

/// CLI arguments for `airlock self-update`.
#[derive(Args, Debug)]
pub struct SelfUpdateArgs {
    /// Only check for a newer release, do not install it
    #[arg(long)]
    pub check: bool,
}

/// Update the running airlock binary to the latest release.
/// Returns:
///   Process exit code: 0 on success, 1 on error. With `--check`, 0 also
///   when a newer release is available.
pub async fn main(args: &SelfUpdateArgs) -> i32 {
    let result = tokio::select! {
        result = run(args) => result,
        () = cli::interrupted() => Err(anyhow::anyhow!("update interrupted")),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            cli::error!("Self-update failed: {e:#}");
            1
        }
    }
}

async fn run(args: &SelfUpdateArgs) -> anyhow::Result<()> {
    let Some(current) = cli::release_version() else {
        anyhow::bail!(
            "this is a development build ({}). Only release builds can update",
            cli::version_string(true)
        );
    };
    let current = Version::parse(&current)?;
    let source = ReleaseSource::default();
    let client = self_update::http_client(&current)?;

    let spinner = cli::spinner("checking for updates");
    let latest = self_update::latest_version(&client, &source).await;
    spinner.finish_and_clear();
    let latest = latest?;

    if latest <= current {
        cli::log!("{} airlock {current} is the latest version", cli::check());
        return Ok(());
    }
    if args.check {
        cli::log!("airlock {latest} is available (current version: {current})");
        cli::log!("Run `airlock self-update` to install it.");
        return Ok(());
    }

    // Resolve symlinks, so that the update replaces the real file and not
    // the link to it.
    let target = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| anyhow::anyhow!("cannot find the airlock binary: {e}"))?;

    cli::log!("Updating airlock {current} -> {latest}");
    let mp = cli::multi_progress();
    let pb = cli::layer_progress_bar(&mp, 0);
    let result = self_update::install(&client, &source, &latest, &target, &pb).await;
    let _ = mp.clear();
    result?;
    cli::log!(
        "{} airlock {latest} installed to {}",
        cli::check(),
        target.display()
    );
    Ok(())
}
