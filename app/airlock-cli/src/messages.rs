//! Texts that the CLI shows to the user.
//!
//! Holds the status, error, prompt and output texts of all commands in one
//! place. One submodule per area of the CLI. The texts carry no marks,
//! indents or colors. The caller adds them.

pub mod app {
    //! Texts of the `app` area.

    use std::fmt::{Debug, Display};

    /// Error when `--` arguments follow a command other than `start`.
    pub const DASH_ARGS_ONLY_WITH_START: &str = "'--' args are only supported with 'start'";

    /// Error when `--` arguments follow the `exec` command.
    pub const DASH_ARGS_NOT_WITH_EXEC: &str = "'--' args are not supported with 'exec'";

    /// Error when a command fails. Shows the error with its debug format.
    pub fn command_failed(err: impl Debug) -> String {
        format!("Error: {err:?}")
    }

    /// Error when a daemon environment variable cannot be resolved.
    pub fn daemon_env_failed(name: impl Display, key: impl Display, err: impl Display) -> String {
        format!("daemons.{name}.env.{key}: {err}")
    }

    /// Verbose line with the number of enabled daemons.
    pub fn daemons_summary(count: impl Display) -> String {
        format!("daemons: {count}")
    }

    /// Verbose line with the settings of one daemon.
    pub fn daemon_details(
        name: impl Display,
        cmd: impl Display,
        restart: impl Display,
        max: impl Display,
        harden: impl Display,
    ) -> String {
        format!("{name}: {cmd} (restart={restart}, max={max}, harden={harden})")
    }

    /// Spinner text while a daemon shuts down.
    pub fn daemon_shutting_down(name: impl Display) -> String {
        format!("daemon {name}: shutting down...")
    }

    /// Error when the user presses Ctrl+C while daemons shut down.
    pub const KILLED_BY_USER: &str = "Killed by user";

    /// Status when a daemon ends. `label` tells how it ended.
    pub fn daemon_ended(name: impl Display, label: impl Display) -> String {
        format!("daemon {name}: {label}")
    }

    /// Label of a daemon that got SIGKILL.
    pub const DAEMON_KILLED: &str = "killed";

    /// Label of a daemon that stopped on its own.
    pub const DAEMON_SHUT_DOWN: &str = "shut down";

    /// Error when a mask path is empty.
    pub fn mask_path_empty(name: impl Display, idx: impl Display) -> String {
        format!("mask.{name}.paths[{idx}]: empty path")
    }

    /// Error when a mask path is absolute.
    pub fn mask_path_absolute(name: impl Display, idx: impl Display, path: impl Display) -> String {
        format!(
            "mask.{name}.paths[{idx}]: absolute paths are not allowed (got `{path}`); paths must be project-relative"
        )
    }

    /// Error when a mask path starts with `~`.
    pub fn mask_path_home_relative(
        name: impl Display,
        idx: impl Display,
        path: impl Display,
    ) -> String {
        format!(
            "mask.{name}.paths[{idx}]: home-relative paths are not allowed (got `{path}`); paths must be project-relative"
        )
    }

    /// Error when a mask path has a `..` segment.
    pub fn mask_path_dotdot(name: impl Display, idx: impl Display, path: impl Display) -> String {
        format!("mask.{name}.paths[{idx}]: `..` is not allowed in mask paths (got `{path}`)")
    }

    /// Verbose line with the number of enabled masks.
    pub fn masks_summary(count: impl Display) -> String {
        format!("masks: {count}")
    }

    /// Verbose line with the paths of one mask.
    pub fn mask_details(name: impl Display, count: impl Display, paths: impl Display) -> String {
        format!("{name}: {count} path(s): {paths}")
    }

    /// Error when an image digest has a bad format.
    pub fn invalid_image_digest(digest: impl Debug) -> String {
        format!("invalid image digest {digest:?}")
    }

    /// Error when a cache entry name is not a plain file name.
    pub fn invalid_cache_entry_name(name: impl Debug) -> String {
        format!("invalid cache entry name {name:?}")
    }

    /// Error when the home directory is unknown.
    pub const HOME_NOT_SET: &str = "HOME not set";

    /// Error when a directory cannot be created.
    pub fn cannot_create_dir(dir: impl Display, err: impl Display) -> String {
        format!("cannot create {dir}: {err}")
    }

    /// Warning when the move of the old cache directory fails. `warning` is
    /// the colored warning label.
    pub fn cache_move_failed(
        warning: impl Display,
        from: impl Display,
        to: impl Display,
        err: impl Display,
    ) -> String {
        format!("{warning} cannot move {from} to {to}: {err}. Using {from}.")
    }

    /// Error when the VM asset cache lock fails.
    #[cfg(not(test))]
    pub fn asset_lock_failed(path: impl Display, err: impl Display) -> String {
        format!("failed to lock VM asset cache {path}: {err}")
    }

    /// Error when a distroless build has no configured VM asset.
    #[cfg(not(test))]
    pub fn asset_must_be_set(name: impl Display) -> String {
        format!(
            "vm.{name} must be set in config (this is a distroless build with no bundled {name})"
        )
    }

    /// Error when a configured custom VM asset does not exist.
    pub fn custom_asset_not_found(name: impl Display, path: impl Display) -> String {
        format!("custom {name} not found: {path}")
    }
}

pub mod cli {
    //! Texts of the `cli` area.

    use std::fmt::Display;

    /// Error when the current directory is unknown. Shows for `start`, `rm` and
    /// `info`.
    pub fn cannot_determine_cwd(err: impl Display) -> String {
        format!("Cannot determine current directory: {err}")
    }

    /// Error when the sandbox lookup fails.
    pub fn sandbox_lookup_failed(err: impl Display) -> String {
        format!("{SANDBOX_LOOKUP_FAILED_CONTEXT}: {err}")
    }

    /// Message when the user aborts an action. Shows for `rm`, `secret`,
    /// `templates`, `start` and the vault prompts.
    pub const ABORTED: &str = "Aborted.";

    /// Error when the sandbox removal fails.
    pub fn failed_to_remove_sandbox(err: impl Display) -> String {
        format!("Failed to remove sandbox: {err}")
    }

    /// Status after the removal of a sandbox, or of the `.airlock` link.
    pub const SANDBOX_REMOVED: &str = "Sandbox removed";

    /// Status when `rm` finds no sandbox to remove.
    pub const NO_SANDBOX_TO_REMOVE: &str = "No sandbox to remove";

    /// Warning when `.airlock` holds user-level files, so `rm` removes only
    /// the sandbox.
    pub fn user_airlock_dir_warning(dir: impl Display, marker: impl Display) -> String {
        format!(
            "warning: {dir} holds the user-level airlock file {marker}; \
             it is a user airlock directory, so only its sandbox goes"
        )
    }

    /// Error when `rm` cannot remove the `.airlock` link.
    pub fn failed_to_remove_airlock_link(err: impl Display) -> String {
        format!("Failed to remove the .airlock link: {err}")
    }

    /// Title of the `rm` confirmation prompt.
    pub const REMOVE_SANDBOX_TITLE: &str = "Remove sandbox?";

    /// Label of the confirm choice of the `rm` prompt.
    pub const REMOVE_SANDBOX_YES: &str = "Yes, remove";

    /// Context of the vault read error.
    pub const READ_VAULT: &str = "read airlock vault";

    /// Context of the vault write error.
    pub const WRITE_VAULT: &str = "write airlock vault";

    /// Status when the vault has no secrets.
    pub const NO_SECRETS: &str = "No secrets stored. Add one with `airlock secrets add <NAME>`.";

    /// Header of the name column in the secrets table.
    pub const SECRETS_COL_NAME: &str = "NAME";

    /// Header of the value column in the secrets table.
    pub const SECRETS_COL_VALUE: &str = "VALUE";

    /// Header of the save time column in the secrets table.
    pub const SECRETS_COL_SAVED_AT: &str = "SAVED AT";

    /// Status after `secrets add` stores a secret.
    pub fn secret_stored(name: impl Display) -> String {
        format!("stored secret {name}")
    }

    /// Status after `secrets remove` removes a secret.
    pub fn secret_removed(name: impl Display) -> String {
        format!("removed secret {name}")
    }

    /// Error when no running sandbox serves a directory.
    pub fn no_running_sandbox(dir: impl Display) -> String {
        format!(
            "no running sandbox for {dir} or its parent directories. \
             is 'airlock start' running in this project?"
        )
    }

    /// Error when the CLI socket exists and no process listens on it.
    pub fn stale_cli_sock(path: impl Display) -> String {
        format!("stale cli.sock at {path} — is 'airlock start' still running?")
    }

    /// Error when the CLI socket of a sandbox cannot be connected.
    pub fn sandbox_connect_failed(path: impl Display, err: impl Display) -> String {
        format!("failed to connect to {path}: {err}")
    }

    /// Status when a command attaches to the sandbox of another project.
    pub fn attached_to_sandbox(project: impl Display) -> String {
        format!("attached to the sandbox of {project}")
    }

    /// Error when a sandbox directory or socket is a symlink.
    pub fn path_is_symlink(path: impl Display) -> String {
        format!("{path} is a symlink; remove it and run `airlock start` again")
    }

    /// Status when `secrets remove` finds no secret with this name.
    pub fn no_secret_named(name: impl Display) -> String {
        format!("No secret named {name}")
    }

    /// Error when the user interrupts the self-update.
    pub const UPDATE_INTERRUPTED: &str = "update interrupted";

    /// Error when the self-update fails.
    pub fn self_update_failed(err: impl Display) -> String {
        format!("Self-update failed: {err}")
    }

    /// Error when a development build runs `self-update`.
    pub fn dev_build_cannot_update(version: impl Display) -> String {
        format!("this is a development build ({version}). Only release builds can update")
    }

    /// Spinner text while `self-update` looks for a new release.
    pub const CHECKING_FOR_UPDATES: &str = "checking for updates";

    /// Status when the installed version is the latest.
    pub fn already_latest_version(current: impl Display) -> String {
        format!("airlock {current} is the latest version")
    }

    /// Status when a newer release exists and `--check` is set.
    pub fn update_available(latest: impl Display, current: impl Display) -> String {
        format!("airlock {latest} is available (current version: {current})")
    }

    /// Hint that tells how to install the new release.
    pub const RUN_SELF_UPDATE_HINT: &str = "Run `airlock self-update` to install it.";

    /// Error when the path of the airlock binary is unknown.
    pub fn cannot_find_binary(err: impl Display) -> String {
        format!("cannot find the airlock binary: {err}")
    }

    /// Status when `self-update` starts to install a release.
    pub fn updating_airlock(current: impl Display, latest: impl Display) -> String {
        format!("Updating airlock {current} -> {latest}")
    }

    /// Status after `self-update` installs the new release.
    pub fn airlock_installed(latest: impl Display, target: impl Display) -> String {
        format!("airlock {latest} installed to {target}")
    }

    /// Status when the registry has no sandboxes.
    pub fn no_sandboxes_in(dir: impl Display) -> String {
        format!("No sandboxes in {dir}")
    }

    /// Suffix for a project path that no longer exists.
    pub const PROJECT_MISSING_SUFFIX: &str = " (missing)";

    /// Last run column text of a sandbox that never ran.
    pub const NEVER: &str = "never";

    /// Column headers of the sandbox list.
    pub const SANDBOX_LIST_HEADER: [&str; 5] = ["ID", "STATUS", "LAST RUN", "DISK", "PROJECT"];

    /// Error when a sandbox of the given id runs.
    pub fn sandbox_id_running_stop_first(id: impl Display) -> String {
        format!("Sandbox {id} is running, stop it first")
    }

    /// Message when the user aborts the removal of a sandbox with this id.
    pub fn aborted_id(id: impl Display) -> String {
        format!("Aborted ({id}).")
    }

    /// Status after `sandboxes remove` removes a sandbox.
    pub fn sandbox_id_removed(id: impl Display) -> String {
        format!("Sandbox {id} removed")
    }

    /// Title of the confirmation prompt of `sandboxes remove`.
    pub fn remove_sandbox_id_title(id: impl Display) -> String {
        format!("Remove sandbox {id}?")
    }

    /// Note of the confirmation prompt of `sandboxes remove`.
    pub fn project_note(project: impl Display) -> String {
        format!("Project: {project}")
    }

    /// Error when `info` cannot read the config.
    pub fn config_error(err: impl Display) -> String {
        format!("Config error: {err}")
    }

    /// Error when `info` finds no sandbox for the project.
    pub fn no_sandbox_for_project(dir: impl Display) -> String {
        format!("No sandbox for {dir} — run `airlock start` first")
    }

    /// Warning of `airlock config` when the project has no project config.
    pub fn no_project_config(dir: impl Display) -> String {
        format!("No project config for {dir}")
    }

    /// Error of `airlock config --resolved` when the sandbox has no
    /// config of a last run.
    pub fn no_last_run_config(dir: impl Display) -> String {
        format!("No config of a last run for {dir} — run `airlock start` first")
    }

    /// Error when `airlock config` cannot export the config.
    pub fn config_export_failed(err: impl Display) -> String {
        format!("Cannot export config: {err}")
    }

    /// Status text of a running sandbox.
    pub const STATUS_RUNNING: &str = "running";

    /// Status text of a stopped sandbox.
    pub const STATUS_STOPPED: &str = "stopped";

    /// Status text of a sandbox without a directory.
    pub const STATUS_MISSING: &str = "missing";

    /// Line of `info` with the project path.
    pub fn info_path(path: impl Display) -> String {
        format!("Path:     {path}")
    }

    /// Line of `info` with the project path of a project that is gone.
    pub fn info_path_missing(path: impl Display) -> String {
        format!("Path:     {path} (missing)")
    }

    /// Line of `info` with the sandbox id.
    pub fn info_id(id: impl Display) -> String {
        format!("ID:       {id}")
    }

    /// Line of `info` with the sandbox status.
    pub fn info_status(status: impl Display) -> String {
        format!("Status:   {status}")
    }

    /// Line of `info` with the image.
    pub fn info_image(image: impl Display) -> String {
        format!("Image:    {image}")
    }

    /// Line of `info` with the CPU count.
    pub fn info_cpus(cpus: impl Display) -> String {
        format!("CPUs:     {cpus}")
    }

    /// Line of `info` with the memory size.
    pub fn info_memory(memory: impl Display) -> String {
        format!("Memory:   {memory}")
    }

    /// Line of `info` with the time of the last run.
    pub fn info_last_run(ago: impl Display) -> String {
        format!("Last run: {ago}")
    }

    /// Line of `info` with the sandbox directory.
    pub fn info_sandbox(dir: impl Display) -> String {
        format!("Sandbox:  {dir}")
    }

    /// Line of `info` with the disk use.
    pub fn info_disk(disk: impl Display) -> String {
        format!("Disk:     {disk}")
    }

    /// Text of the disk use: used and total size.
    pub fn disk_use(used: impl Display, size: impl Display) -> String {
        format!("{used} / {size}")
    }

    /// Header of the disk cache section of `info`.
    pub const INFO_DISK_CACHE: &str = "Disk cache:";

    /// Header of the mounts section of `info`.
    pub const INFO_MOUNTS: &str = "Mounts:";

    /// Header of the network rules section of `info`.
    pub const INFO_NETWORK_RULES: &str = "Network rules:";

    /// Header of the network middleware section of `info`.
    pub const INFO_NETWORK_MIDDLEWARE: &str = "Network middleware:";

    /// Header of the packs section of `info`.
    pub const INFO_PACKS: &str = "Packs:";

    /// Header of the services section of `info`.
    pub const INFO_SERVICES: &str = "Services:";

    /// Suffix for a disabled item in `info`.
    pub const DISABLED_SUFFIX: &str = " (disabled)";

    /// Line of `info` with the paths of one disk cache.
    pub fn info_disk_cache_entry(
        key: impl Display,
        paths: impl Display,
        suffix: impl Display,
    ) -> String {
        format!("{key}: {paths}{suffix}")
    }

    /// Line of `info` with one mount.
    pub fn info_mount(
        key: impl Display,
        source: impl Display,
        target: impl Display,
        suffix: impl Display,
    ) -> String {
        format!("{key}: {source} \u{2192} {target}{suffix}")
    }

