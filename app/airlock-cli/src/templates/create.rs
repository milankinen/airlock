//! Template creation.
//!
//! Asks the user for packs and settings with the setup wizard, and builds
//! the template from the answers: gets the image, installs the packs on a
//! new disk, and keeps the disk, the install records and the config. The
//! config pins the digest of the image.
//!
//! The build runs in a build directory next to the templates. The template
//! appears only after a successful build.

use std::io::Write;

use super::{CONFIG_FILE, FILES, templates_dir};
use crate::cli::LogLevel;
use crate::config::config_values::ImageRef;
use crate::config::{self, ConfigOverrides, LayeredConfig};
use crate::context::Context;
#[cfg(not(test))]
use crate::oci::prepare as prepare_image;
use crate::oci::{ImageChange, ImageChangeStop, OnImageChange};
#[cfg(not(test))]
use crate::packs::install::facts::check as check_image;
use crate::packs::install::plan::{self, DecideInput, Wanted};
use crate::packs::install::state::{self as install_state, InstallState, ReadState};
use crate::packs::{InstallerScript, PackManager};
use crate::project::{self, SandboxLock};
use crate::start::install::{self, InstallDecision};
use crate::start::{Exit, SandboxOptions};
#[cfg(test)]
use crate::test_cfg::start::{check_image, prepare_image};
use crate::util::verified_path::VerifiedPath;
use crate::util::{PinnedDir, vfs};
use crate::{cli, messages, start};

/// Prefix of the build directory of a template. A template name cannot
/// start with `.`, thus the template list does not show the build, staging
/// and old directories.
const BUILD_PREFIX: &str = ".build-";
/// Prefix of the staging directory of a new template (see `keep_files`).
const STAGING_PREFIX: &str = ".new-";
/// Prefix of a replaced template until its removal (see `keep_files`).
const OLD_PREFIX: &str = ".old-";

/// Create the template `name` with the setup wizard.
///
/// The wizard is the same as the wizard of `airlock start`, but without the
/// question about `airlock.toml`. The template can start from an other
/// template.
/// Args:
///  - `context`: Process context
///  - `name`: Name of the new template
///  - `force`: Replace a template with the same name. The old template
///    stays until the new one is ready.
///  - `log_level`: Log level of the install VM
///  - `verbose`: Show the install output
///
/// Returns:
///   The exit for an error, an aborted wizard or a failed install. A
///   template with the same name is an error without `force`.
pub async fn create(
    context: &Context,
    name: &str,
    force: bool,
    log_level: LogLevel,
    verbose: bool,
) -> Result<(), Exit> {
    super::check_name(name).map_err(|e| Exit::error(2, e))?;
    if !force {
        check_absent(&context.roots.data_dir, name)?;
    }
    let packs = crate::packs::init().map_err(Exit::config)?;
    let build = BuildDir::new(&context.roots.data_dir, name)?;
    let project_dir = build.project.to_path().map_err(anyhow::Error::from)?;
    // The wizard checks the answers with the user files only. The build
    // project directory has no config files.
    let config = config::load(project_dir, &project_dir.join(".airlock")).map_err(Exit::config)?;
    let templates = super::list(&context.roots.data_dir);
    let generated = Box::pin(start::wizard::run_template_wizard(
        project_dir,
        &context.data_dir,
        &packs,
        &config,
        &context.vault,
        &templates,
    ))
    .await?;
    let config = config
        .with_generated_project(generated)
        .map_err(Exit::config)?;
    let options = SandboxOptions {
        yes: true,
        log_level,
        verbose,
        allow_config_changes: false,
        overrides: ConfigOverrides::default(),
    };
    Box::pin(build_template(
        context, name, &packs, &config, &options, force,
    ))
    .await?;
    cli::log!(
        "{} {}",
        cli::check(),
        messages::templates::template_created(name)
    );
    Ok(())
}

