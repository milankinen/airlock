//! Tests of the pack approval in the config resolve: a user pack or a
//! project pack runs its `config.lua` only after the user approved the
//! script.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::config::{ConfigOverrides, LayeredConfig, ResolveInput, ResolvedConfig};
use crate::data_dir::DataDir;
use crate::packs::PackManager;
use crate::packs::approval::{
    self, ApprovalAborted, ApprovalMode, Decision, PacksNeedApproval, STORE_FILE,
};
use crate::test_cfg::config::ConfigDirs;
use crate::test_cfg::oci::data_dir_at;
use crate::test_cfg::packs::{with_user_packs, write_user_pack};
use crate::test_cfg::{TempDir, block_on, temp_dir};

/// `pack.toml` of the test packs.
const PACK_TOML: &str = "label = \"Marker\"\ndescription = \"Writes a marker\"\n";

/// Make a `config.lua` that writes the file `marker` with `text` and sets
/// the env `MARKER` to `text`. The file shows that the script ran on the
/// host.
fn marker_lua(marker: &Path, text: &str) -> String {
    format!(
        "local f = assert(io.open({path:?}, \"w\"))\nf:write({text:?})\nf:close()\n\
         config.env = {{ MARKER = {text:?} }}\n",
        path = marker.display().to_string(),
    )
}

/// A project with the project pack `packs/marker` in `airlock.toml`, and
/// a data directory.
struct Project {
    dirs: ConfigDirs,
    data: TempDir,
    data_dir: DataDir,
    /// The file that the `config.lua` of the pack writes.
    marker: PathBuf,
}

impl Project {
    /// Make the project. The `config.lua` of the pack writes `text`.
    fn new(text: &str) -> Self {
        let dirs = ConfigDirs::new();
        let data = temp_dir();
        let data_dir = data_dir_at(data.path());
        let marker = data.path().join("marker");
        dirs.project_file("airlock.toml", "[packs]\n\"./packs/marker\" = {}\n");
        let project = Self {
            dirs,
            data,
            data_dir,
            marker,
        };
        project.write_lua(text);
        project
    }

    /// Write `setup.sh` of the pack with `text`.
    fn write_setup(&self, text: &str) {
        write_user_pack(
            &self.dirs.project().join("packs"),
            "marker",
            &[("setup.sh", text)],
        );
    }

    /// The install fingerprint of the pack in `resolved`.
    fn fingerprint(resolved: &ResolvedConfig) -> String {
        resolved.packs[0].setup_installer().unwrap().fingerprint
    }

    /// Write the files of the pack. The `config.lua` writes `text`.
    fn write_lua(&self, text: &str) {
        write_user_pack(
            &self.dirs.project().join("packs"),
            "marker",
            &[
                ("pack.toml", PACK_TOML),
                ("config.lua", &marker_lua(&self.marker, text)),
            ],
        );
    }

    /// Load and resolve the config files with `mode`.
    fn resolve(&self, mode: ApprovalMode) -> anyhow::Result<ResolvedConfig> {
        resolve(
            &self.dirs.load().unwrap(),
            &self.data_dir,
            &crate::packs::init_with_sample(),
            Some(&self.dirs.project_dir()),
            mode,
        )
    }

    /// The text of the marker file, if the `config.lua` ran.
    fn marker(&self) -> Option<String> {
        std::fs::read_to_string(&self.marker).ok()
    }

    /// Remove the marker file.
    fn clear_marker(&self) {
        let _ = std::fs::remove_file(&self.marker);
    }
}

/// Resolve `layers` with `mode`.
fn resolve(
    layers: &LayeredConfig,
    data_dir: &DataDir,
    packs: &PackManager,
    project_root: Option<&crate::util::VerifiedPath>,
    mode: ApprovalMode,
) -> anyhow::Result<ResolvedConfig> {
    block_on(layers.resolve(&ResolveInput {
        data_dir,
        packs,
        overrides: &ConfigOverrides::default(),
        project_root,
        approval: mode,
    }))
}