    /// Line of `info` with the network policy.
    pub fn info_network_policy(policy: impl Display) -> String {
        format!("Network policy: {policy}")
    }

    /// Line of `info` with one network rule.
    pub fn info_network_rule(
        key: impl Display,
        allow: impl Display,
        deny: impl Display,
        suffix: impl Display,
    ) -> String {
        format!("{key}: allow {allow} deny {deny}{suffix}")
    }

    /// Line of `info` with one network middleware.
    pub fn info_network_middleware(
        key: impl Display,
        targets: impl Display,
        suffix: impl Display,
    ) -> String {
        format!("{key}: {targets} targets{suffix}")
    }

    /// Note in the services section of `info`.
    pub const SERVICES_SIGN_INS_SHARED: &str =
        "(sign-ins are the user's: all sandboxes share them)";

    /// Context of the token store read error.
    pub const CANNOT_READ_TOKEN_STORE: &str = "cannot read the token store";

    /// Line of `info` for an enabled service without a sign-in.
    pub fn service_not_signed_in(name: impl Display) -> String {
        format!("{name}: not signed in")
    }

    /// Line of `info` for an enabled service that cannot run. Gives the reason
    /// and tells that the service hosts are denied.
    pub fn service_unavailable(name: impl Display, reason: impl Display) -> String {
        format!(
            "{name}: unavailable ({reason}); its hosts are denied \
             (`[network.services] {name} = false` to sign in without airlock)"
        )
    }

    /// Account text of a stored sign-in without an account name.
    pub const UNKNOWN_ACCOUNT: &str = "unknown account";

    /// Scopes text of a stored sign-in without scopes.
    pub const NO_SCOPES: &str = "no scopes";

    /// Scopes text of a stored sign-in with many scopes.
    pub fn scope_count(count: impl Display) -> String {
        format!("{count} scopes")
    }

    /// Line of `info` for one stored sign-in.
    pub fn grant_summary(account: impl Display, scopes: impl Display, age: impl Display) -> String {
        format!("{account} ({scopes}), saved {age}")
    }

    /// Status of a pack version without a setup script.
    pub const PACK_CONFIG_ONLY: &str = "config only";

    /// Line of `info` when the sandbox directory cannot be read.
    pub fn cannot_read_sandbox_dir(why: impl Display) -> String {
        format!("(cannot read the sandbox directory: {why})")
    }

    /// Install status of an installed pack.
    pub const PACK_INSTALLED: &str = "installed";

    /// Install status of a pack whose install the user did not confirm.
    pub const PACK_INSTALL_NOT_CONFIRMED: &str = "install not confirmed";

    /// Install status of a pack whose install failed.
    pub const PACK_INSTALL_FAILED: &str = "install failed";

    /// Install status of a pack without an install record.
    pub const PACK_PENDING: &str = "pending";

    /// Status of an installed pack that is no longer in the config.
    pub const PACK_INSTALLED_REMOVED: &str = "installed, removed from config";

    /// Status of a kept pack that is no longer in the config.
    pub const PACK_KEPT_REMOVED: &str = "kept, removed from config";

    /// Context of the error when the sandbox lookup fails.
    pub const SANDBOX_LOOKUP_FAILED_CONTEXT: &str = "Sandbox lookup failed";

    /// Error when no sandbox has this id.
    pub fn no_sandbox_with_id(id: impl Display) -> String {
        format!("No sandbox {id} (see `airlock sandboxes list`)")
    }

    /// Line of the help status section when KVM access works.
    #[cfg(target_os = "linux")]
    pub const KVM_ACCESS_GRANTED: &str = "kvm access granted";

    /// Line of the help status section when KVM does not exist.
    #[cfg(target_os = "linux")]
    pub const KVM_NOT_AVAILABLE: &str = "kvm not available";

    /// Line of the help status section when KVM access is denied.
    #[cfg(target_os = "linux")]
    pub const KVM_PERMISSION_DENIED: &str = "kvm permission denied";

    /// Line of the help status section when KVM is not usable for another
    /// reason.
    #[cfg(target_os = "linux")]
    pub fn kvm_unavailable(err: impl Display) -> String {
        format!("kvm unavailable: {err}")
    }

    /// Title of the help status section.
    #[cfg(target_os = "linux")]
    pub const HELP_STATUS_TITLE: &str = "Status";

    /// Tag of a distroless build in the version string.
    pub const DISTROLESS_TAG: &str = "[distroless]";

    /// Size text in gigabytes.
    pub fn size_gb(value: impl Display) -> String {
        format!("{value:.1} GB")
    }

    /// Size text in megabytes.
    pub fn size_mb(value: impl Display) -> String {
        format!("{value:.1} MB")
    }

    /// Size text in kilobytes.
    pub fn size_kb(value: impl Display) -> String {
        format!("{value} KB")
    }

    /// Time text when the local time is not known.
    pub const UNKNOWN_TIME: &str = "----/--/-- --:--:--";

    /// Progress bar message of a layer download.
    pub const DOWNLOADING: &str = "downloading";

    /// Status when no template exists.
    pub const NO_TEMPLATES: &str =
        "No templates. Create one with `airlock templates create <name>`.";

    /// Column headers of the template list.
    pub const TEMPLATE_LIST_HEADER: [&str; 4] = ["NAME", "IMAGE", "DISK", "PACKS"];

    /// Line of `templates info` with the template name.
    pub fn template_info_name(name: impl Display) -> String {
        format!("Name:      {name}")
    }

    /// Line of `templates info` with the template directory.
    pub fn template_info_directory(dir: impl Display) -> String {
        format!("Directory: {dir}")
    }

    /// Line of `templates info` with the creation time.
    pub fn template_info_created(time: impl Display) -> String {
        format!("Created:   {time}")
    }

    /// Line of `templates info` with the image.
    pub fn template_info_image(image: impl Display) -> String {
        format!("Image:     {image}")
    }

    /// Line of `templates info` with the disk use.
    pub fn template_info_disk(used: impl Display, size: impl Display) -> String {
        format!("Disk:      {used} / {size}")
    }

    /// Line of `templates info` with the clipboard settings.
    pub fn template_info_clipboard(copy: impl Display, paste: impl Display) -> String {
        format!("Clipboard: copy {copy}, paste {paste}")
    }

    /// Text of an enabled clipboard direction.
    pub const YES: &str = "yes";

    /// Text of a disabled clipboard direction.
    pub const NO: &str = "no";

    /// Status after `templates remove` removes a template.
    pub fn template_removed(name: impl Display) -> String {
        format!("Template {name} removed")
    }

    /// Title of the confirmation prompt of `templates remove`.
    pub fn remove_template_title(name: impl Display) -> String {
        format!("Remove template {name}?")
    }

    /// Error when a prompt runs without a terminal.
    pub const PROMPT_NEEDS_TERMINAL: &str =
        "this step needs a terminal (stdin and stderr must be a TTY)";

    /// Error when the user interrupts a prompt, a question, an image download
    /// or a pack install.
    pub const PROMPT_INTERRUPTED: &str = "interrupted";

    /// Error when the terminal I/O of a prompt fails.
    pub fn prompt_failed(err: impl Display) -> String {
        format!("prompt failed: {err}")
    }

    /// Key help line of the choice prompt.
    pub const CHOOSE_KEYS: &str = "↑↓ move · enter confirm · esc abort";

    /// Label of the abort choice in a choice prompt, in the `start` questions and
    /// in the image questions.
    pub const ABORT_CHOICE: &str = "Abort";
}

pub mod config {
    //! Texts of the `config` area.

    use std::fmt::Display;

    /// Title of the error block for config problems.
    pub const INVALID_CONFIGURATION: &str = "invalid configuration";

    /// Error for config problems. Shows the title and then the problem lines.
    pub fn invalid_configuration_problems(problems: impl Display) -> String {
        format!("{INVALID_CONFIGURATION}\n{problems}")
    }

    /// Error when a user config file has a `packs` table.
    pub fn packs_only_in_project_files(origin: impl Display) -> String {
        format!(
            "`[packs]` is allowed only in project config files (airlock.toml or the local \
             project config); remove it from {origin}"
        )
    }

    /// Error when the setup wizard finds a project config file already.
    pub const PROJECT_HAS_CONFIG_ALREADY: &str = "the project has a config file already";

    /// Origin name of the config that the setup wizard made.
    pub fn setup_wizard_origin(path: impl Display) -> String {
        format!("setup wizard ({path})")
    }

    /// Detail of a config problem for a field that the config does not set.
    pub const MISSING: &str = "missing";

    /// Conflict when two packs set one config value to different values.
    pub fn packs_set_same_key(
        owner: impl Display,
        pack: impl Display,
        key: impl Display,
        base: impl Display,
        overlay: impl Display,
    ) -> String {
        format!("packs {owner} and {pack} both set `{key}` ({base} vs {overlay})")
    }

    /// Error when a local config directory outside `.airlock` resolves into
    /// the project directory.
    pub fn local_dir_in_project(local_dir: impl Display, project: impl Display) -> String {
        format!("{local_dir} is in the project directory {project}, but not in its .airlock")
    }

    /// Error when a config file appears while the setup questions are open.
    pub fn config_appeared_during_setup(path: impl Display) -> String {
        format!("{path} appeared while the setup questions were open; it was not changed")
    }

    /// Error when the setup wizard cannot create the config file.
    pub fn create_config_failed(path: impl Display, error: impl Display) -> String {
        format!("create {path}: {error}")
    }

    /// Error when a config file exists but cannot be read.
    pub fn read_config_failed(path: impl Display, error: impl Display) -> String {
        format!("read config file {path}: {error}")
    }

    /// Error when a project config file is a symlink that leaves the project.
    pub fn config_link_leaves_project(path: impl Display, project: impl Display) -> String {
        format!(
            "config file {path} is a symlink that points outside the project {project}; \
             only links inside the project are allowed"
        )
    }

    /// Error when a config file has an unknown extension.
    pub fn unsupported_config_format(path: impl Display) -> String {
        format!("unsupported config format: {path}")
    }

    /// Problem when the `packs` key is not a table.
    pub fn packs_not_table(other: impl Display, origin: impl Display) -> String {
        format!("`packs` must be a table, not {other} (set in: {origin})")
    }

    /// Hint for a pack table name that is a list-form name.
    pub fn pack_list_form_hint(name: impl Display) -> String {
        format!("; `{name}` is a list-form name: write `presets = [\"{name}\"]`")
    }

    /// Problem when a `packs` entry names a pack that does not exist.
    pub fn unknown_pack(
        name: impl Display,
        known: impl Display,
        hint: impl Display,
        origin: impl Display,
    ) -> String {
        format!("`packs.{name}` unknown pack (known: {known}){hint} (set in: {origin})")
    }

    /// Problem when a `packs` entry is not a table.
    pub fn pack_entry_not_table(name: impl Display, origin: impl Display) -> String {
        format!("`packs.{name}` must be a table (set in: {origin})")
    }

    /// Problem when `enabled` of a pack entry is not a boolean.
    pub fn pack_enabled_not_bool(name: impl Display, origin: impl Display) -> String {
        format!("`packs.{name}.enabled` must be true or false (set in: {origin})")
    }

    /// Problem when `version` of a pack entry is not valid.
    pub fn pack_version_invalid(
        name: impl Display,
        error: impl Display,
        origin: impl Display,
    ) -> String {
        format!("`packs.{name}.version` {error} (set in: {origin})")
    }

    /// Problem when `args` of a pack entry is not a table.
    pub fn pack_args_not_table(name: impl Display, origin: impl Display) -> String {
        format!("`packs.{name}.args` must be a table (set in: {origin})")
    }

    /// Problem when a pack entry has a key that is not known.
    pub fn pack_unknown_key(
        name: impl Display,
        key: impl Display,
        known_keys: impl Display,
        origin: impl Display,
    ) -> String {
        format!(
            "`packs.{name}.{key}` unknown key (known: {known_keys}; args go in \
             `args = {{ {key} = … }}`) (set in: {origin})"
        )
    }

    /// Detail of a pack `version` that is a number below 1.
    pub const VERSION_BELOW_ONE: &str = "must be 1 or higher";

    /// Detail of a pack `version` that has the wrong type.
    pub fn version_wrong_type(value: impl Display) -> String {
        format!("must be a string or a whole number, not {value}")
    }

    /// Problem when a pack entry has no `version`.
    pub fn pack_needs_version(
        name: impl Display,
        files: impl Display,
        supported: impl Display,
    ) -> String {
        format!("`packs.{name}` needs `version` (set in: {files}): {supported}")
    }

    /// Problem when a legacy pack version is not supported. Shows the list form.
    pub fn pack_version_use_list_form(
        name: impl Display,
        version: impl Display,
        released: impl Display,
        files: impl Display,
    ) -> String {
        format!(
            "`packs.{name}`: version \"{version}\" is not supported; use the list \
             form `presets = [\"{released}\"]` (set in: {files})"
        )
    }

    /// Problem when a pack version is not supported.
    pub fn pack_version_unsupported(
        name: impl Display,
        version: impl Display,
        supported: impl Display,
        files: impl Display,
    ) -> String {
        format!(
            "`packs.{name}`: version \"{version}\" is not supported (supported: {supported}) \
             (set in: {files})"
        )
    }

    /// Problem when a pack arg is not known for the pack version.
    pub fn pack_unknown_arg(
        path: impl Display,
        version: impl Display,
        known_args: impl Display,
        files: impl Display,
    ) -> String {
        format!("{path} unknown arg of version \"{version}\" ({known_args}) (set in: {files})")
    }

    /// Problem when a pack arg value is not valid.
    pub fn pack_arg_invalid(path: impl Display, error: impl Display, file: impl Display) -> String {
        format!("{path} {error} (set in: {file})")
    }

    /// Error when a preset name is not known. The hint can be empty.
    pub fn unknown_preset_in_list(
        origin: impl Display,
        name: impl Display,
        known: impl Display,
        hint: impl Display,
    ) -> String {
        format!("{origin}: unknown preset `{name}` (known: {known}){hint}")
    }

    /// Hint to use the `packs` table for a known pack name.
    pub fn preset_use_packs_table_hint(name: impl Display) -> String {
        format!(
            "; newer packs use the [packs] table of a project config file, for example \
             `[packs] {name} = {{ version = 1 }}`"
        )
    }

    /// Error when a `presets` list has an entry that is not a string.
    pub fn presets_entry_not_string(origin: impl Display, entry: impl Display) -> String {
        format!(
            "{origin}: `presets` list entries must be preset names (strings), \
             not {entry}"
        )
    }

    /// Error when `presets` is a table.
    pub fn presets_is_table(origin: impl Display) -> String {
        format!(
            "{origin}: `presets` must be a list of preset names; use a [packs] table in a \
             project config file for versioned, installable packs"
        )
    }

    /// Error when `presets` is not a list.
    pub fn presets_not_list(origin: impl Display, other: impl Display) -> String {
        format!("{origin}: `presets` must be a list of preset names, not {other}")
    }

    /// Error when preset documents depend on each other in a loop.
    pub fn circular_preset_dependency(chain: impl Display, name: impl Display) -> String {
        format!("circular preset dependency: {chain} -> {name}")
    }

    /// Error when a preset name has no built-in document.
    pub fn unknown_preset(name: impl Display) -> String {
        format!("unknown preset: `{name}`")
    }

    /// Error when a preset document is not UTF-8.
    pub fn preset_not_utf8(name: impl Display) -> String {
        format!("preset `{name}` is not UTF-8")
    }

    /// Error when a preset document is not valid.
    pub fn preset_parse_failed(name: impl Display, error: impl Display) -> String {
        format!("preset `{name}`: {error}")
    }

    /// Error when an `[env]` entry has the wrong form.
    pub const ENV_VAR_EXPECTED: &str =
        "expected a string or a table `{ value = \"...\", mask = true }`";

    /// Description of a valid port mapping value.
    pub const PORT_MAPPING_EXPECTED: &str = "a port number or \"host:guest\" string";

    /// Error when a port mapping has port 0.
    pub const PORT_MAPPING_ZERO_PORT: &str = "port 0 is not valid";

    /// Error when a port mapping has a port that is not a number.
    pub fn port_mapping_bad_port(port: impl Display, err: impl Display) -> String {
        format!("invalid port \"{port}\": {err}")
    }