/// Build the template `name` from `config`.
///
/// Gets the image, installs the packs of `config` on a new disk, and moves
/// the disk, the install records, the image link and the config to the
/// template directory. If `config` comes from an other template, the build
/// starts with a copy of that template. The build directory is removed at
/// the end, also on failure.
/// Args:
///  - `context`: Process context
///  - `name`: Name of the new template
///  - `packs`: Available packs
///  - `config`: User files and the config of the wizard
///  - `options`: Install options
///  - `replace`: Replace a template with the same name
///
/// Returns:
///   The exit for an error or a failed install. Without `replace`, a
///   template with the same name is an error.
pub(crate) async fn build_template(
    context: &Context,
    name: &str,
    packs: &PackManager,
    config: &LayeredConfig,
    options: &SandboxOptions,
    replace: bool,
) -> Result<(), Exit> {
    let data_dir = &context.roots.data_dir;
    let build = BuildDir::new(data_dir, name)?;
    crate::cache::create_private_dir(&templates_dir(data_dir).map_err(anyhow::Error::from)?)?;
    let lock = SandboxLock::acquire_dir(
        &build.sandbox,
        build.project.to_path().map_err(anyhow::Error::from)?,
    )?;
    let result = Box::pin(build_locked(
        context, name, packs, config, options, &build, &lock,
    ))
    .await
    .and_then(|toml| keep_files(data_dir, name, &build.sandbox, &toml, replace));
    drop(lock);
    if result.is_ok() {
        let _ = vfs::remove_dir_all(&build.root);
    } else {
        remove_except_log(&build);
    }
    result
}

/// [`build_template`] under the lock of the build sandbox `lock`, up to the
/// save of the template files.
/// Returns:
///   The template config, with the image pinned.
async fn build_locked(
    context: &Context,
    name: &str,
    packs: &PackManager,
    config: &LayeredConfig,
    options: &SandboxOptions,
    build: &BuildDir,
    lock: &SandboxLock,
) -> Result<String, Exit> {
    let resolved = config
        .resolve(&context.data_dir, packs, &options.overrides)
        .await
        .map_err(Exit::config)?;
    start::env::check_env_early(&resolved.values, &context.vault)?;
    vfs::create_dir_all(&build.project).map_err(anyhow::Error::from)?;
    let sandbox = PinnedDir::pin(lock.dir().to_path().map_err(anyhow::Error::from)?)
        .map_err(anyhow::Error::from)?;
    // Remove the files of an earlier build that stopped.
    for file in FILES {
        sandbox.remove(file).map_err(anyhow::Error::from)?;
    }
    let base = config.generated_project().and_then(|g| g.template.clone());
    if let Some(base) = &base {
        let template = super::find(&context.roots.data_dir, base).map_err(|e| Exit::error(1, e))?;
        super::seed::seed(&template, &sandbox, &build.sandbox)?;
    }

    let values = &resolved.values;
    cli::log!("{}", messages::templates::building_template(name));
    let prepared = prepare_image(
        &context.roots.data_dir,
        &build.sandbox,
        &values.vm.image,
        &context.vault,
        OnImageChange::Recreate,
    )
    .await
    .map_err(image_error)?;
    let image = prepared.image;
    let pin = prepared.pin;
    // The base template disk belongs to the base image. With an other
    // image, start with an empty disk.
    if base.is_some() && prepared.change != ImageChange::Unchanged {
        project::reset_disk(&sandbox)?;
        sandbox
            .remove(install_state::STATE_FILE)
            .map_err(anyhow::Error::from)?;
    }

    let state = match install_state::read(&sandbox) {
        ReadState::Ok(state) => state,
        _ => InstallState::default(),
    };
    let candidates = install::install_candidates(&resolved.packs);
    let wanted: Vec<Wanted> = candidates
        .iter()
        .map(|t| Wanted {
            id: t.pack.clone(),
            fingerprint: t.fingerprint.clone(),
        })
        .collect();
    let mut plan = plan::decide(&DecideInput {
        state: &state,
        wanted: &wanted,
        disk: project::disk_id(&sandbox),
        image_id: Some(&image.image_id),
    });
    if !plan.pending.is_empty() {
        check_image(&context.roots.data_dir, &image).map_err(|e| Exit::error(2, e))?;
    }
    project::ensure_disk(&sandbox, &values.disk, base.is_some())?;
    let to_install: Vec<InstallerScript> = candidates
        .into_iter()
        .filter(|t| plan.pending.iter().any(|p| p.id == t.pack))
        .collect();
    let transitions = std::mem::take(&mut plan.transitions);
    let installs = InstallDecision::save(sandbox, state, &transitions, to_install, &image)?;
    let project = project::open(lock, values.clone(), None, context.clone())?;
    Box::pin(install::install_tools(
        &project, &resolved, &image, installs, options,
    ))
    .await?;

    let generated = config
        .generated_project()
        .ok_or_else(|| Exit::error(1, messages::templates::NO_WIZARD_CONFIG))?;
    pin_image(&generated.toml, &values.vm.image, pin.as_deref()).map_err(|e| Exit::error(1, e))
}