/// Test that a new project pack does not run its `config.lua` before the
/// approval. The guest can write the project, so the script must not run
/// on the host without the user.
///   1. Resolve with `Refuse` and check the approval error with the pack id
///   2. Resolve and abort the question: check the abort error
///   3. Check that no marker file exists and no approval was saved
#[test]
fn unapproved_project_pack_does_not_run_config_lua() {
    let project = Project::new("one");

    let error = project.resolve(ApprovalMode::Refuse).err().unwrap();
    let need = error
        .downcast_ref::<PacksNeedApproval>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert_eq!(need.0, ["./packs/marker"]);

    let error = project
        .resolve(ApprovalMode::Answer(Decision::Abort))
        .err()
        .unwrap();
    assert!(error.is::<ApprovalAborted>(), "{error:#}");

    assert_eq!(project.marker(), None);
    assert!(project.resolve(ApprovalMode::Refuse).is_err());
}

/// Test that an approved pack runs, and that a change of its `config.lua`
/// needs a new approval. Approval is for the exact script.
///   1. Approve the pack and check that its `config.lua` ran
///   2. Check that the approval file has mode 0600
///   3. Resolve again with `Refuse` and check that it runs with no question
///   4. Change `config.lua` and check that `Refuse` fails and the new text
///      does not run
///   5. Approve again and check that the new text runs
#[test]
fn approved_pack_runs_and_changed_pack_needs_approval_again() {
    let project = Project::new("one");

    let resolved = project.resolve(ApprovalMode::APPROVE).unwrap();
    assert_eq!(resolved.values.env["MARKER"].value, "one");
    assert_eq!(project.marker().as_deref(), Some("one"));
    let store = project.data.path().join(STORE_FILE);
    let mode = std::fs::metadata(&store).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    project.clear_marker();
    project.resolve(ApprovalMode::Refuse).unwrap();
    assert_eq!(project.marker().as_deref(), Some("one"));

    project.clear_marker();
    project.write_lua("two");
    let error = project.resolve(ApprovalMode::Refuse).err().unwrap();
    assert!(error.is::<PacksNeedApproval>(), "{error:#}");
    assert_eq!(project.marker(), None);

    let resolved = project.resolve(ApprovalMode::APPROVE).unwrap();
    assert_eq!(resolved.values.env["MARKER"].value, "two");
    assert_eq!(project.marker().as_deref(), Some("two"));
}

/// Test that the mode `Auto` approves a new pack without a question and
/// records the approval.
///   1. Resolve with `Auto` and check that the `config.lua` ran
///   2. Resolve with `Refuse` and check that it runs with no question
#[test]
fn auto_mode_approves_and_records_new_pack() {
    let project = Project::new("one");

    let resolved = project.resolve(ApprovalMode::Auto).unwrap();
    assert_eq!(resolved.values.env["MARKER"].value, "one");
    assert_eq!(project.marker().as_deref(), Some("one"));

    project.clear_marker();
    project.resolve(ApprovalMode::Refuse).unwrap();
    assert_eq!(project.marker().as_deref(), Some("one"));
}

/// Test that the approval of a project pack is for one project. The same
/// pack path in an other project is an other pack.
///   1. Approve the pack in one project
///   2. Make a second project with the same pack, and the same data
///      directory
///   3. Check that `Refuse` fails in the second project
#[test]
fn approval_of_project_pack_does_not_cover_other_project() {
    let first = Project::new("one");
    first.resolve(ApprovalMode::APPROVE).unwrap();

    let second = Project::new("one");
    let error = resolve(
        &second.dirs.load().unwrap(),
        &first.data_dir,
        &crate::packs::init_with_sample(),
        Some(&second.dirs.project_dir()),
        ApprovalMode::Refuse,
    )
    .err()
    .unwrap();
    assert!(error.is::<PacksNeedApproval>(), "{error:#}");
}

/// Test that a change of only `setup.sh` needs no new approval, but makes
/// a new install fingerprint. Only `config.lua` runs on the host, and the
/// setup script runs in the sandbox. The sandbox must still install the
/// changed pack.
///   1. Write a pack with `config.lua` and `setup.sh`, and approve it
///   2. Change `setup.sh`
///   3. Resolve with `Refuse` and check that it resolves and `config.lua`
///      ran
///   4. Check that the install fingerprint changed
#[test]
fn setup_script_change_needs_no_approval_but_changes_fingerprint() {
    let project = Project::new("one");
    project.write_setup("echo one\n");
    let before = Project::fingerprint(&project.resolve(ApprovalMode::APPROVE).unwrap());

    project.clear_marker();
    project.write_setup("echo two\n");
    let resolved = project.resolve(ApprovalMode::Refuse).unwrap();
    assert_eq!(project.marker().as_deref(), Some("one"));
    assert_ne!(Project::fingerprint(&resolved), before);
}