    /// Error when a port mapping string has no colon.
    pub const PORT_MAPPING_NEEDS_COLON: &str = "expected \"host:guest\"";

    /// Error when a signal name is not known.
    pub fn unknown_signal(name: impl Display) -> String {
        format!(
            "unknown signal '{name}' — expected one of SIGTERM, SIGINT, SIGHUP, \
             SIGQUIT, SIGUSR1, SIGUSR2, SIGKILL"
        )
    }

    /// Error when the merged config is not a table.
    pub const CONFIG_NOT_TABLE: &str = "config must be a TOML table";

    /// Problem when `network.services` names a service that does not exist.
    pub fn unknown_service(name: impl Display, known: impl Display) -> String {
        format!("`network.services.{name}` unknown service (known: {known})")
    }

    /// Problem when a network rule pattern is not valid.
    pub fn rule_pattern_invalid(
        rule: impl Display,
        field: impl Display,
        error: impl Display,
    ) -> String {
        format!("`network.rules.{rule}.{field}` {error}")
    }

    /// Problem when a network rule has both `inject` and `passthrough`.
    pub fn rule_inject_with_passthrough(rule: impl Display) -> String {
        format!("`network.rules.{rule}` inject cannot be combined with passthrough")
    }

    /// Problem when a rule injects a variable that is not a masked `[env]` entry.
    pub fn rule_inject_not_masked(rule: impl Display, var: impl Display) -> String {
        format!("`network.rules.{rule}.inject` `{var}` must be defined in [env] with mask = true")
    }

    /// Problem when a middleware target pattern is not valid.
    pub fn middleware_target_invalid(middleware: impl Display, error: impl Display) -> String {
        format!("`network.middleware.{middleware}.target` {error}")
    }
}

pub mod db {
    //! Texts of the `db` area.

    use std::fmt::Display;

    /// The database cannot be created inside the environment.
    pub fn create_database(name: impl Display) -> String {
        format!("create the database {name}")
    }

    /// The database cannot start a transaction.
    pub const BEGIN_TRANSACTION: &str = "begin a database transaction";

    /// The database cannot commit a transaction.
    pub const COMMIT_TRANSACTION: &str = "commit a database transaction";

    /// The blocking database task fails to finish.
    pub const DATABASE_TASK: &str = "database task";

    /// The database environment cannot be opened.
    pub fn open_database(dir: impl Display) -> String {
        format!("open the database {dir}")
    }

    /// The database cannot clear stale readers.
    pub const CLEAR_STALE_READERS: &str = "clear stale database readers";

    /// The database directory cannot be created.
    pub fn create_dir(dir: impl Display) -> String {
        format!("create {dir}")
    }

    /// The database path exists but is not a directory.
    pub fn not_a_directory(dir: impl Display) -> String {
        format!("{dir} is not a directory")
    }

    /// The database directory cannot be restricted to its owner.
    pub fn restrict_dir_to_owner(dir: impl Display) -> String {
        format!("restrict {dir} to its owner")
    }
}

pub mod network {
    //! Texts of the `network` area.

    use std::fmt::Display;

    /// Context for a bad `allow` pattern of a network rule.
    pub fn rule_allow_context(rule_name: impl Display) -> String {
        format!("network.rules.{rule_name}.allow")
    }

    /// Context for a bad `deny` pattern of a network rule.
    pub fn rule_deny_context(rule_name: impl Display) -> String {
        format!("network.rules.{rule_name}.deny")
    }

    /// Context for a bad `target` pattern of a middleware.
    pub fn middleware_target_context(mw_name: impl Display) -> String {
        format!("network.middleware.{mw_name}.target")
    }

    /// Error when an inject entry has no masked variable in `[env]`.
    pub fn inject_not_masked(rule_name: impl Display, name: impl Display) -> String {
        format!(
            "network.rules.{rule_name}.inject: `{name}` must be defined in [env] with mask = true"
        )
    }

    /// Error when the port of a target pattern is not valid.
    pub fn invalid_port(target: impl Display, port: impl Display) -> String {
        format!("`{target}`: port `{port}` must be a number in 0-65535 or `*`")
    }

    /// One line of a conflict report that names two conflicting entries.
    pub fn conflicts_with(a: impl Display, b: impl Display) -> String {
        format!("{a} conflicts with {b}")
    }

    /// Error when reverse port forwards share a host port. Takes the report lines.
    pub fn reverse_forward_host_port_conflict(conflicts: impl Display) -> String {
        format!("network config: reverse port forward(s) share a host port:\n  {conflicts}")
    }

    /// Error when passthrough targets overlap intercepting targets. Takes the report lines.
    pub fn passthrough_intercept_conflict(conflicts: impl Display) -> String {
        format!(
            "network config: passthrough target(s) overlap intercepting (middleware/inject) target(s):\n  {conflicts}"
        )
    }

    /// Context when the IPv4 bind of a host → guest port forward fails.
    pub fn reverse_forward_bind_v4(host_port: impl Display) -> String {
        format!("bind 127.0.0.1:{host_port} for port forward to the sandbox")
    }

    /// Context when the IPv6 bind of a host → guest port forward fails.
    pub fn reverse_forward_bind_v6(host_port: impl Display) -> String {
        format!("bind [::1]:{host_port} for port forward to the sandbox")
    }

    /// Label of a service target in a conflict report.
    pub fn label_service(name: impl Display, host: impl Display, port: impl Display) -> String {
        format!("service `{name}` host `{host}:{port}`")
    }

    /// Label of a passthrough rule target in a conflict report.
    pub fn label_rule_passthrough(rule_name: impl Display, allow: impl Display) -> String {
        format!("rule `{rule_name}` allow=`{allow}` (passthrough)")
    }

    /// Label of a middleware target in a conflict report.
    pub fn label_middleware(mw_name: impl Display, target: impl Display) -> String {
        format!("middleware `{mw_name}` target=`{target}`")
    }

    /// Label of an inject target in a conflict report.
    pub fn label_rule_inject(rule_name: impl Display, allow: impl Display) -> String {
        format!("rule `{rule_name}` inject target=`{allow}`")
    }

    /// Label of a reverse port forward in a conflict report.
    pub fn label_ports_guest(
        group_name: impl Display,
        host: impl Display,
        guest: impl Display,
    ) -> String {
        format!("ports `{group_name}` guest=`{host}:{guest}`")
    }

    /// Context when a middleware env variable cannot be resolved.
    pub fn resolve_middleware_env(key: impl Display) -> String {
        format!("resolve middleware env.{key}")
    }

    /// Error when a middleware script does not compile.
    pub fn compile_middleware_failed(script: impl Display, err: impl Display) -> String {
        format!("failed to compile middleware '{script}': {err}")
    }

    /// Kind of a host path in the config: the host side of a socket
    /// forward.
    pub const SOCKET_KIND_HOST: &str = "host socket";

    /// Error when the host socket of a socket forward is not the socket
    /// that was there at the start or at the first connection, or has more
    /// than one hard link.
    pub fn host_socket_replaced(path: impl Display) -> String {
        format!(
            "host socket {path} is not the socket of the start, or it has more \
             than one link. Airlock refuses connections to it. If the host \
             service restarted, restart the sandbox"
        )
    }
}

pub mod network_forward {
    //! Texts of the `network forward` command and of its server side in
    //! `airlock start`.

    use std::fmt::Display;

    /// Header of the list of forwards from the host to the sandbox.
    pub const FORWARDING_TO_SANDBOX: &str = "Forwarding ports to the sandbox (Ctrl+C to stop):";

    /// Header of the list of forwards from the sandbox to the host.
    pub const FORWARDING_TO_HOST: &str = "Forwarding ports to the host (Ctrl+C to stop):";

    /// One forward from the host to the sandbox.
    pub fn to_sandbox_line(host_port: impl Display, guest_port: impl Display) -> String {
        format!("localhost:{host_port} → sandbox:{guest_port}")
    }

    /// One forward from the sandbox to the host.
    pub fn to_host_line(guest_port: impl Display, host_port: impl Display) -> String {
        format!("sandbox localhost:{guest_port} → host:{host_port}")
    }

    /// Error when the sandbox stops while the forwards are active.
    pub const SANDBOX_STOPPED: &str = "the sandbox stopped, and the port forwards closed";

    /// Error when two forwards use the same host port.
    pub fn duplicate_host_port(port: impl Display) -> String {
        format!("host port {port} is in more than one forward")
    }

    /// Error when two forwards use the same sandbox port.
    pub fn duplicate_guest_port(port: impl Display) -> String {
        format!("sandbox port {port} is in more than one forward")
    }

    /// Error when the sandbox cannot listen on a port.
    pub fn listen_failed(guest_port: impl Display, err: impl Display) -> String {
        format!("listen on sandbox port {guest_port}: {err}")
    }

    /// Warning when a connection to a sandbox port fails. Shows one time
    /// for each forward.
    pub fn connect_failed(guest_port: impl Display, err: impl Display) -> String {
        format!("connect to sandbox port {guest_port}: {err}")
    }

    /// Error when a forward from the host goes to a sandbox port that a
    /// config forward sends back to the host.
    pub fn loop_to_host(guest_port: impl Display) -> String {
        format!(
            "sandbox port {guest_port} forwards to the host in the config (`host` list); \
             a forward to it makes a loop"
        )
    }

    /// Error when a forward to the host listens on a sandbox port that a
    /// config forward from the host uses.
    pub fn loop_from_host(guest_port: impl Display) -> String {
        format!(
            "sandbox port {guest_port} gets a forward from the host in the config (`guest` list); \
             a forward from it makes a loop"
        )
    }

    /// Error when the network policy denies a connection of a forward to
    /// the host (`deny-always`, or the install network).
    pub const DENIED_BY_POLICY: &str = "the network policy denies port forwards to the host";
}

pub mod oci {
    //! Texts of the `oci` area.

    use std::fmt::{Debug, Display};

    /// Error when the image changed and the process has no terminal for the question.
    pub const IMAGE_CHANGED_NEEDS_TERMINAL: &str =
        "Sandbox image has been changed. Run in a terminal or pass --yes.";

    /// Error when the user aborts a question or an image download.
    pub const ABORTED_BY_USER: &str = "aborted by user";

    /// Status when the old image of the sandbox is not in the cache any more.
    pub const OLD_IMAGE_UNAVAILABLE: &str =
        "the old image is not available any more — using the new image";

    /// Status after airlock removes the old environment of the sandbox.
    pub const OLD_ENVIRONMENT_ERASED: &str = "old environment erased";

    /// Status when the image comes from the cache. `id` is the short image ID.
    pub fn image_cached(id: impl Display) -> String {
        format!("image cached {id}")
    }

    /// Error line when the registry refuses the credentials.
    pub const AUTH_FAILED_RETRY: &str = "authentication failed, try again";

    /// Error when the image file cannot get a hardlink in the sandbox directory.
    pub fn hardlink_image_failed(
        image_path: impl Display,
        sandbox_image: impl Display,
        err: impl Display,
    ) -> String {
        format!(
            "failed to hardlink image ref {image_path} → {sandbox_image}: {err} \
             (both paths must live on the same filesystem)"
        )
    }

    /// Error when the image has no layers.
    pub fn image_has_no_layers(image_id: impl Display) -> String {
        format!("image {image_id} has no layers")
    }

    /// Error when the image user is not in any layer `/etc/passwd`.
    pub fn user_not_found(user: impl Display) -> String {
        format!("no user {user} found in any layer /etc/passwd")
    }

    /// Error when the image group is not in any layer `/etc/group`.
    pub fn group_not_found(group: impl Display) -> String {
        format!("no group {group} found in any layer /etc/group")
    }

    /// Error when the home directory of the image user is not in any layer.
    pub fn home_dir_not_found(uid: impl Display) -> String {
        format!("no home directory found for uid {uid} in any layer /etc/passwd")
    }

    /// Question title when the image of an existing sandbox changed.
    pub const IMAGE_CHANGED_TITLE: &str = "Sandbox image has been changed";

    /// Question title when the image resolution fails.
    pub fn resolution_failed_title(err: impl Display) -> String {
        format!("Image resolution failed: {err}")
    }

    /// Choice to use the cached image after a failed resolution.
    pub const CONTINUE_WITH_CACHED_IMAGE: &str = "Continue with the cached image";

    /// Status when airlock repairs the gid of an old sandbox.
    pub fn container_gid_corrected(old_gid: impl Display, new_gid: impl Display) -> String {
        format!("container gid corrected {old_gid} → {new_gid}")
    }

    /// Error when an old sandbox runs with the wrong uid.
    pub fn legacy_uid_mismatch(
        user: impl Display,
        stored_uid: impl Display,
        stored_gid: impl Display,
        uid: impl Display,
        gid: impl Display,
    ) -> String {
        format!(
            "This sandbox was created by an airlock version that resolved the \
             image's `USER {user}` to uid {stored_uid}, gid {stored_gid} instead of uid {uid}, gid {gid}, \
             so everything inside it has been running as the wrong user. That bug \
             is fixed (https://github.com/milankinen/airlock/pull/12), but the \
             sandbox's disk already holds state owned by the wrong user, so the \
             sandbox must be re-created: run `airlock rm` and start again."
        )
    }

    /// Status when a local engine cannot use the image and airlock tries the next source.
    pub fn local_image_unusable_trying_next(reason: impl Display) -> String {
        format!("{reason} — trying next")
    }

    /// Error when the local engine does not have the image.
    pub fn image_not_found_in_engine(image_ref: impl Display, engine: impl Display) -> String {
        format!("image {image_ref} not found in {engine}")
    }

    /// Error when the registry digest is not the pinned digest.
    pub fn registry_digest_mismatch(
        image_ref: impl Display,
        digest: impl Display,
        pinned: impl Display,
    ) -> String {
        format!(
            "registry resolved {image_ref} to digest {digest}, which does not match the pinned {pinned}"
        )
    }

    /// Status when the registry resolved the image. `reference` is the short reference.
    pub fn image_resolved(reference: impl Display) -> String {
        format!("image resolved {reference}")
    }

    /// Reason when the local image does not match the pinned digest.
    pub fn local_image_pin_mismatch(
        engine: impl Display,
        image_ref: impl Display,
        pinned: impl Display,
    ) -> String {
        format!("{engine} image {image_ref} does not match the pinned digest {pinned}")
    }

    /// Reason when the local image has a different architecture than the host.
    pub fn local_image_arch_mismatch(
        engine: impl Display,
        image_arch: impl Display,
        host_arch: impl Display,
    ) -> String {
        format!("{engine} image is {image_arch}, need {host_arch}")
    }

    /// Status when a local engine resolved the image. `id` is the short image ID.
    pub fn image_resolved_via(engine: impl Display, id: impl Display) -> String {
        format!("image resolved via {engine} {id}")
    }

    /// Spinner text while airlock exports the image from a local engine.
    pub fn exporting_from(engine: impl Display) -> String {
        format!("exporting from {engine}...")
    }

    /// Error when the export of a local engine lacks a layer of the manifest.
    pub fn save_stream_missing_layer(engine: impl Display, digest: impl Display) -> String {
        format!(
            "{engine} save stream did not include blob for layer {digest} \
             (manifest referenced a layer that was not in the export)"
        )
    }

    /// Status after the export from a local engine.
    pub fn exported_from(engine: impl Display) -> String {
        format!("exported from {engine}")
    }

    /// Status when some layers of a registry image are in the cache.
    pub fn layers_found_from_cache(cached: impl Display, total: impl Display) -> String {
        format!("{cached} of {total} layers found from cache")
    }

    /// Progress bar text of a layer that is in the cache.
    pub const PROGRESS_CACHED: &str = "cached";

    /// Status after the download of layers. `summary` is the layer count and size.
    pub fn downloaded(summary: impl Display) -> String {
        format!("downloaded {summary}")
    }

    /// Summary of the downloaded layers.
    pub fn downloaded_layers(count: impl Display, size: impl Display) -> String {
        format!("{count} layers, {size}")
    }

    /// Error when the layer file is missing after the pull.
    pub const LAYER_TARBALL_MISSING_AFTER_PULL: &str =
        "unreachable: layer tarball missing after pull";

