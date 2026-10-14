//! The `airlock self-update` command.
//!
//! Updates the airlock binary to the latest release, or only tells if a
//! newer release is available. Development builds cannot update.

use clap::Args;

use crate::self_update::{self, ReleaseSource, Version};
use crate::util::verified_path::VerifiedPath;
use crate::{cli, messages};

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
        () = cli::interrupted() => Err(anyhow::anyhow!(messages::cli::UPDATE_INTERRUPTED)),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            cli::error!("{}", messages::cli::self_update_failed(format!("{e:#}")));
            1
        }
    }
}

async fn run(args: &SelfUpdateArgs) -> anyhow::Result<()> {
    let Some(current) = cli::release_version() else {
        anyhow::bail!(messages::cli::dev_build_cannot_update(cli::version_string(
            true
        )));
    };
    let current = Version::parse(&current)?;
    let source = ReleaseSource::default();
    let client = self_update::http_client(&current)?;

    let spinner = cli::spinner(messages::cli::CHECKING_FOR_UPDATES);
    let latest = self_update::latest_version(&client, &source).await;
    spinner.finish_and_clear();
    let latest = latest?;

    if latest <= current {
        cli::log!(
            "{} {}",
            cli::check(),
            messages::cli::already_latest_version(current)
        );
        return Ok(());
    }
    if args.check {
        cli::log!("{}", messages::cli::update_available(latest, current));
        cli::log!("{}", messages::cli::RUN_SELF_UPDATE_HINT);
        return Ok(());
    }

    // Resolve symlinks, so that the update replaces the real file and not
    // the link to it. A package manager often installs a link.
    let target = std::env::current_exe()
        .and_then(|exe| VerifiedPath::new_follow(exe, None))
        .map_err(|e| anyhow::anyhow!(messages::cli::cannot_find_binary(e)))?;

    cli::log!("{}", messages::cli::updating_airlock(&current, &latest));
    let mp = cli::multi_progress();
    let pb = cli::layer_progress_bar(&mp, 0);
    let result = self_update::install(&client, &source, &latest, &target, &pb).await;
    let _ = mp.clear();
    result?;
    cli::log!(
        "{} {}",
        cli::check(),
        messages::cli::airlock_installed(&latest, &target)
    );
    Ok(())
}