/// Test that a project pack with a static config needs no approval and
/// makes no approval file. A static config only sets values, and no code
/// of the pack runs on the host.
///   1. Write a project pack with `config.toml` and `setup.sh`
///   2. Resolve with `Refuse` and check the config value of the pack
///   3. Check that no approval file exists
#[test]
fn pack_without_config_lua_needs_no_approval() {
    let dirs = ConfigDirs::new();
    dirs.project_file("airlock.toml", "[packs]\n\"./packs/static\" = {}\n");
    write_user_pack(
        &dirs.project().join("packs"),
        "static",
        &[
            ("pack.toml", PACK_TOML),
            ("config.toml", "[env]\nMARKER = \"static\"\n"),
            ("setup.sh", "echo hi\n"),
        ],
    );
    let data = temp_dir();
    let resolved = resolve(
        &dirs.load().unwrap(),
        &data_dir_at(data.path()),
        &crate::packs::init_with_sample(),
        Some(&dirs.project_dir()),
        ApprovalMode::Refuse,
    )
    .unwrap();
    assert_eq!(resolved.values.env["MARKER"].value, "static");
    assert!(!data.path().join(STORE_FILE).exists());
}

/// Test that built-in packs need no approval and make no approval file.
///   1. Resolve a config with a built-in pack with `Refuse`
///   2. Check that it resolves and that no approval file exists
#[test]
fn builtin_pack_needs_no_approval() {
    let dirs = ConfigDirs::new();
    dirs.project_file("airlock.toml", "[packs]\nsample = { version = 1 }\n");
    let data = temp_dir();
    let resolved = resolve(
        &dirs.load().unwrap(),
        &data_dir_at(data.path()),
        &crate::packs::init_with_sample(),
        Some(&dirs.project_dir()),
        ApprovalMode::Refuse,
    )
    .unwrap();
    assert_eq!(resolved.packs[0].metadata().id, "sample");
    assert!(!data.path().join(STORE_FILE).exists());
}

/// Test that a change of the pack files after the load does not change
/// what runs. The approval covers the script in memory, and airlock does
/// not read it again. The user packs load one time, before the resolve.
///   1. Load a user pack whose `config.lua` writes `one`, and approve it
///      as the setup wizard does
///   2. Change `config.lua` on the disk to write `two`
///   3. Resolve with `Refuse` with the loaded packs and check that `one`
///      ran
///   4. Load the packs again and check that the changed pack needs a new
///      approval
#[test]
fn pack_file_change_after_load_does_not_change_what_runs() {
    let tmp = temp_dir();
    let dir = tmp.path().join("packs");
    let marker = tmp.path().join("marker");
    let write = |text: &str| {
        write_user_pack(
            &dir,
            "marker",
            &[
                ("pack.toml", PACK_TOML),
                ("config.lua", &marker_lua(&marker, text)),
            ],
        );
    };
    write("one");
    let (packs, _) = with_user_packs(&[&dir], &[]).unwrap();
    let data = temp_dir();
    let data_dir = data_dir_at(data.path());
    let pack = packs
        .offered()
        .into_iter()
        .find(|p| p.metadata().id == "~marker")
        .unwrap();
    approval::approve_selected(&data_dir, &pack).unwrap();

    write("two");
    let layers = LayeredConfig::from_values(
        vec![],
        None,
        vec![(
            "airlock.toml",
            toml::from_str("[packs]\n\"~marker\" = {}\n").unwrap(),
        )],
    )
    .unwrap();
    let resolved = resolve(&layers, &data_dir, &packs, None, ApprovalMode::Refuse).unwrap();
    assert_eq!(resolved.values.env["MARKER"].value, "one");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "one");

    let (reloaded, _) = with_user_packs(&[&dir], &[]).unwrap();
    let error = resolve(&layers, &data_dir, &reloaded, None, ApprovalMode::Refuse)
        .err()
        .unwrap();
    assert!(error.is::<PacksNeedApproval>(), "{error:#}");
}