    /// Size in bytes, for a size below 1 KB.
    pub fn size_bytes(bytes: impl Display) -> String {
        format!("{bytes}B")
    }

    /// Size in KB with one decimal.
    pub fn size_kb(kb: f64) -> String {
        format!("{kb:.1}KB")
    }

    /// Size in MB with one decimal.
    pub fn size_mb(mb: f64) -> String {
        format!("{mb:.1}MB")
    }

    /// Detail of a layer file that airlock refused to read.
    pub fn layer_file_refused(
        rel_path: impl Display,
        layer_key: impl Display,
        why: impl Display,
    ) -> String {
        format!("{rel_path} in layer {layer_key}: {why}")
    }

    /// Error text when a record is not found and airlock refused some layer files.
    pub fn not_found_with_refused(not_found: impl Display, refused: impl Display) -> String {
        format!("{not_found} (ignored: {refused})")
    }

    /// Reason when a symlink in a layer file path does not resolve.
    pub fn symlink_target_unresolved(err: impl Display) -> String {
        format!("symlink target cannot be resolved in this layer: {err}")
    }

    /// Reason when a symlink in a layer file path points out of the layer.
    pub fn symlink_outside_layer(target: impl Display) -> String {
        format!("symlink resolves outside the layer ({target})")
    }

    /// Reason when a layer record is not a regular file.
    pub const NOT_REGULAR_FILE: &str = "not a regular file";

    /// Reason when a layer record file is too big.
    pub fn layer_file_too_large(len: impl Display, limit: impl Display) -> String {
        format!("{len} bytes, larger than the {limit} byte limit")
    }

    /// Error when a registry needs credentials and the process has no terminal.
    pub fn registry_requires_auth(host: impl Display) -> String {
        format!("registry {host} requires authentication")
    }

    /// Title of the registry sign-in form.
    pub fn sign_in_title(host: impl Display) -> String {
        format!("Sign in to {host}")
    }

    /// Label of the username field in the sign-in form.
    pub const USERNAME_LABEL: &str = "username";

    /// Label of the password field in the sign-in form.
    pub const PASSWORD_LABEL: &str = "password";

    /// Error text when a field of the sign-in form is empty.
    pub fn field_must_not_be_empty(label: impl Display) -> String {
        format!("{label} must not be empty")
    }

    /// Error when the user aborts the registry sign-in.
    pub fn sign_in_aborted(host: impl Display) -> String {
        format!("sign-in to {host} aborted")
    }

    /// Error when the engine cannot run the image inspect command.
    pub fn run_inspect_failed(engine: impl Display, err: impl Display) -> String {
        format!("failed to run {engine} image inspect: {err}")
    }

    /// Error when the image inspect command of the engine fails.
    pub fn inspect_failed(
        engine: impl Display,
        image_id: impl Display,
        stderr: impl Display,
    ) -> String {
        format!("{engine} image inspect {image_id} failed: {stderr}")
    }

    /// Error when a blob of the docker export has a wrong digest.
    pub fn docker_blob_digest_mismatch(named: impl Display, actual: impl Display) -> String {
        format!(
            "docker layer digest mismatch: blob named sha256:{named} \
             hashes to sha256:{actual}"
        )
    }

    /// Error when the docker export has no manifest.
    pub const NO_MANIFEST_IN_SAVE: &str = "no manifest.json in docker save output";

    /// Error when the docker export lacks the config blob.
    pub fn config_blob_missing(hex: impl Display) -> String {
        format!("config blob {hex} missing in docker save")
    }

    /// Error when the docker export has a layer with an unsupported name.
    pub fn unsupported_layer_ref(layer_ref: impl Debug) -> String {
        format!("unsupported layer {layer_ref:?} in docker save")
    }

    /// Progress bar text of a layer that is ready.
    pub const PROGRESS_READY: &str = "ready";

    /// Progress bar text of a layer in extraction.
    pub const PROGRESS_EXTRACTING: &str = "extracting";

    /// Error when airlock cannot set an extended attribute on a layer directory.
    pub fn set_xattr_failed(attr: impl Display, path: impl Display, err: impl Display) -> String {
        format!(
            "set {attr} on {path}: {err} \
             (host filesystem must support user xattrs)"
        )
    }

    /// Error when a layer has a whiteout with an unsafe name.
    pub fn unsafe_whiteout_name(path: impl Display) -> String {
        format!("refusing layer: unsafe whiteout name {path}")
    }

    /// Error when a whiteout path of a layer goes through a symlink.
    pub fn whiteout_traverses_symlink(path: impl Display) -> String {
        format!("refusing layer: whiteout path traverses a symlink at {path}")
    }

    /// Error when a whiteout path of a layer has an unsafe component.
    pub fn unsafe_whiteout_component(path: impl Display) -> String {
        format!("refusing layer: unsafe whiteout path component in {path}")
    }

    /// Error when the downloaded layer has a wrong size.
    pub fn layer_size_mismatch(expected: impl Display, actual: impl Display) -> String {
        format!("layer size mismatch: expected {expected} bytes, got {actual}")
    }

    /// Error when the downloaded layer has a wrong digest.
    pub fn layer_digest_mismatch(expected: impl Display, actual: impl Display) -> String {
        format!("layer digest mismatch: expected {expected}, got {actual}")
    }
}

pub mod packs {
    //! Texts of the `packs` area.

    use std::fmt::{Debug, Display};

    /// Arg error text shown when a pack has no args.
    pub const ARGS_NONE: &str = "it has no args";

    /// Arg error text that lists the arg keys of a pack.
    pub fn args_known(keys: impl Display) -> String {
        format!("known: {keys}")
    }

    /// Arg error text shown when a choice value is not in the allowed values.
    pub fn arg_value_invalid(value: impl Display, expected: impl Display) -> String {
        format!("`{value}` is not valid ({expected})")
    }

    /// Arg error text shown when a bool arg gets a value of the wrong type.
    pub const ARG_MUST_BE_BOOL: &str = "must be true or false";

    /// Arg error text shown when a choice arg gets a value of the wrong type.
    pub fn arg_must_be_string(expected: impl Display) -> String {
        format!("must be a string ({expected})")
    }

    /// Description of the valid values of a choice arg that accepts other values.
    pub fn arg_expected_choice_or_other(values: impl Display) -> String {
        format!("one of: {values}, or any other non-empty string")
    }

    /// Description of the valid values of a choice arg that accepts only listed values.
    pub fn arg_expected_choice(values: impl Display) -> String {
        format!("one of: {values}")
    }

    /// Error shown when the pack directory cannot be made or `config.lua` fails.
    pub fn pack_failed(name: impl Display, detail: impl Display) -> String {
        format!("pack {name}: {detail}")
    }

    /// Error shown when `config.lua` leaves `config` as a value that is not a table.
    pub fn lua_config_not_table(name: impl Display, value: impl Display) -> String {
        format!("pack {name}: config.lua must leave `config` a table, not {value}")
    }

    /// Error shown when `config.lua` sets `packs` or `presets`.
    pub fn lua_config_reserved_keys(name: impl Display) -> String {
        format!("pack {name}: config.lua cannot set `packs` or `presets`")
    }

    /// Error shown when the pack folder holds a file that is not a pack folder.
    pub fn pack_root_file_unknown(file_name: impl Display) -> String {
        format!("packs/{file_name}: unknown file (a pack is a folder <name>@<version>)")
    }

    /// Error shown when a pack folder name does not match `<name>@<version>`.
    pub fn pack_folder_name_invalid(file_name: impl Display) -> String {
        format!(
            "packs/{file_name}: the folder name must be <name>@<version> \
             (name: [a-z][a-z0-9-]*, version: a whole number from 1)"
        )
    }

    /// Error shown when two versions of a pack have different labels.
    pub fn pack_label_differs(
        at: impl Display,
        label: impl Display,
        first: impl Display,
    ) -> String {
        format!("{at}: the label \"{label}\" differs from \"{first}\" of the other versions")
    }

    /// Error shown when two versions of a pack have different kinds.
    pub fn pack_kind_differs(at: impl Display) -> String {
        format!("{at}: the kind differs from the kind of the other versions")
    }

    /// Error shown when a pack version folder holds a folder.
    pub fn pack_folder_unknown(at: impl Display) -> String {
        format!("{at}: unknown folder")
    }

    /// Error shown when a pack version folder has no `pack.toml`.
    pub fn pack_file_missing(at: impl Display) -> String {
        format!("{at}: missing")
    }

    /// Error shown when the description in `pack.toml` is empty.
    pub fn pack_description_empty(at: impl Display) -> String {
        format!("{at}: `description` is empty")
    }

    /// Error shown when a pack version folder has more than one config file.
    pub fn pack_config_files_many(folder: impl Display, files: impl Display) -> String {
        format!("packs/{folder}: more than one config file: {files}")
    }

    /// Error shown when a pack version folder holds a file that the loader does not use.
    pub fn pack_file_unknown(at: impl Display) -> String {
        format!("{at}: unknown file")
    }

    /// Prefix of the arg errors: path of the file and the arg key.
    pub fn arg_at(at: impl Display, key: impl Display) -> String {
        format!("{at}: arg `{key}`")
    }

    /// Error shown when an arg key is not valid.
    pub fn arg_key_invalid(arg_at: impl Display) -> String {
        format!(
            "{arg_at}: the key must match [a-z][a-z0-9-]* and not be `version`, `enabled` or \
             `args`"
        )
    }

    /// Error shown when two args have the same key.
    pub fn arg_key_duplicate(arg_at: impl Display) -> String {
        format!("{arg_at}: the key is used twice")
    }

    /// Error shown when a bool arg has `values` or `other`.
    pub fn arg_values_only_for_choice(arg_at: impl Display) -> String {
        format!("{arg_at}: `values` and `other` are only for a choice")
    }

    /// Error shown when a choice arg has no `values`.
    pub fn arg_choice_needs_values(arg_at: impl Display) -> String {
        format!("{arg_at}: a choice needs `values`")
    }

    /// Error shown when `values` of a choice arg is empty.
    pub fn arg_values_empty(arg_at: impl Display) -> String {
        format!("{arg_at}: `values` is empty")
    }

    /// Error shown when `values` of a choice arg has a duplicate.
    pub fn arg_value_duplicate(arg_at: impl Display, value: impl Display) -> String {
        format!("{arg_at}: the value `{value}` is in `values` twice")
    }

    /// Error shown when the default value of an arg is not valid.
    pub fn arg_default_invalid(arg_at: impl Display, reason: impl Display) -> String {
        format!("{arg_at}: default {reason}")
    }

    /// Error shown when a static pack config is not a table.
    pub fn pack_config_file_not_table(path: impl Display) -> String {
        format!("{path}: a pack config must be a table")
    }

    /// Error shown when a static pack config sets `packs` or `presets`.
    pub fn pack_config_file_reserved_keys(path: impl Display) -> String {
        format!("{path}: a pack config cannot set `packs` or `presets`")
    }

    /// Error shown when a pack file name is not UTF-8.
    pub fn pack_file_name_not_utf8(path: impl Display) -> String {
        format!("packs/{path}: the name is not UTF-8")
    }

    /// Error shown when a pack file content is not UTF-8.
    pub fn pack_file_not_utf8(path: impl Display) -> String {
        format!("packs/{path}: not UTF-8")
    }

    /// Error shown when the image does not run as root.
    pub fn image_uid_not_root(name: impl Display, uid: impl Display) -> String {
        format!(
            "image {name} runs as uid {uid}; packs install only into an image that runs as root"
        )
    }

    /// Error shown when the image has no `/etc/os-release`.
    pub fn image_no_os_release(name: impl Display) -> String {
        format!("image {name} has no /etc/os-release")
    }

    /// Error shown when the image distribution does not support packs.
    pub fn image_os_unsupported(name: impl Display, id: impl Display) -> String {
        format!("image {name} is {id}; packs install only into Alpine- and Debian-based images")
    }

    /// Reason shown when the install script prints nothing for too long.
    pub fn install_idle_timeout(minutes: impl Display) -> String {
        format!("the install printed nothing for {minutes} minutes and was stopped")
    }

    /// Reason shown when the install process cannot start.
    pub fn install_start_failed(error: impl Display) -> String {
        format!("could not start the install: {error}")
    }

    /// Progress result shown when an install did not start.
    pub const INSTALL_NOT_STARTED: &str = "not started";

    /// Progress result shown when an install script exits with an error code.
    pub fn install_failed_exit_code(code: impl Display) -> String {
        format!("failed (exit code {code})")
    }

    /// Progress result shown when an install stopped.
    pub const INSTALL_STOPPED: &str = "stopped";

    /// Error context shown when the install VM does not boot.
    pub const INSTALL_VM_BOOT: &str = "boot the install VM";

    /// Heading shown before the last log lines of a failed install.
    pub fn install_last_output(label: impl Display) -> String {
        format!("Last output of {label}:")
    }

    /// Reason shown when the install state file has version 0.
    pub const STATE_VERSION_ZERO: &str = "version 0";

    /// Reason shown when the install state file has no version.
    pub const STATE_NO_VERSION: &str = "no version";

    /// Reason shown when a pack name in the install state is not valid.
    pub fn state_pack_name_invalid(id: impl Debug) -> String {
        format!("invalid pack name {id:?}")
    }

    /// Reason shown when a fingerprint in the install state is not valid.
    pub fn state_fingerprint_invalid(id: impl Display) -> String {
        format!("invalid fingerprint of {id}")
    }

    /// Reason shown when the image ID in the install state is not valid.
    pub const STATE_IMAGE_ID_INVALID: &str = "invalid image id";

    /// Hint shown when the install script exits with code 10.
    pub const EXIT_HINT_IMAGE_UNSUPPORTED: &str =
        "the image is not Alpine- or Debian-based, or the CPU is not supported";

    /// Hint shown when the install script exits with code 11.
    pub const EXIT_HINT_PACKAGE_FAILED: &str = "a package could not be installed; see the log";

    /// Hint shown when the install script exits with code 12.
    pub const EXIT_HINT_DOWNLOAD_FAILED: &str = "a download failed; check the network (GitHub rate limits show as HTTP 403) \
         and run `airlock start` again";

    /// Hint shown when the install script exits with code 13.
    pub const EXIT_HINT_ARG_INVALID: &str =
        "an arg value of the pack is not valid; check the [packs] args";
}

pub mod project {
    //! Texts of the `project` area.

    use std::fmt::Display;

    /// Error when the sandbox disk file cannot be removed.
    pub fn remove_disk_failed(err: impl Display) -> String {
        format!("remove sandbox disk: {err}")
    }

    /// Error when the sandbox disk id file cannot be removed.
    pub fn remove_disk_id_failed(err: impl Display) -> String {
        format!("remove sandbox disk id: {err}")
    }

    /// Error when the `.airlock` directory is a symbolic link.
    pub fn airlock_dir_is_symlink(path: impl Display) -> String {
        format!(
            "{path} is a symbolic link; airlock does not follow it. Run `airlock rm` to remove \
             just the link (its target is left untouched), or replace it with a real \
             directory."
        )
    }

    /// Error when another user owns the `.airlock` directory.
    pub fn airlock_dir_foreign_owner(path: impl Display) -> String {
        format!("{path} is owned by another user")
    }

    /// Error when another airlock instance with a known process ID holds the sandbox lock.
    pub fn sandbox_in_use_by_pid(pid: impl Display) -> String {
        format!("another airlock instance (pid {pid}) is using this sandbox")
    }

    /// Error when another airlock instance holds the sandbox lock. Shows in the
    /// project lock and in `start`.
    pub const SANDBOX_IN_USE: &str = "another airlock instance is using this sandbox";

    /// Error when the sandbox lock cannot be taken.
    pub fn lock_failed(err: impl Display) -> String {
        format!("failed to lock sandbox: {err}")
    }

    /// Error when the CA file of the sandbox is missing.
    pub const CA_NOT_FOUND: &str = "CA not found — run `airlock start` first";

    /// Error text of an `[env]` entry with a problem.
    pub fn env_error(name: impl Display, reason: impl Display) -> String {
        format!("env.{name}: {reason}")
    }