/// Set `vm.image` in the config text `toml` to the image `image` with the
/// digest `pin` (see [`crate::oci::PreparedImage::pin`]).
/// Returns:
///   The new config text, or error if `toml` is not valid TOML. Without a
///   pin, `toml` does not change: a config from a base template keeps its
///   pin, and an image without a registry digest stays unpinned.
// The table repeats all image settings. The settings of a distro pack then
// cannot change the image.
fn pin_image(toml: &str, image: &ImageRef, pin: Option<&str>) -> anyhow::Result<String> {
    let Some(pin) = pin else {
        return Ok(toml.to_string());
    };
    let mut doc: toml_edit::DocumentMut = toml.parse()?;
    let base = match image.pinned_digest() {
        Some(_) => image.name.rsplit_once('@').map_or(&*image.name, |(n, _)| n),
        None => &image.name,
    };
    let mut pinned = image.clone();
    pinned.name = format!("{base}@{pin}");
    let text = toml::to_string(&serde_json::json!({ "vm": { "image": pinned } }))?;
    let vm: toml_edit::DocumentMut = text.parse()?;
    if !doc.contains_table("vm") {
        doc["vm"] = toml_edit::table();
    }
    doc["vm"]["image"] = vm["vm"]["image"].clone();
    Ok(doc.to_string())
}