    /// Reason when a network rule injects an env entry that is not masked.
    pub const ENV_INJECT_NOT_MASKED: &str =
        "must be defined in [env] with mask = true to be injected";

    /// Reason when an injected env value is too short.
    pub fn env_inject_too_short(min_len: impl Display) -> String {
        format!("injected value is shorter than {min_len} bytes")
    }

    /// Reason when an injected env value has bytes that an HTTP header does not allow.
    pub const ENV_INJECT_BAD_BYTES: &str = "injected value contains bytes that are not allowed in an HTTP header \
         (a newline or control character, perhaps a trailing newline)";
}

pub mod rpc {
    //! Texts of the `rpc` area.

    use std::fmt::Display;

    /// Warning at start when no clipboard program is on the host. The call
    /// site adds the indent and the bullet.
    pub const CLIPBOARD_DISABLED_NO_PROGRAM: &str =
        "clipboard disabled: no clipboard program found on the host";

    /// Error when a guest stdout data frame cannot be decoded.
    pub fn guest_stdout_frame_decode_failed(err: impl Display) -> String {
        format!("guest stdout frame decode failed: {err}")
    }

    /// Error when the guest sends an unknown stdout frame.
    pub fn unknown_guest_stdout_frame(err: impl Display) -> String {
        format!("unknown guest stdout frame: {err}")
    }

    /// Error when a guest stderr data frame cannot be decoded.
    pub fn guest_stderr_frame_decode_failed(err: impl Display) -> String {
        format!("guest stderr frame decode failed: {err}")
    }

    /// Error when the guest sends an unknown stderr frame.
    pub fn unknown_guest_stderr_frame(err: impl Display) -> String {
        format!("unknown guest stderr frame: {err}")
    }

    /// Error when the guest sends an unknown process output kind.
    pub fn unknown_guest_process_output(err: impl Display) -> String {
        format!("unknown guest process output: {err}")
    }

    /// Refusal when the guest URL is not valid.
    pub const URL_NOT_VALID: &str = "not a valid URL";

    /// Refusal when the URL scheme is not https.
    pub fn url_only_https(scheme: impl Display) -> String {
        format!("{scheme}: only https pages open")
    }

    /// Refusal when the URL has a user name or password.
    pub fn url_has_user_name(place: impl Display) -> String {
        format!("{place}: the URL carries a user name")
    }

    /// Refusal when the URL has an explicit port.
    pub fn url_has_port(place: impl Display) -> String {
        format!("{place}: the URL names a port")
    }

    /// Refusal when the URL has a fragment.
    pub fn url_has_fragment(place: impl Display) -> String {
        format!("{place}: the URL has a fragment")
    }

    /// Refusal when the URL is longer than the limit.
    pub fn url_too_long(place: impl Display) -> String {
        format!("{place}: the URL is too long")
    }

    /// Refusal when the URL has forbidden characters.
    pub fn url_has_forbidden_chars(place: impl Display) -> String {
        format!("{place}: the URL has characters airlock refuses")
    }

    /// Notice that shows how many open requests the notice queue dropped.
    pub fn browser_more_requests_refused(dropped: impl Display) -> String {
        format!("{dropped} more requests to open a page were refused")
    }

    /// Notice when the host has no browser program.
    pub fn browser_no_program(page: impl Display) -> String {
        format!(
            "no browser program found on this computer; open the sign-in link \
             for {page} yourself"
        )
    }

    /// Notice when the host browser program fails to start.
    pub fn browser_start_failed(err: impl Display) -> String {
        format!("could not start the browser: {err}")
    }

    /// Refusal when the URL checks fail. Adds the reason.
    pub fn browser_refused_to_open(reason: impl Display) -> String {
        format!("refused to open a page: {reason}")
    }

    /// Refusal when no grant knows the page.
    pub fn browser_page_not_known(place: impl Display) -> String {
        format!("refused to open a page: {place} is not a sign-in page airlock knows")
    }

    /// Refusal when the sandbox hits the open rate limit.
    pub const BROWSER_TOO_MANY_OPENS: &str = "the sandbox asked to open too many pages; refused";
}

pub mod runtime {
    //! Texts of the `runtime` area.

    use std::fmt::Display;

    /// Status line with the path of the PTY dump file.
    pub fn pty_dump_path(path: impl Display) -> String {
        format!("PTY dump: {path}")
    }

    /// Error line when the PTY dump file cannot open.
    pub fn pty_dump_open_failed(path: impl Display, error: impl Display) -> String {
        format!("Failed to open PTY dump {path}: {error}")
    }

    /// Error text when the monitor key bindings are not valid.
    pub fn invalid_monitor_key_bindings(error: impl Display) -> String {
        format!("invalid monitor key bindings:\n{error}")
    }

    /// Error text when the terminal launches before stdin is attached.
    pub const ATTACH_STDIN_BEFORE_LAUNCH: &str = "attach_stdin must be called before launch";

    /// Error text when the terminal launches before signals are set.
    pub const SIGNALS_BEFORE_LAUNCH: &str = "signals must be called before launch";
}

pub mod sandbox {
    //! Texts of the `sandbox` area.

    use std::fmt::Display;

    /// Error text when the process to spawn has no command.
    pub const SPAWN_EMPTY_COMMAND: &str = "spawn: empty command";

    /// Error context when the VM does not confirm that it stopped.
    pub const VM_STOP_NOT_CONFIRMED: &str = "VM stop not confirmed";

    /// Error context when the sandbox VM fails to boot.
    pub const BOOT_SANDBOX_VM: &str = "boot the sandbox VM";

    /// Error context when the main process fails to start.
    pub const START_MAIN_PROCESS: &str = "start the main process";

    /// Label before a browser notice after the main process exits.
    pub const BROWSER_NOTICE_LABEL: &str = "browser:";

    /// Error context when the VM stop fails after a boot error.
    pub fn boot_failed(error: impl Display) -> String {
        format!("boot failed: {error}")
    }

    /// Status line when the sandbox preparation starts.
    pub const PREPARING_SANDBOX: &str = "Preparing sandbox...";

    /// Status line when the config is loaded.
    pub fn config_loaded(check: impl Display, image: impl Display) -> String {
        format!("{check} config loaded, image: {image}")
    }

    /// Status line when a new CA certificate is generated.
    pub fn ca_cert_generated(check: impl Display) -> String {
        format!("{check} ca cert generated")
    }

    /// Verbose line with the number of env vars.
    pub fn env_summary(bullet: impl Display, count: impl Display, masked: impl Display) -> String {
        format!("{bullet} env: {count} vars ({masked} masked)")
    }

    /// Verbose line with the number of mounts.
    pub fn mounts_summary(bullet: impl Display, count: impl Display) -> String {
        format!("{bullet} mounts: {count}")
    }

    /// Verbose line for one mount and its target.
    pub fn mount_entry(key: impl Display, source: impl Display, target: impl Display) -> String {
        format!("{key}: {source} \u{2192} {target}")
    }

    /// Verbose line with the number of network rules and the policy.
    pub fn network_rules_summary(
        bullet: impl Display,
        count: impl Display,
        policy: impl Display,
    ) -> String {
        format!("{bullet} network rules: {count} (policy: {policy})")
    }

    /// Suffix of a network rule line that tells how many values it injects.
    pub fn rule_inject_suffix(count: impl Display) -> String {
        format!(" inject {count}")
    }

    /// Verbose line for one network rule.
    pub fn network_rule_entry(
        key: impl Display,
        allow: impl Display,
        deny: impl Display,
        inject: impl Display,
    ) -> String {
        format!("{key}: allow {allow} deny {deny}{inject}")
    }

    /// Verbose line with the number of socket forwards.
    pub fn sockets_summary(bullet: impl Display, count: impl Display) -> String {
        format!("{bullet} sockets: {count}")
    }

    /// Verbose line with the number of port forwards.
    pub fn port_forwards_summary(bullet: impl Display, count: impl Display) -> String {
        format!("{bullet} port forwards: {count}")
    }

    /// Verbose line for a port forward from the guest to the host.
    pub fn port_forward_to_host(
        key: impl Display,
        host: impl Display,
        guest: impl Display,
    ) -> String {
        format!("{key}: host :{host} \u{2190} guest :{guest}")
    }

    /// Verbose line for a port forward from the host to the guest.
    pub fn port_forward_to_guest(
        key: impl Display,
        host: impl Display,
        guest: impl Display,
    ) -> String {
        format!("{key}: host :{host} \u{2192} guest :{guest}")
    }

    /// Status line when the VM boot starts.
    pub const BOOTING_VM: &str = "Booting VM...";
}

pub mod sandboxes {
    //! Texts of the `sandboxes` area.

    use std::fmt::Display;

    /// Hint when the project `.airlock` is a symlink. The sandbox search
    /// does not go through it.
    pub fn airlock_dir_symlink_hint(dir: impl Display) -> String {
        format!("hint: {dir} is a symbolic link. Airlock does not look for a sandbox through it")
    }

    /// Error when an id is not a sandbox id. Shows for `rm` and for the project
    /// lookup.
    pub fn not_sandbox_id(id: impl Display) -> String {
        format!("not a sandbox id: {id}")
    }

    /// Error when a sandbox to remove is in use. Shows for `rm` and the sandbox
    /// removal.
    pub const SANDBOX_RUNNING_STOP_FIRST: &str = "Sandbox is running, stop it first";

    /// Error when removing a path fails.
    pub fn remove_failed(path: impl Display, err: impl Display) -> String {
        format!("remove {path}: {err}")
    }

    /// Error when a random sandbox id cannot be made.
    pub fn random_id_failed(err: impl Display) -> String {
        format!("random sandbox id: {err}")
    }

    /// Error when a path must be a directory but is not.
    pub fn not_a_directory(path: impl Display) -> String {
        format!("{path} is not a directory")
    }

    /// Error when another airlock process uses the sandbox to move.
    pub const SANDBOX_IN_USE: &str = "the sandbox is in use by another airlock process";

    /// Error when a sandbox move fails because of different file systems.
    pub fn cross_device(from: impl Display, to: impl Display) -> String {
        format!(
            "cannot move {from} to {to}: they are on different file systems. Set \
             `sandbox_type = \"project-owned\"` in the `[security]` table of \
             ~/.airlock/settings.toml to keep the sandbox in the project, or remove it with \
             `airlock rm`"
        )
    }

    /// Error when `.airlock` holds files that airlock does not know.
    pub fn extra_files(names: &[String]) -> String {
        format!(
            ".airlock directory contains extra files ({}), can't migrate - remove extra files or \
             continue using current directory",
            names.join(", ")
        )
    }

    /// Error when the sandbox directory to move does not exist.
    pub fn does_not_exist(path: impl Display) -> String {
        format!("{path} does not exist")
    }

    /// Error when moving a path fails.
    pub fn move_failed(from: impl Display, to: impl Display, err: impl Display) -> String {
        format!("move {from} to {to}: {err}")
    }
}

pub mod self_update {
    //! Texts of the `self_update` area.

    use std::fmt::Display;

    /// Error when a release tag has a wrong form.
    pub fn invalid_release_version(tag: impl Display) -> String {
        format!("invalid release version '{tag}'")
    }

    /// Error when the HTTP client cannot be made.
    pub const HTTP_CLIENT_FAILED: &str = "failed to create HTTP client";

    /// Error when the request for the latest release fails.
    pub const LATEST_RELEASE_FAILED: &str = "failed to get the latest release";

    /// Error when the latest release URL has an unknown form.
    pub fn unexpected_latest_location(url: impl Display) -> String {
        format!("unexpected latest release location {url}")
    }

    /// Error when the OS or the CPU has no release.
    pub fn no_release_for(platform: impl Display) -> String {
        format!("no airlock releases for {platform}")
    }

    /// Error when the checksum download fails.
    pub const CHECKSUM_DOWNLOAD_FAILED: &str = "failed to download the release checksum";

    /// Error when the checksum file has no checksum.
    pub const CHECKSUM_FILE_EMPTY: &str = "release checksum file is empty";

    /// Error when the archive download fails.
    pub fn archive_download_failed(archive: impl Display) -> String {
        format!("failed to download {archive}")
    }

    /// Error when the archive checksum differs from the release checksum.
    pub fn checksum_mismatch(
        archive: impl Display,
        expected: impl Display,
        actual: impl Display,
    ) -> String {
        format!("checksum mismatch in {archive}: expected {expected}, got {actual}")
    }

    /// Error when the binary cannot be extracted from the archive.
    pub fn extract_failed(archive: impl Display) -> String {
        format!("failed to extract the binary from {archive}")
    }

    /// Error when the archive has no `airlock` file.
    pub const ARCHIVE_NO_BINARY: &str = "archive has no airlock binary";

    /// Error when the target binary path has no parent directory.
    pub fn no_parent_directory(path: impl Display) -> String {
        format!("{path} has no parent directory")
    }

    /// Error when the new binary does not start.
    pub const NEW_BINARY_RUN_FAILED: &str = "failed to run the new binary";

    /// Error when the new binary reports a wrong version.
    pub fn new_binary_wrong_version(reported: impl Display, expected: impl Display) -> String {
        format!("new binary reports '{reported}', expected airlock {expected}")
    }

    /// Error when the user cannot write to the binary directory.
    pub fn no_write_access(dir: impl Display) -> String {
        format!("no write access to {dir}. Run the update as a user that can write there")
    }

    /// Error when writing the new binary fails.
    pub fn write_binary_failed(dir: impl Display) -> String {
        format!("failed to write the new binary to {dir}")
    }
}

pub mod services {
    //! Texts of the `services` area.

    use std::fmt::{Debug, Display};

    /// Warning when enabled services cannot run, with the reason and the opt-out.
    pub fn services_unavailable(
        services: impl Display,
        names: impl Display,
        are: impl Display,
        reason: impl Display,
        their: impl Display,
        agents: impl Display,
        opt_out: impl Display,
    ) -> String {
        format!(
            "the network {services} {names} {are} unavailable ({reason}), so airlock denies {their} \
             hosts; to let the {agents} sign in without airlock, set {opt_out}"
        )
    }

    /// Config setting that turns off one service, shown in the unavailable warning.
    pub fn service_opt_out_setting(service: impl Display) -> String {
        format!("`[network.services] {service} = false`")
    }

    /// Error when the vault is off. Shows when the service token store cannot
    /// open and when `info` cannot read sign-ins.
    pub const VAULT_DISABLED: &str = "the vault is disabled";

    /// Error when the token endpoint does not answer before the deadline.
    pub fn endpoint_answer_timeout(host: impl Display) -> String {
        format!("{host} did not answer in time")
    }

    /// Error context when the connection to the token endpoint fails.
    pub fn connect_to(host: impl Display, port: impl Display) -> String {
        format!("connect to {host}:{port}")
    }

    /// Error when the token endpoint host name is not valid.
    pub fn invalid_hostname(host: impl Display, error: impl Display) -> String {
        format!("invalid hostname {host}: {error}")
    }

    /// Error context when the TLS handshake with the token endpoint fails.
    pub fn tls_to(host: impl Display) -> String {
        format!("TLS to {host}")
    }

    /// Error when the body of a token endpoint answer cannot be read.
    pub fn read_token_endpoint_body(error: impl Display) -> String {
        format!("read a token endpoint body: {error}")
    }

    /// Error context when the endpoint authority is not a valid header value.
    pub const ENDPOINT_AUTHORITY: &str = "endpoint authority";

    /// Error context when the pinned request URI does not parse.
    pub const PINNED_REQUEST_URI: &str = "pinned request URI";

    /// Error context when the route URI does not parse.
    pub const ROUTE_URI: &str = "route URI";

    /// Error when a stored token cannot go into a header.
    pub const STORED_TOKEN_NOT_HEADER_VALUE: &str = "a stored token is not a valid header value";

    /// Error context when the token store cannot be read for the answer scan.
    pub const READ_STORE_FOR_ANSWER_SCAN: &str = "read the token store for the answer scan";

    /// Error context when the revoke task fails.
    pub const REVOKE_TASK: &str = "revoke task";

    /// Error context when the refresh relay task fails.
    pub const REFRESH_RELAY_TASK: &str = "refresh relay task";

    /// Error context when the sign-in callback relay fails.
    pub const SIGN_IN_CALLBACK: &str = "sign-in callback";

    /// Error context when the rewritten callback URI does not parse.
    pub const REWRITTEN_CALLBACK_URI: &str = "rewritten callback URI";

    /// Error when a token is not a JWT and so has no fake JWT.
    pub const TOKEN_NOT_JWT: &str = "the token is no JWT";

    /// Error when the body of an API answer fails to read.
    pub fn api_answer(error: impl Display) -> String {
        format!("API answer: {error}")
    }

    /// Refusal reason when the sandbox is not ready for a sign-in.
    pub const SANDBOX_NOT_READY_FOR_SIGN_IN: &str = "the sandbox is not ready for a sign-in";

    /// Message when the sign-in callback port is in use on the host.
    pub fn sign_in_port_in_use(port: impl Display) -> String {
        format!(
            "port {port} is in use on this computer, so the browser cannot \
             reach the sign-in in the sandbox"
        )
    }

    /// Message when the host cannot listen on the sign-in callback port.
    pub fn sign_in_listen_failed(port: impl Display, error: impl Display) -> String {
        format!("could not listen on port {port} for the sign-in: {error}")
    }

    /// Message when a sign-in page is not allowed to open.
    pub fn refused_to_open_page(place: impl Display, reason: impl Display) -> String {
        format!("refused to open a page: {place}: {reason}")
    }

    /// Refusal reason when a URL parameter does not appear exactly once.
    pub fn expected_one_param(name: impl Display, found: impl Display) -> String {
        format!("expected one {name}, found {found}")
    }

    /// Refusal reason when the `client_id` is empty.
    pub const CLIENT_ID_EMPTY: &str = "the client_id is empty";

    /// Refusal reason when the `response_type` is not `code`.
    pub const RESPONSE_TYPE_NOT_CODE: &str = "the response_type is not code";

    /// Refusal reason when the `code_challenge_method` is not `S256`.
    pub const CODE_CHALLENGE_METHOD_NOT_S256: &str = "the code_challenge_method is not S256";

    /// Refusal reason when the `code_challenge` is empty.
    pub const CODE_CHALLENGE_EMPTY: &str = "the code_challenge is empty";

    /// Refusal reason when the `scope` is malformed.
    pub const SCOPE_MALFORMED: &str = "the scope is malformed";

    /// Refusal reason when the `scope` names a scope twice.
    pub const SCOPE_DUPLICATE: &str = "the scope names a scope twice";

    /// Refusal reason when the URL sets a `response_mode`.
    pub const URL_SETS_RESPONSE_MODE: &str = "the URL sets a response_mode";

    /// Refusal reason when the URL asks for `prompt=none`.
    pub const URL_ASKS_PROMPT_NONE: &str = "the URL asks for prompt=none";

    /// Refusal reason when the `redirect_uri` is not a URL.
    pub const REDIRECT_URI_NOT_URL: &str = "the redirect_uri is not a URL";

    /// Refusal reason when the `redirect_uri` is not an http loopback address.
    pub const REDIRECT_URI_NOT_LOOPBACK: &str = "the redirect_uri is not an http loopback address";

    /// Refusal reason when the `redirect_uri` has a user name.
    pub const REDIRECT_URI_HAS_USER: &str = "the redirect_uri carries a user name";

    /// Refusal reason when the `redirect_uri` has a query.
    pub const REDIRECT_URI_HAS_QUERY: &str = "the redirect_uri has a query";

    /// Refusal reason when the `redirect_uri` path is wrong.
    pub fn redirect_uri_wrong_path(path: impl Display) -> String {
        format!("the redirect_uri path is not {path}")
    }

    /// Refusal reason when the `redirect_uri` has no port.
    pub const REDIRECT_URI_NO_PORT: &str = "the redirect_uri names no port";

    /// Refusal reason when the callback port is not in the allowed set.
    pub fn callback_port_not_allowed(port: impl Display) -> String {
        format!("callback port {port} is not allowed")
    }

    /// Text of the refusal for a token-like key with an unknown token format.
    pub fn token_key_unknown_format(key: impl Debug) -> String {
        format!("the value of {key:?} is in no known token format")
    }

    /// Text of the refusal for an answer with a token of a forbidden kind.
    pub fn answer_carries_token_kind(kind: impl Debug) -> String {
        format!("the answer carries a {kind:?} token")
    }

    /// Error when encrypting a store value fails.
    pub fn encrypt_key(key: impl Display) -> String {
        format!("encrypt {key}")
    }

    /// Error when a sealed store value is empty.
    pub fn store_value_empty(key: impl Display) -> String {
        format!("{key} is empty")
    }

    /// Error when a sealed store value has an unknown format version.
    pub fn store_value_unknown_version(key: impl Display, version: impl Display) -> String {
        format!("{key} has the unknown format version {version}")
    }

    /// Error when a sealed store value is too short.
    pub fn store_value_too_short(key: impl Display) -> String {
        format!("{key} is too short")
    }

    /// Error when decrypting a store value fails.
    pub fn decrypt_key(key: impl Display) -> String {
        format!("decrypt {key}: wrong key or corrupt data")
    }

    /// Error when the system random generator fails.
    pub fn random_bytes_failed(error: impl Display) -> String {
        format!("random bytes: {error}")
    }

    /// Error when the stored generation counter has a bad size.
    pub const CORRUPT_STORE_GENERATION: &str = "corrupt store generation";

    /// Error context when a store value does not parse.
    pub fn parse_key(key: impl Display) -> String {
        format!("parse {key}")
    }

    /// Error context when the grants do not serialize.
    pub const SERIALIZE_GRANTS: &str = "serialize the grants";

    /// Error when the sign-in to update no longer exists.
    pub const SIGN_IN_REMOVED: &str = "the sign-in was removed";

    /// Error context when the stored sign-ins do not parse.
    pub const PARSE_SIGN_INS: &str = "parse the sign-ins";
}

pub mod settings {
    //! Texts of the `settings` area.

    use std::fmt::Display;

    /// A monitor key binding names an action that does not exist.
    pub fn monitor_keys_unknown_action(name: impl Display) -> String {
        format!("monitor.keys.{name}: unknown action (see the manual for the list)")
    }

    /// A monitor key binding has a key string that is not valid.
    pub fn monitor_keys_invalid_key(name: impl Display, err: impl Display) -> String {
        format!("monitor.keys.{name}: {err}")
    }

    /// The home directory of the user is unknown.
    pub const HOME_DIR_MISSING: &str = "home directory missing";

    /// The `data_dir` setting is not an absolute path.
    pub fn data_dir_not_absolute(dir: impl Display) -> String {
        format!("data_dir must be an absolute path: {dir}")
    }

    /// The settings file cannot be read.
    pub fn read_settings_file(path: impl Display) -> String {
        format!("read settings file {path}")
    }

    /// The settings file cannot be loaded.
    pub fn load_settings_file(path: impl Display) -> String {
        format!("load settings file {path}")
    }

    /// The settings file has a top-level value that is not a table.
    pub const SETTINGS_NOT_TABLE: &str = "settings must be a table";

    /// Heading of the error list when the settings are not valid.
    pub const INVALID_SETTINGS: &str = "invalid settings";
}

pub mod start {
    //! Texts of the `start` area.

    use std::fmt::Display;

    /// Shows when the data directory path is too long for VM sockets.
    pub fn data_dir_too_long(
        path: impl Display,
        len: impl Display,
        max: impl Display,
        settings_path: impl Display,
    ) -> String {
        format!(
            "The data directory path {path} is too long ({len} bytes, the limit is \
             {max}). Set a shorter data directory in {settings_path}, for example: \
             data_dir = \"~/airlock-data\""
        )
    }

    /// Shows when a configuration error ends the run.
    pub fn config_error(err: impl Display) -> String {
        format!("Config error: {err:#}")
    }

    /// Shows when the sandbox is removed between lookup and lock.
    pub const SANDBOX_REMOVED_DURING_START: &str =
        "the sandbox was removed while airlock started it; run `airlock start` again";

    /// Shows when a project sandbox cannot move because no terminal is available.
    pub fn sandbox_data_visible(dir: impl Display) -> String {
        format!(
            "the sandbox data is in {dir}, where the sandbox can see it; run `airlock start` \
             in a terminal to move it"
        )
    }

    /// Choice label to move the sandbox into the data directory.
    pub const MOVE_CHOICE_YES: &str = "Yes, migrate";

    /// Choice label to keep the sandbox in the project.
    pub const MOVE_CHOICE_NO: &str = "No, keep current";

    /// Title of the question about moving the sandbox.
    pub const MOVE_TITLE: &str = "Migrate sandbox to secure directory?";

    /// Note of the question about moving the sandbox.
    pub const MOVE_NOTE: &str = "See airlock v2026.10.1 release notes for details";

    /// Shows after the sandbox moved into the data directory.
    pub fn sandbox_moved(dir: impl Display) -> String {
        format!("sandbox moved to {dir}")
    }

    /// Shows when an install fails, with the path of the install log.
    pub fn full_log(path: impl Display) -> String {
        format!("Full log: {path}")
    }

    /// Shows when the pack install starts.
    pub const INSTALLING_PACKS: &str = "Installing packs...";

    /// Shows when the install of a pack fails.
    pub fn install_failed(label: impl Display, code: impl Display) -> String {
        format!("{label} failed (exit code {code})")
    }

    /// Shows a hint for the exit code of a failed install.
    pub fn install_hint(hint: impl Display) -> String {
        format!("hint: {hint}")
    }

    /// Shows when the install VM did not confirm its disk write.
    pub const INSTALL_DISK_UNCONFIRMED: &str = "The install VM did not confirm that its disk was written; the packs \
         install again on the next start";

    /// Shows when the config changed and no question is possible.
    pub const CONFIG_CHANGED_NO_PROMPT: &str = "The sandbox configuration has changed since the last run. Run in a terminal \
         without --yes to see the changes, or pass --allow-configuration-changes.";

    /// Note of the config question when the last config does not resolve.
    pub fn last_config_invalid(err: impl Display) -> String {
        format!("The configuration of the last run is not valid any more: {err:#}")
    }

    /// Replaces a masked `[env]` value in the config diff.
    pub fn masked_value_hash(hash: impl Display) -> String {
        format!("<masked, sha256:{hash}>")
    }

    /// Title of the host path changes in the config diff.
    pub const HOST_PATHS_CHANGED: &str = "Resolved host paths:";

    /// One host path in the config diff. `resolved` is the path, or the
    /// old and the new path (see [`host_path_moved`]).
    pub fn host_path_line(
        key: impl Display,
        given: impl Display,
        resolved: impl Display,
    ) -> String {
        format!("{key} = \"{given}\": {resolved}")
    }

    /// The old and the new resolved path of a changed host path.
    pub fn host_path_moved(old: impl Display, new: impl Display) -> String {
        format!("{old} → {new}")
    }

    /// Key help of the config question while the changes are hidden.
    pub fn diff_show_hint(key: impl Display) -> String {
        format!("Press {key} to show the changes")
    }

    /// Key help of the config question while the changes show.
    pub fn diff_hide_hint(key: impl Display) -> String {
        format!("Press {key} to hide the changes")
    }

    /// Choice label to accept the config changes.
    pub const CONFIG_CHOICE_ACCEPT: &str = "Accept changes";

    /// Choice label to use the config of the last run.
    pub const CONFIG_CHOICE_LAST: &str = "Continue with old configuration";

    /// Title of the config question.
    pub const CONFIG_CHANGED_TITLE: &str = "Sandbox configuration has changed since last run:";

    /// Note of the added-tools question when the old image is gone.
    pub const OLD_IMAGE_GONE_NOTE: &str =
        "The old image is not available any more, so the tools install again for the new image.";

    /// Note of the added-tools question about internet access.
    pub const ADDED_TOOLS_NOTE_INTERNET: &str = "The install reaches the public internet without limits (local and private addresses \
         are blocked) inside the existing sandbox; code already in the sandbox can run during \
         it.";

    /// Note of the added-tools question about a clean install.
    pub const ADDED_TOOLS_NOTE_RECREATE: &str = "Re-create the sandbox for a clean install.";

    /// Part of the tool change error that lists changed packs.
    pub fn tools_changed_changed(list: impl Display) -> String {
        format!("changed: {list}")
    }

    /// Part of the tool change error that lists added tools.
    pub fn tools_changed_added(list: impl Display) -> String {
        format!("added: {list}")
    }

    /// Part of the tool change error that lists removed tools.
    pub fn tools_changed_removed(list: impl Display) -> String {
        format!("removed: {list}")
    }

    /// Shows when tools changed and nobody can answer the question.
    pub fn tools_changed(parts: impl Display) -> String {
        format!("Tools changed in the sandbox ({parts}). Run in a terminal or pass --yes.")
    }

    /// Title of the changed-packs question.
    pub fn packs_changed(labels: impl Display) -> String {
        format!("Packs have changed: {labels}")
    }

    /// Note of the changed-packs question.
    pub const PACKS_CHANGED_NOTE: &str = "Changed packs install only into a re-created sandbox.";

    /// Choice label to re-create the sandbox. Shows in the `start` and image
    /// questions.
    pub const RECREATE_SANDBOX: &str = "Re-create sandbox";

    /// Title of the removed-tools question.
    pub fn tools_removed(labels: impl Display) -> String {
        format!("Tools have been removed from sandbox: {labels}")
    }

    /// Note of the removed-tools question.
    pub const TOOLS_REMOVED_NOTE: &str = "Removed tools remain in the sandbox until recreation.";

    /// Choice label to keep the current sandbox. Shows in the `start` and image
    /// questions.
    pub const KEEP_SANDBOX: &str = "Continue with current sandbox";

    /// Title of the added-tools question.
    pub fn tools_added(labels: impl Display) -> String {
        format!("Tools have been added to sandbox: {labels}")
    }

    /// Choice label to install the added tools into the current sandbox.
    pub const INSTALL_ANYWAYS: &str = "Install anyways";

    /// Shows when the install records come from a newer airlock.
    pub fn state_too_new(path: impl Display, version: impl Display) -> String {
        format!(
            "{path} was written by a newer airlock (format {version}); update airlock, or run \
             `airlock rm` to start over"
        )
    }

    /// Shows when the install records are corrupt and the run ignores them.
    pub fn state_corrupt_ignored(path: impl Display, why: impl Display) -> String {
        format!("{path} is not valid ({why}); treating every pack as not installed")
    }

    /// Shows when the install records are corrupt and no terminal is available.
    pub fn state_corrupt(path: impl Display, why: impl Display) -> String {
        format!(
            "{path} is not valid ({why}); run `airlock start` in a terminal, or `airlock rm` \
             to start over"
        )
    }

    /// Shows when a sandbox exists but the project config is missing.
    pub fn sandbox_without_config(dir: impl Display, config_path: impl Display) -> String {
        format!(
            "A sandbox exists in {dir}, but there is no config. Put the config in airlock.toml or \
             {config_path} (restore it), or run `airlock rm` to start over."
        )
    }

    /// Shows when the project has no config and no terminal is available.
    pub fn no_config_no_terminal(dir: impl Display) -> String {
        format!(
            "No airlock config in {dir}. Run `airlock start` in a terminal, or create airlock.toml."
        )
    }

    /// Shows when the template wizard has no terminal.
    pub const TEMPLATE_NEEDS_TERMINAL: &str = "`airlock templates create` needs a terminal.";

    /// Shows after the wizard wrote a shareable config file.
    pub fn config_created(path: impl Display) -> String {
        format!("created {path}")
    }

    /// Shows when the user aborts the setup wizard.
    pub const ABORTED_NO_CONFIG: &str = "Aborted; no config written.";

    /// Shows when the user aborts the template wizard.
    pub const ABORTED_NO_TEMPLATE: &str = "Aborted; no template created.";

    /// Wizard description of the custom image row.
    pub const WIZARD_CUSTOM_DESCRIPTION: &str = "User defined image";

    /// Wizard description of a template without packs.
    pub const WIZARD_NO_PACKS_DESCRIPTION: &str = "No preinstalled packs";

    /// Wizard item of the "other" slot of a choice arg when it is not current.
    pub const WIZARD_OTHER_CHOICE: &str = "other…";

    /// Wizard section title for templates.
    pub const WIZARD_SECTION_TEMPLATE: &str = "Template";