/// Move the template files from the build sandbox `sandbox_dir` to the new
/// template directory of `name`, and write the config `toml` there.
/// Args:
///  - `data_dir`: Airlock data directory
///  - `name`: Template name
///  - `sandbox_dir`: The build sandbox
///  - `toml`: Template config
///  - `replace`: Replace a template with the same name
///
/// Returns:
///   Error if the template exists and `replace` is false, or a move
///   fails. Then the old template does not change.
// The files go to a staging directory first. The rename of the staging
// directory is the commit point: a parallel create of the same name fails
// there. A replaced template moves aside first, and is removed after the
// commit.
fn keep_files(
    data_dir: &VerifiedPath,
    name: &str,
    sandbox_dir: &VerifiedPath,
    toml: &str,
    replace: bool,
) -> Result<(), Exit> {
    let root = templates_dir(data_dir).map_err(anyhow::Error::from)?;
    let dir = root.join(name).map_err(anyhow::Error::from)?;
    let staging = root
        .join(format!("{STAGING_PREFIX}{name}"))
        .map_err(anyhow::Error::from)?;
    let _ = vfs::remove_dir_all(&staging);
    let staged = (|| -> anyhow::Result<()> {
        vfs::create_dir(&staging)?;
        for file in FILES {
            let src = sandbox_dir.join(file)?;
            if vfs::symlink_metadata(&src).is_ok() {
                vfs::rename(&src, &staging.join(file)?)?;
            }
        }
        vfs::create_new(&staging.join(CONFIG_FILE)?)?.write_all(toml.as_bytes())?;
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = vfs::remove_dir_all(&staging);
        return Err(Exit::error(
            1,
            messages::templates::save_template_failed(name, e),
        ));
    }
    let old = root
        .join(format!("{OLD_PREFIX}{name}"))
        .map_err(anyhow::Error::from)?;
    let _ = vfs::remove_dir_all(&old);
    let replaced = replace && vfs::rename(&dir, &old).is_ok();
    if let Err(e) = vfs::rename(&staging, &dir) {
        let _ = vfs::remove_dir_all(&staging);
        if replaced {
            let _ = vfs::rename(&old, &dir);
        }
        // Renaming onto a non-empty directory fails: the template exists.
        return Err(if vfs::symlink_metadata(&dir).is_ok() {
            exists_error(name)
        } else {
            Exit::error(1, messages::templates::save_template_failed(name, e))
        });
    }
    if replaced {
        let _ = vfs::remove_dir_all(&old);
        // The old template image can be unused now.
        crate::oci::gc_sweep(data_dir);
    }
    Ok(())
}

/// Remove the build directory `build` after a failed build, except the
/// install log. The user can then read the log. The cached image is not
/// in use any more.
fn remove_except_log(build: &BuildDir) {
    let _ = vfs::remove_dir_all(&build.project);
    let log = crate::packs::install::setup::log_path(build.sandbox.given());
    let entries = match vfs::read_dir(&build.sandbox) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("templates: clean up {}: {e}", build.sandbox);
            return;
        }
    };
    for entry in entries.filter_map(Result::ok) {
        if Some(entry.file_name().as_os_str()) == log.file_name() {
            continue;
        }
        let Ok(path) = build.sandbox.join(entry.file_name()) else {
            continue;
        };
        let _ = match entry.file_type() {
            Ok(t) if t.is_dir() => vfs::remove_dir_all(&path),
            _ => vfs::remove_file(&path),
        };
    }
}

/// Check that the template `name` does not exist yet.
fn check_absent(data_dir: &VerifiedPath, name: &str) -> Result<(), Exit> {
    let dir = templates_dir(data_dir)
        .and_then(|root| root.join(name))
        .map_err(anyhow::Error::from)?;
    if vfs::symlink_metadata(&dir).is_ok() {
        return Err(exists_error(name));
    }
    Ok(())
}

/// The error for a template that exists.
fn exists_error(name: &str) -> Exit {
    Exit::error(1, messages::templates::template_exists(name))
}

/// Convert a failed image preparation to the end of the build.
fn image_error(e: anyhow::Error) -> Exit {
    match e.downcast_ref::<ImageChangeStop>() {
        Some(ImageChangeStop::Interrupted) => Exit::INTERRUPTED,
        Some(ImageChangeStop::Aborted) => Exit::aborted(),
        _ => Exit::Failed(e),
    }
}

/// The paths of the build directory of a template.
struct BuildDir {
    /// `<data_dir>/templates/.build-<name>`.
    root: VerifiedPath,
    /// The sandbox of the build (`<root>/sandbox`).
    sandbox: VerifiedPath,
    /// The empty project directory of the build (`<root>/project`).
    project: VerifiedPath,
}

impl BuildDir {
    /// Return the build directory of the template `name`.
    /// Returns:
    ///   The paths, or error if the path check fails.
    fn new(data_dir: &VerifiedPath, name: &str) -> anyhow::Result<Self> {
        let root = templates_dir(data_dir)?.join(format!("{BUILD_PREFIX}{name}"))?;
        Ok(Self {
            sandbox: root.join("sandbox")?,
            project: root.join("project")?,
            root,
        })
    }
}