    /// Wizard section title for the capabilities.
    pub const WIZARD_SECTION_CAPABILITIES: &str = "Capabilities";

    /// Wizard section title for the config.
    pub const WIZARD_SECTION_CONFIG: &str = "Config";

    /// Wizard section title for distro packs.
    pub const WIZARD_SECTION_BASE_IMAGE: &str = "Base image";

    /// Wizard section title for agent packs.
    pub const WIZARD_SECTION_AGENTS: &str = "Coding agents";

    /// Wizard section title for tool packs.
    pub const WIZARD_SECTION_TOOLS: &str = "Tools";

    /// Wizard key help while the start bar has focus.
    pub const WIZARD_KEYS_START_BAR: &str = "←→ choose · enter confirm";

    /// Wizard key help while the "other" slot is empty.
    pub const WIZARD_KEYS_OTHER_EMPTY: &str = "type a value · ← back";

    /// Wizard key help while the "other" slot has an error.
    pub const WIZARD_KEYS_OTHER_ERROR: &str = "edit the value · ← back";

    /// Wizard key help while the "other" slot has a value.
    pub fn wizard_keys_other(enter: impl Display) -> String {
        format!("↑↓ move · ← back · enter {enter}")
    }

    /// Wizard key help while an arg row has focus.
    pub fn wizard_keys_arg(enter: impl Display) -> String {
        format!("↑↓ move · ←→ change · enter {enter}")
    }

    /// Wizard key help while a selectable row has focus.
    pub fn wizard_keys_select(enter: impl Display) -> String {
        format!("↑↓ move · space select · enter {enter}")
    }

    /// Wizard start bar option to start the sandbox.
    pub const WIZARD_START_LABEL_START: &str = "start";

    /// Wizard start bar option to create the template.
    pub const WIZARD_START_LABEL_CREATE: &str = "create";

    /// Wizard start bar option to end the wizard.
    pub const WIZARD_START_LABEL_ABORT: &str = "abort";

    /// Wizard description of a template row, with the packs of the template.
    pub fn wizard_template_packs(list: impl Display) -> String {
        format!("packs: {list}")
    }

    /// Wizard display text of a true bool arg.
    pub const WIZARD_YES: &str = "yes";

    /// Wizard display text of a false bool arg.
    pub const WIZARD_NO: &str = "no";

    /// Wizard description of the clipboard copy row.
    pub const WIZARD_CLIPBOARD_COPY_DESCRIPTION: &str = "Sandbox can copy to the host clipboard";

    /// Wizard description of the clipboard paste row.
    pub const WIZARD_CLIPBOARD_PASTE_DESCRIPTION: &str =
        "Sandbox can read the host clipboard. Enable only if you know what you are doing";

    /// Wizard description of the shareable config row.
    pub const WIZARD_SHARE_CONFIG_DESCRIPTION: &str =
        "Write the config to the project, to share it. Otherwise it stays local";

    /// Wizard label of the empty template row.
    pub const WIZARD_TEMPLATE_EMPTY: &str = "empty";

    /// Wizard label of the custom image row.
    pub fn wizard_custom_image(image: impl Display) -> String {
        format!("custom ({image})")
    }

    /// Wizard label of the clipboard copy row.
    pub const WIZARD_CLIPBOARD_COPY: &str = "clipboard: copy";

    /// Wizard label of the clipboard paste row.
    pub const WIZARD_CLIPBOARD_PASTE: &str = "clipboard: paste";

    /// Wizard label of the shareable config row.
    pub const WIZARD_CREATE_CONFIG: &str = "Create airlock.toml";
}

pub mod templates {
    //! Texts of the `templates` area.

    use std::fmt::{Debug, Display};

    /// Shows after `airlock templates create` made the template.
    pub fn template_created(name: impl Display) -> String {
        format!("template {name} created")
    }

    /// Shows when the build of a template starts.
    pub fn building_template(name: impl Display) -> String {
        format!("Building template {name}...")
    }

    /// Shows when the wizard result has no config for the template.
    pub const NO_WIZARD_CONFIG: &str = "the template has no wizard config";

    /// Shows when the save of the template files fails.
    pub fn save_template_failed(name: impl Display, e: impl Display) -> String {
        format!("save template {name}: {e:#}")
    }

    /// Shows when the template to create exists already.
    pub fn template_exists(name: impl Display) -> String {
        format!(
            "Template {name} exists. Re-create it with `airlock templates create --force {name}`, \
             or use a different name."
        )
    }

    /// Shows when a pack entry in the template config has no version.
    pub fn pack_has_no_version(name: impl Display) -> String {
        format!("pack {name} has no version")
    }

    /// Shows when a template name is not valid.
    pub fn invalid_name(name: impl Display, max_len: impl Display) -> String {
        format!(
            "`{name}` is not a valid template name (use 1 to {max_len} letters, digits, `-`, \
             `_` or `.`, and start with a letter or a digit)"
        )
    }

    /// Shows when the named template does not exist.
    pub fn template_missing(name: impl Display) -> String {
        format!("template {name} does not exist")
    }

    /// Shows when the config file of a template cannot be read.
    pub fn read_config_failed(name: impl Display, path: impl Display, e: impl Display) -> String {
        format!("template {name}: read {path}: {e}")
    }

    /// Shows when the config of a template is not valid.
    pub fn config_invalid(name: impl Display, e: impl Display) -> String {
        format!("template {name}: {e:#}")
    }

    /// Shows when the removal of a template directory fails.
    pub fn remove_failed(path: impl Display, e: impl Display) -> String {
        format!("remove {path}: {e}")
    }

    /// Shows when the copy of the template disk fails.
    pub fn copy_disk_failed(
        name: impl Display,
        src: impl Display,
        dst: impl Display,
        e: impl Display,
    ) -> String {
        format!("template {name}: copy {src} to {dst}: {e}")
    }

    /// Shows when the install records of a template are not valid.
    pub fn install_records_invalid(name: impl Display, state: impl Debug) -> String {
        format!("template {name}: the install records are not valid ({state:?})")
    }
}

pub mod util {
    //! Texts of the `util` area.

    use std::fmt::Display;

    /// Shows when the parse of a JSON file fails.
    pub fn parse_failed(path: impl Display, e: impl Display) -> String {
        format!("parse {path}: {e}")
    }

    /// Shows when the write of a JSON file fails.
    pub fn write_failed(path: impl Display, e: impl Display) -> String {
        format!("write {path}: {e}")
    }

    /// Shows when a directory path has parts that are not plain names.
    pub fn not_plain_relative_path(path: impl Display) -> String {
        format!("not a plain relative path: {path}")
    }

    /// Shows when a file is larger than the allowed size.
    pub fn file_too_large(path: impl Display, cap: impl Display) -> String {
        format!("{path} is larger than {cap} bytes")
    }

    /// Shows when a hard link from a file descriptor is not possible.
    #[cfg(not(target_os = "linux"))]
    pub const LINK_FROM_FD_UNSUPPORTED: &str = "linkat from fd not available on this platform";

    /// Shows when a path is not a regular file.
    pub fn not_regular_file(path: impl Display) -> String {
        format!("{path} is not a regular file")
    }

    /// Shows when a path is a symlink that airlock refuses to follow.
    pub fn symlink_refused(path: impl Display) -> String {
        format!("{path} is a symlink; refusing to follow it")
    }

    /// Shows when a file operation fails, with the path of the file.
    pub fn path_error(path: impl Display, e: impl Display) -> String {
        format!("{path}: {e}")
    }

    /// Shows when a file name is empty, `.`, `..` or has a slash.
    pub fn not_file_name(name: impl Display) -> String {
        format!("not a file name: {name}")
    }

    /// Shows when a file belongs to an other user.
    pub fn owned_by_other_user(path: impl Display, uid: impl Display) -> String {
        format!("{path} is owned by uid {uid}, not by the current user")
    }

    /// Shows when airlock refuses a host path. `given` is the path that the
    /// user wrote, `resolved` its absolute form, `at` the component with the
    /// problem and `reason` the problem.
    pub fn path_refused(
        given: impl Display,
        resolved: impl Display,
        at: impl Display,
        reason: impl Display,
    ) -> String {
        format!("refusing path {given} ({resolved}): {at} {reason}")
    }

    /// Reason when a path component is a symlink.
    pub const REFUSED_SYMLINK: &str = "is a symlink";
    /// Reason when a path component that must be a directory is not one.
    pub const REFUSED_NOT_DIR: &str = "is not a directory";
    /// Reason when a file is a FIFO.
    pub const REFUSED_FIFO: &str = "is a FIFO";
    /// Reason when a directory changed after airlock checked the path.
    pub const REFUSED_REPLACED: &str = "changed after the check";
    /// Reason when a path has a `..` component.
    pub const REFUSED_DOT: &str = "has a '..' component";
    /// Reason when a path that must be relative is absolute.
    pub const REFUSED_ABSOLUTE: &str = "is not a relative path";
    /// Reason when a path is not below its allowed directory.
    pub fn refused_outside(scope: impl Display) -> String {
        format!("is not below {scope}")
    }
    /// Reason when a path is empty or has no file name.
    pub const REFUSED_NO_NAME: &str = "has no file name";
}

pub mod vault {
    //! Texts of the `vault` area.

    use std::fmt::Display;

    /// Storage cannot parse the vault blob.
    pub const PARSE_VAULT_BLOB: &str = "parse airlock vault blob — storage may be corrupt";

    /// Vault cannot serialize its data before a write.
    pub const SERIALIZE_VAULT: &str = "serialize airlock vault";

    /// Error when a command stores an empty secret value. Shows for `secret add`
    /// and for the vault.
    pub const SECRET_VALUE_EMPTY: &str = "secret value must not be empty";

    /// A command stores registry credentials for an empty host.
    pub const REGISTRY_HOST_EMPTY: &str = "registry host must not be empty";

    /// Random generator fails while the vault makes the service store key.
    pub const GENERATE_SERVICE_STORE_KEY: &str = "generate the service store key";

    /// A secret name is empty.
    pub const SECRET_NAME_EMPTY: &str = "secret name must not be empty";

    /// A secret name starts with a wrong character.
    pub fn secret_name_bad_start(first: impl Display, name: impl Display) -> String {
        format!("secret name must start with A-Z or '_', got '{first}' in \"{name}\"")
    }

    /// A secret name has a wrong character after the first one.
    pub fn secret_name_bad_char(c: impl Display, name: impl Display) -> String {
        format!("secret name must be [A-Z_][A-Z0-9_]*, got '{c}' in \"{name}\"")
    }

    /// Vault cannot read the vault file.
    pub fn read_vault_file(path: impl Display, err: impl Display) -> String {
        format!("read vault file {path}: {err}")
    }

    /// Vault cannot create the directory of the vault file.
    pub fn create_vault_directory(dir: impl Display) -> String {
        format!("create vault directory {dir}")
    }

    /// Vault cannot open the vault lock file.
    pub fn open_vault_lock(path: impl Display) -> String {
        format!("open vault lock {path}")
    }

    /// Vault cannot lock the vault lock file.
    pub fn lock_vault(path: impl Display, err: impl Display) -> String {
        format!("lock vault {path}: {err}")
    }

    /// The vault path has no file name.
    pub fn vault_path_no_file_name(path: impl Display) -> String {
        format!("vault path has no file name: {path}")
    }

    /// Vault cannot create the temporary file for an atomic write.
    pub fn create_vault_tempfile(path: impl Display) -> String {
        format!("create vault tempfile {path}")
    }

    /// Vault cannot write the temporary file for an atomic write.
    pub fn write_vault_tempfile(path: impl Display) -> String {
        format!("write vault tempfile {path}")
    }

    /// Vault cannot sync the temporary file for an atomic write.
    pub fn fsync_vault_tempfile(path: impl Display) -> String {
        format!("fsync vault tempfile {path}")
    }

    /// Vault cannot rename the temporary file to the vault file.
    pub fn rename_vault_tempfile(path: impl Display) -> String {
        format!("rename vault tempfile to {path}")
    }

    /// A stored vault value is not valid base64.
    pub fn decode_vault_value(label: impl Display) -> String {
        format!("decode vault {label}")
    }

    /// A stored vault value has the wrong length.
    pub fn vault_value_wrong_length(label: impl Display, expected: impl Display) -> String {
        format!("vault {label} has wrong length: expected {expected} bytes")
    }

    /// The Argon2 parameters are not valid.
    pub fn invalid_argon2_params(err: impl Display) -> String {
        format!("invalid argon2 params: {err}")
    }

    /// The Argon2 key derivation fails.
    pub fn argon2_kdf_failed(err: impl Display) -> String {
        format!("argon2id kdf failed: {err}")
    }

    /// The vault file cannot be parsed as an envelope.
    pub fn parse_vault_file(path: impl Display) -> String {
        format!("parse vault file {path}")
    }

    /// The vault file has a plaintext vault, but settings select encrypted storage.
    pub fn plaintext_vault_but_encrypted_storage(path: impl Display) -> String {
        format!(
            "{path} is a plaintext vault, but vault.storage = \"encrypted-file\" in settings. \
             Set vault.storage = \"file\" (or delete the file to re-create encrypted)."
        )
    }

    /// The vault file uses an unknown key derivation algorithm.
    pub fn unsupported_kdf_algo(algo: impl Display) -> String {
        format!("unsupported vault KDF algo: {algo}")
    }

    /// The vault file has key derivation parameters above the limits.
    pub fn kdf_params_out_of_bounds(m: impl Display, t: impl Display, p: impl Display) -> String {
        format!("vault KDF parameters out of bounds (m={m} t={t} p={p})")
    }

    /// The vault ciphertext is not valid base64.
    pub const DECODE_VAULT_CIPHERTEXT: &str = "decode vault ciphertext";

    /// The vault cannot decrypt.
    pub const DECRYPT_VAULT_FAILED: &str = "decrypt vault: wrong passphrase or corrupt data";

    /// The decrypted vault is not valid UTF-8.
    pub const DECRYPTED_VAULT_NOT_UTF8: &str = "decrypted vault is not valid UTF-8";

    /// The random generator fails while the vault makes a salt.
    pub const GENERATE_VAULT_SALT: &str = "generate vault salt";

    /// The random generator fails while the vault makes a nonce.
    pub const GENERATE_VAULT_NONCE: &str = "generate vault nonce";

    /// The vault cannot encrypt.
    pub fn encrypt_vault_failed(err: impl Display) -> String {
        format!("encrypt vault: {err}")
    }

    /// The vault cannot serialize the encrypted envelope.
    pub const SERIALIZE_ENCRYPTED_ENVELOPE: &str = "serialize encrypted envelope";

    /// Label of the passphrase input row when the user unlocks the vault.
    pub const PASSPHRASE_UNLOCK_LABEL: &str = "vault passphrase";

    /// The passphrase env var exists but is empty.
    pub fn passphrase_env_empty(env: impl Display) -> String {
        format!("{env} is empty")
    }

    /// The unlock prompt needs a terminal and none exists.
    pub fn no_tty_for_unlock(env: impl Display) -> String {
        format!(
            "no TTY available to prompt for the vault passphrase — set {env} \
             or run from an interactive terminal"
        )
    }

    /// Key help of the unlock prompt.
    pub const UNLOCK_KEYS: &str = "enter unlock · esc abort";

    /// The vault passphrase prompt cannot read input.
    pub const READ_VAULT_PASSPHRASE: &str = "read vault passphrase";

    /// The user aborts the vault passphrase prompt.
    pub const PASSPHRASE_PROMPT_ABORTED: &str = "vault passphrase prompt aborted";

    /// The create prompt needs a terminal and none exists.
    pub fn no_tty_for_create(env: impl Display) -> String {
        format!(
            "no TTY available to set a new vault passphrase — set {env} \
             or run from an interactive terminal"
        )
    }

    /// Title of the prompt for a new vault passphrase.
    pub const NEW_PASSPHRASE_TITLE: &str = "Set a vault passphrase";

    /// Label of the new passphrase input row.
    pub const NEW_PASSPHRASE_LABEL: &str = "new passphrase";

    /// Label of the passphrase confirmation input row.
    pub const CONFIRM_PASSPHRASE_LABEL: &str = "confirm";

    /// Key help of a text form with a next field. Shows for the vault create
    /// prompt and the registry sign-in form.
    pub const CREATE_KEYS: &str = "enter next · esc abort";

    /// The two typed passphrases differ.
    pub const PASSPHRASES_DIFFER: &str = "passphrases do not match";

    /// The user typed an empty passphrase.
    pub const PASSPHRASE_EMPTY: &str = "vault passphrase must not be empty";

    /// Message for commands that must store a secret while the vault is disabled.
    pub fn vault_disabled(settings_path: impl Display) -> String {
        format!(
            "the airlock vault is disabled. Enable it to store user \
             secrets and registry credentials.\n\n\
             Edit {settings_path} and set:\n\n  \
             vault.storage = \"keyring\"         # system keychain / Secret Service (default), or\n  \
             vault.storage = \"encrypted-file\"  # passphrase-encrypted, or\n  \
             vault.storage = \"file\"            # plain 0600 JSON\n"
        )
    }

    /// Warning that the plaintext vault backend stores secrets as cleartext.
    pub const PLAINTEXT_VAULT_WARNING: &str = concat!(
        "Your vault backend is \"file\" — secrets will be written as plaintext\n",
        "JSON to ~/.airlock/vault.default.json (mode 0600). Anyone with read access to\n",
        "that file can recover them. For stronger at-rest protection, set one of\n",
        "these in ~/.airlock/settings.toml:\n",
        "\n",
        "# RECOMMENDED: Keychain (macOS) / Secret Service (Linux)\n",
        "vault.storage = \"keyring\"\n",
        "# AEAD-encrypted, passphrase on first use (AIRLOCK_VAULT_PASSPHRASE for non-interactive)\n",
        "vault.storage = \"encrypted-file\"\n",
    );

    /// The plaintext vault needs a confirmation but the shell is not interactive.
    pub const PLAINTEXT_VAULT_NEEDS_YES: &str = "vault is \"file\" (plaintext). Re-run with --yes to confirm, or switch to \
         \"encrypted-file\" / \"keyring\" in ~/.airlock/settings.toml.";

    /// Question that asks if the user wants to use the plaintext vault.
    pub const PROCEED_PLAINTEXT_VAULT_QUESTION: &str = "Proceed with the plaintext vault?";

    /// Choice label that confirms the plaintext vault.
    pub const PROCEED_PLAINTEXT_VAULT_YES: &str = "Yes, proceed";

    /// The plaintext vault confirmation fails.
    pub const CONFIRM_PLAINTEXT_VAULT: &str = "confirm plaintext vault";

    /// The `--stdin` option is used with a terminal.
    pub const STDIN_NEEDS_PIPED_INPUT: &str =
        "--stdin requires piped input; omit it to prompt interactively";

    /// The command cannot read the secret from stdin.
    pub const READ_SECRET_FROM_STDIN: &str = "read secret from stdin";

    /// The secret prompt needs a terminal and none exists.
    pub const NO_TTY_USE_STDIN: &str = "no TTY available — use `--stdin` to pipe the value in";

    /// Key help of the secret value prompt.
    pub const SECRET_KEYS: &str = "enter save · esc abort";

    /// The secret value prompt cannot read input.
    pub const READ_SECRET_VALUE: &str = "read secret value";

    /// The keyring cannot read the vault.
    pub fn read_vault_from_keyring(err: impl Display) -> String {
        format!("read airlock vault from keyring: {err}")
    }

    /// The keyring cannot store the vault.
    pub const WRITE_VAULT_TO_KEYRING: &str = "write airlock vault to keyring";

    /// The airlock directory for the vault file is unknown.
    pub const FIND_VAULT_DIR: &str = "find the airlock directory for the vault";

    /// The airlock directory for the vault lock file is unknown.
    pub const FIND_VAULT_LOCK_DIR: &str = "find the airlock directory for the vault lock";

    /// The keyring entry cannot be made.
    pub const CONSTRUCT_KEYRING_ENTRY: &str = "construct airlock keyring entry";

    /// The vault data cannot be serialized again.
    pub const RESERIALIZE_VAULT_DATA: &str = "re-serialize vault data";

    /// The vault file has an encrypted vault, but settings select plain file storage.
    pub fn encrypted_vault_but_file_storage(path: impl Display) -> String {
        format!(
            "{path} is an encrypted vault, but vault.storage = \"file\" in settings. \
             Set vault.storage = \"encrypted-file\" (or delete the file to start fresh)."
        )
    }

    /// The vault data to store cannot be parsed.
    pub const PARSE_VAULT_DATA: &str = "parse vault data";

    /// The vault envelope cannot be serialized.
    pub const SERIALIZE_VAULT_ENVELOPE: &str = "serialize vault envelope";
}

pub mod vm {
    //! Texts of the `vm` area.

    use std::fmt::Display;

    /// Error when a file sync source path has no parent directory.
    pub const SYNC_SOURCE_NO_PARENT: &str = "source has no parent dir";

    /// Error when a file sync source path has no file name.
    pub const SYNC_SOURCE_NO_FILE_NAME: &str = "source has no file name";

    /// Error when `/dev/kvm` does not exist.
    #[cfg(target_os = "linux")]
    pub const KVM_NOT_FOUND: &str = "KVM not available (/dev/kvm not found)";

    /// Hint that follows the KVM not found error.
    #[cfg(target_os = "linux")]
    pub const KVM_ENABLE_HINT: &str = "ensure KVM is enabled in your kernel/BIOS";

    /// Error when the user cannot access `/dev/kvm`.
    #[cfg(target_os = "linux")]
    pub const KVM_NO_PERMISSION: &str = "no permission to access /dev/kvm";

    /// Hint that follows the KVM permission error.
    #[cfg(target_os = "linux")]
    pub const KVM_PERMISSION_HINT: &str = "run: sudo usermod -aG kvm $USER  (then re-login)";

    /// Error when `/dev/kvm` fails to open for another reason.
    #[cfg(target_os = "linux")]
    pub fn kvm_open_failed(err: impl Display) -> String {
        format!("cannot open /dev/kvm: {err}")
    }

    /// Error when the VM does not accept a vsock connection after all retries.
    pub fn vsock_connect_retries_failed(
        port: impl Display,
        attempts: impl Display,
        err: impl Display,
    ) -> String {
        format!("vsock connect to port {port} failed after {attempts} attempts: {err}")
    }

    /// Name of the home directory in the mount exposure check.
    pub const HOME_DIRECTORY: &str = "home directory";

    /// Name of the airlock data directory in the mount exposure check.
    pub const DATA_DIRECTORY: &str = "airlock data directory";

    /// Warning when a file mount source is on another device than the sandbox
    /// directory.
    pub fn file_mount_cross_device(source: impl Display) -> String {
        format!(
            "file mount {source}: cross-device hard link failed, falling back to copy \
             (writes inside the VM will NOT sync back to the host)"
        )
    }

    /// Error context when a file mount fails to link.
    pub fn file_mount_failed(source: impl Display) -> String {
        format!("file mount {source}")
    }

    /// VM cpu count line in the start summary.
    pub fn config_cpus(cpus: impl Display) -> String {
        format!("cpus:   {cpus}")
    }

    /// VM memory size line in the start summary.
    pub fn config_memory(memory: impl Display) -> String {
        format!("memory: {memory}")
    }

    /// VM disk size line in the start summary.
    pub fn config_disk(disk: impl Display) -> String {
        format!("disk:   {disk}")
    }

    /// Error when the platform has no VM backend.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub const UNSUPPORTED_PLATFORM: &str = "unsupported platform";

    /// Error when the mount source is missing and the mount must exist.
    pub fn mount_source_missing(source: impl Display) -> String {
        format!("mount source does not exist: {source}")
    }

    /// Status line when a missing mount source is skipped.
    pub fn mount_skipped_not_found(source: impl Display) -> String {
        format!("mount skipped (not found): {source}")
    }

    /// Error when a mount mode is not an octal number.
    pub fn invalid_octal_mode(mode: impl std::fmt::Debug) -> String {
        format!("invalid octal mode: {mode:?}")
    }

    /// Kind of the exposing mount when it is the project directory.
    pub const MOUNT_KIND_PROJECT: &str = "project directory";

    /// Kind of the exposing mount when it is a user mount source.
    pub const MOUNT_KIND_SOURCE: &str = "mount source";

    /// Kind of a host path in the config: the custom VM kernel or
    /// initramfs.
    pub fn custom_asset_kind(name: impl Display) -> String {
        format!("custom {name}")
    }

    /// Error when the given path of a host path is not absolute.
    pub fn path_not_absolute(path: impl Display) -> String {
        format!("host path is not absolute: {path}")
    }

    /// Error when a host path in the config goes into a directory that the
    /// sandbox can write, and a symlink there points out of that directory.
    pub fn path_leaves_writable_dir(
        kind: impl Display,
        given: impl Display,
        dir: impl Display,
        resolved: impl Display,
    ) -> String {
        format!(
            "the {kind} {given} goes through {dir}, which the sandbox can write, to \
             {resolved} outside of it. The sandbox can change the symlinks there.\n\
             Set the real path {resolved} in the config."
        )
    }

    /// Error when a mount exposes a protected host directory.
    pub fn mount_exposes_directory(
        kind: impl Display,
        source: impl Display,
        name: impl Display,
        dir: impl Display,
    ) -> String {
        format!(
            "the {kind} {source} is or contains the {name} {dir}. The sandbox could read all \
             secrets in it.\nUse a directory below it, or set `insecure_mounts = true` \
             in the `[security]` table of ~/.airlock/settings.toml."
        )
    }

    /// Status line after the disk is recreated at a smaller size.
    pub fn disk_recreated(size: impl Display) -> String {
        format!("disk recreated {size} (previous data erased)")
    }

    /// Status line when the user keeps the larger disk.
    pub fn disk_kept(current: impl Display, configured: impl Display) -> String {
        format!("disk kept at {current} (configured {configured} is smaller; data preserved)")
    }

    /// Status line after the disk grows.
    pub fn disk_grown(size: impl Display) -> String {
        format!("disk grown to {size}")
    }

    /// Status line after the disk of a template is in the new sandbox.
    pub fn disk_copied(size: impl Display) -> String {
        format!("disk copied from template {size}")
    }

    /// Status line after a new disk is created.
    pub fn disk_created(size: impl Display) -> String {
        format!("disk created {size}")
    }

    /// Disk size in whole gigabytes.
    pub fn size_gb(gb: impl Display) -> String {
        format!("{gb} GB")
    }

    /// Disk size in whole megabytes.
    pub fn size_mb(mb: impl Display) -> String {
        format!("{mb} MB")
    }

    /// Title of the prompt that asks to shrink the disk.
    pub fn disk_shrink_title(target: impl Display, current: impl Display) -> String {
        format!("Configured disk size {target} is smaller than the current {current}")
    }

    /// Label of the prompt choice that erases and recreates the disk.
    pub fn disk_shrink_erase(target: impl Display) -> String {
        format!("Erase and recreate at {target}")
    }

    /// Note in the prompt that asks to shrink the disk.
    pub const DISK_SHRINK_NOTE: &str = "Shrinking erases all sandbox data on the disk.";

    /// Label of the prompt choice that keeps the current disk.
    pub const DISK_SHRINK_KEEP: &str = "Keep the current disk";

    /// Error when the system fails to give random bytes for the disk id.
    pub fn disk_random_id_failed(err: impl Display) -> String {
        format!("random disk id: {err}")
    }

    /// Error when virtiofsd does not start.
    #[cfg(target_os = "linux")]
    pub fn virtiofsd_start_failed(tag: impl Display, err: impl Display) -> String {
        format!("failed to start virtiofsd for {tag}: {err}")
    }

    /// Error when cloud-hypervisor does not start.
    #[cfg(target_os = "linux")]
    pub fn cloud_hypervisor_start_failed(err: impl Display) -> String {
        format!("failed to start cloud-hypervisor: {err}")
    }

    /// Error when the vsock socket of cloud-hypervisor does not open.
    #[cfg(target_os = "linux")]
    pub fn vsock_socket_connect_failed(path: impl Display, err: impl Display) -> String {
        format!("vsock connect to {path}: {err}")
    }

    /// Error when the vsock `CONNECT` handshake gets a bad response.
    #[cfg(target_os = "linux")]
    pub fn vsock_handshake_failed(response: impl Display) -> String {
        format!("vsock CONNECT failed: {response}")
    }

    /// Error when waiting for a child process fails.
    #[cfg(target_os = "linux")]
    pub fn child_wait_failed(name: impl Display, err: impl Display) -> String {
        format!("{name} wait: {err}")
    }

    /// Error when the virtiofsd socket does not appear in time.
    #[cfg(target_os = "linux")]
    pub fn virtiofsd_socket_timeout(tag: impl Display) -> String {
        format!("virtiofsd socket not ready after 5s: {tag}")
    }

    /// Error text for an Objective-C exception with a value.
    #[cfg(target_os = "macos")]
    pub fn objc_exception(exception: impl std::fmt::Debug) -> String {
        format!("ObjC exception: {exception:?}")
    }

    /// Error text for an Objective-C exception without a value.
    #[cfg(target_os = "macos")]
    pub const OBJC_EXCEPTION_NIL: &str = "ObjC exception (nil)";

    /// Error when the VM configuration is not valid.
    #[cfg(target_os = "macos")]
    pub fn vm_config_invalid(err: impl Display) -> String {
        format!("VM configuration validation failed: {err}")
    }

    /// Error when nested virtualization needs a newer macOS.
    #[cfg(target_os = "macos")]
    pub const KVM_NEEDS_MACOS_15: &str = "vm.kvm requires macOS 15 or later";

    /// Error when the Mac has no nested virtualization.
    #[cfg(target_os = "macos")]
    pub const KVM_NEEDS_NESTED_VIRT: &str =
        "vm.kvm requires nested virtualization support on this Mac (M3 or later)";

    /// Error when the cache disk does not attach.
    #[cfg(target_os = "macos")]
    pub fn cache_disk_attach_failed(err: impl Display) -> String {
        format!("cache disk attachment: {err}")
    }

    /// Error when the VM object is gone before start.
    #[cfg(target_os = "macos")]
    pub const VM_DROPPED_BEFORE_START: &str = "VM dropped before start";

    /// Error when the VM start result channel closes.
    #[cfg(target_os = "macos")]
    pub const VM_START_CHANNEL_CLOSED: &str = "VM start channel closed";

    /// Error when the VM does not start.
    #[cfg(target_os = "macos")]
    pub fn vm_start_failed(err: impl Display) -> String {
        format!("VM start failed: {err}")
    }

    /// Error when the VM object is gone before stop.
    #[cfg(target_os = "macos")]
    pub const VM_DROPPED_BEFORE_STOP: &str = "VM dropped before stop";

    /// Error when the VM stop result channel closes.
    #[cfg(target_os = "macos")]
    pub const VM_STOP_CHANNEL_CLOSED: &str = "VM stop channel closed";

    /// Error when the VM does not stop.
    #[cfg(target_os = "macos")]
    pub fn vm_stop_failed(err: impl Display) -> String {
        format!("VM stop failed: {err}")
    }

    /// Error when the VM object is gone before a vsock connect.
    #[cfg(target_os = "macos")]
    pub const VM_DROPPED: &str = "VM dropped";

    /// Error when the VM has no vsock device.
    #[cfg(target_os = "macos")]
    pub const NO_VSOCK_DEVICE: &str = "no vsock device";

    /// Error when the vsock file descriptor does not duplicate.
    #[cfg(target_os = "macos")]
    pub const VSOCK_DUP_FAILED: &str = "failed to dup vsock fd";

    /// Error when the vsock connect gives no connection and no error.
    #[cfg(target_os = "macos")]
    pub const VSOCK_CONNECT_NULL: &str = "vsock connect returned null";

    /// Error when the vsock connect result channel closes.
    #[cfg(target_os = "macos")]
    pub const VSOCK_CHANNEL_CLOSED: &str = "vsock connect channel closed";

    /// Error when the vsock connect fails.
    #[cfg(target_os = "macos")]
    pub fn vsock_connect_failed(err: impl Display) -> String {
        format!("vsock connect failed: {err}")
    }
}
