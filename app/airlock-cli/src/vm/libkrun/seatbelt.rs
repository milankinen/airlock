//! Seatbelt sandbox for the libkrun VM process.
//!
//! libkrun has no sandbox of its own, and its shared directory server does
//! not keep the guest in the shared directories. This module locks the VM
//! process to the files and sockets that the VM needs, so that a guest
//! that escapes a share still cannot get to other host files.
//!
//! The policy denies all operations by default. It allows only:
//!  * read access to the read-only shares and files
//!  * read and write access to the read-write shares and files (for
//!    example the disk image)
//!  * bind and accept on the vsock sockets. These sockets are the only
//!    connection between the guest and airlock. The VM process gets no
//!    other network access.
//!  * a small set of system values that the VM threads read
//!
//! The sandbox applies to the full process and cannot be removed. Apply it
//! after the VM process loaded all libraries and before the guest runs.
//!
//! Host paths go to the policy only as parameters, never as policy text, so
//! that a path cannot change the policy.

use std::ffi::{CStr, CString, c_char, c_int};

use crate::util::VerifiedPath;

/// Host paths that the VM process can use after the sandbox applies. All
/// paths must be absolute.
#[derive(Debug, Default, Clone)]
pub struct Policy {
    /// Directories with read access to all of their contents.
    pub read_only: Vec<String>,
    /// Directories with read and write access to all of their contents.
    pub read_write: Vec<String>,
    /// Files with read access, for example the kernel if libkrun opens it
    /// after the sandbox applies.
    pub ro_files: Vec<String>,
    /// Files with read and write access, for example the disk image.
    pub rw_files: Vec<String>,
    /// Unix sockets that the VM process makes and accepts connections on.
    /// The parent directory must exist. The socket itself can be missing.
    pub unix_sockets: Vec<String>,
}

/// Seatbelt `sandbox_init_with_parameters` flag value for policy text. The
/// other flag (`SANDBOX_NAMED`) selects a built-in policy by name.
const PROFILE_TEXT: u64 = 0;

/// System values (sysctl) that the VM process can read. Each name has a
/// reason. A missing name gives a deny in the system log and an error in
/// the caller, not a crash.
// TODO(verify on macOS): get the real list from the deny lines in the
// system log after a boot and a long session. Remove names that nothing
// reads.
const SYSCTL_NAMES: [&str; 8] = [
    // CPU count. The Rust runtime and libkrun use it for thread counts.
    "hw.ncpu",
    "hw.activecpu",
    "hw.logicalcpu",
    // Page size. libkrun aligns guest memory and free page reports to it.
    "hw.pagesize",
    "hw.pagesize_compat",
    // Cache line size. Memory copy code can read it.
    "hw.cachelinesize",
    // Hypervisor support. The hypervisor library can read it at VM start.
    "kern.hv_support",
    // OS version. libkrun and the hypervisor library can select features
    // by the version.
    "kern.osproductversion",
];

/// System value prefixes that the VM process can read. The Rust runtime
/// reads the CPU feature flags (`hw.optional.arm.FEAT_*`) to select fast
/// code paths.
const SYSCTL_PREFIXES: [&str; 1] = ["hw.optional."];

// The functions are in libSystem (`libsystem_sandbox`), which each macOS
// program links, so no link attribute is necessary. Chromium declares the
// same functions in the same way. Apple marks the API as deprecated, but
// Chromium, WebKit and the system services still use it.
unsafe extern "C" {
    /// Apply the policy `profile` to the current process.
    /// Args:
    ///  - `profile`: Policy text
    ///  - `flags`: `PROFILE_TEXT`
    ///  - `parameters`: Names and values in turns, with a null at the end
    ///  - `errorbuf`: Gets the error text on failure. Free it with
    ///    `sandbox_free_error`.
    ///
    /// Returns:
    ///   0 on success, -1 on failure.
    fn sandbox_init_with_parameters(
        profile: *const c_char,
        flags: u64,
        parameters: *const *const c_char,
        errorbuf: *mut *mut c_char,
    ) -> c_int;

    /// Free an error text from `sandbox_init_with_parameters`.
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Apply the sandbox with `policy` to the current process. The sandbox
/// stays until the process stops. Files and sockets that are already open
/// stay usable.
/// Returns:
///   Error if a path does not resolve, has a null byte or is not UTF-8, or
///   if Seatbelt refuses the policy. Then the process has no sandbox, and
///   the caller must stop it.
pub fn apply(policy: &Policy) -> Result<(), String> {
    let policy = resolved(policy)?;
    let (text, params) = profile(&policy);
    let text = CString::new(text).map_err(|e| format!("sandbox policy: {e}"))?;
    let params = params
        .into_iter()
        .flat_map(|(name, value)| [name, value])
        .map(|s| CString::new(s).map_err(|e| format!("sandbox parameter: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    // The parameter list must end with a null.
    let mut ptrs: Vec<*const c_char> = params.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: the policy text and all parameters are null-terminated and
    // live until the call ends. The list ends with a null.
    let rc = unsafe {
        sandbox_init_with_parameters(text.as_ptr(), PROFILE_TEXT, ptrs.as_ptr(), &raw mut err)
    };
    if rc == 0 {
        return Ok(());
    }
    let reason = if err.is_null() {
        "no reason given".to_string()
    } else {
        // SAFETY: Seatbelt gives a null-terminated text. It is not used
        // after the free.
        unsafe {
            let reason = CStr::from_ptr(err).to_string_lossy().into_owned();
            sandbox_free_error(err);
            reason
        }
    };
    Err(format!("sandbox apply: {reason}"))
}

/// Resolve all paths of `policy`.
///
/// Seatbelt checks the real path of each file, with all symlinks resolved.
/// For example `/var/folders/...` is `/private/var/folders/...` to
/// Seatbelt, so a rule for the `/var` form never matches. Resolve the paths
/// in this process just before the apply, so that the rules match the
/// files that libkrun uses.
/// Returns:
///   The policy with resolved paths, or error if a path does not resolve
///   or is not UTF-8.
fn resolved(policy: &Policy) -> Result<Policy, String> {
    let all = |paths: &[String]| paths.iter().map(|p| resolve(p)).collect::<Result<_, _>>();
    Ok(Policy {
        read_only: all(&policy.read_only)?,
        read_write: all(&policy.read_write)?,
        ro_files: all(&policy.ro_files)?,
        rw_files: all(&policy.rw_files)?,
        unix_sockets: all(&policy.unix_sockets)?,
    })
}

/// Resolve the symlinks of `path`. A missing name at the end stays as it
/// is (a socket before the bind).
/// Returns:
///   The resolved path, or error if it does not resolve or is not UTF-8.
fn resolve(path: &str) -> Result<String, String> {
    let fail = |e: &dyn std::fmt::Display| format!("sandbox path {path}: {e}");
    if !path.starts_with('/') {
        return Err(fail(&"not absolute"));
    }
    let verified = VerifiedPath::new_follow(path, None).map_err(|e| fail(&e))?;
    let real = verified.to_path().map_err(|e| fail(&e))?;
    real.to_str()
        .map(str::to_string)
        .ok_or_else(|| fail(&"not UTF-8"))
}

/// Make the Seatbelt policy text for `policy`. Use resolved paths.
/// Returns:
///   The policy text and its parameters (name, value). The text refers to
///   each path only by its parameter name.
fn profile(policy: &Policy) -> (String, Vec<(String, String)>) {
    let mut b = Builder::default();
    b.line("(version 1)");
    // Deny all that the rules below do not allow. This also denies all
    // network access except the vsock sockets, process start (exec, fork),
    // signals to other processes, system services (mach-lookup) and
    // hardware access (iokit).
    b.line("(deny default)");

    // A rule without filters allows the operation on all objects. Thus
    // each rule below must have a filter, also when its list is empty.
    let mut sysctl: Vec<String> = SYSCTL_NAMES
        .iter()
        .map(|n| format!("(sysctl-name \"{n}\")"))
        .collect();
    sysctl.extend(
        SYSCTL_PREFIXES
            .iter()
            .map(|p| format!("(sysctl-name-prefix \"{p}\")")),
    );
    b.line(&format!("(allow sysctl-read {})", sysctl.join(" ")));

    // Shares: libkrun opens the files of a share with `/.vol/<dev>/<ino>`
    // paths. Seatbelt checks the real path of the file, not the `/.vol`
    // text, so the share rules apply and `/.vol` needs no rule.
    // TODO(verify on macOS): check that an open of `/.vol/<dev>/<ino>` of
    // a file in a share is allowed, and of a file outside is denied. If
    // the lookup of `/.vol` itself gets a deny, allow only
    // `file-read-metadata` on the literal `/.vol`, never a subpath.
    // TODO(verify on macOS): an open through `/.vol` can give the
    // data volume form of the path (`/System/Volumes/Data/Users/...`).
    // If the system log shows denies with this form, add a second rule
    // with this form for each path.
    b.rule("file-read*", "(subpath {})", "RO_DIR", &policy.read_only);
    b.rule(
        "file-read* file-write*",
        "(subpath {})",
        "RW_DIR",
        &policy.read_write,
    );
    // Single files: `literal` matches only the file itself.
    b.rule("file-read*", "(literal {})", "RO_FILE", &policy.ro_files);
    b.rule(
        "file-read* file-write*",
        "(literal {})",
        "RW_FILE",
        &policy.rw_files,
    );

    if !policy.unix_sockets.is_empty() {
        // libkrun makes one listening Unix socket for each vsock port.
        // TODO(verify on macOS): check that libkrun makes the sockets
        // after the sandbox applies. If it makes them before, remove these
        // rules (accept on an open socket needs no rule).
        b.line("(allow system-socket (socket-domain AF_UNIX))");
        b.rule(
            "network-bind network-inbound",
            "(local unix-socket (path-literal {}))",
            "SOCKET",
            &policy.unix_sockets,
        );
        // The bind makes the socket file. A stop or a new bind can remove
        // an old one.
        // TODO(verify on macOS): check if the bind needs
        // `file-write-create`, and if libkrun removes old sockets. Remove
        // the operations that nothing uses.
        b.rule(
            "file-read-metadata file-write-create file-write-unlink",
            "(literal {})",
            "SOCKET_FILE",
            &policy.unix_sockets,
        );
    }

    // No rule for these, by intent:
    //  * Hypervisor: the VM calls go to the kernel through traps, which
    //    Seatbelt does not filter. The process has the hypervisor
    //    entitlement.
    //  * Threads and guest memory: thread start, anonymous memory maps and
    //    random numbers (getentropy) have no Seatbelt check.
    //  * Libraries: the VM process loads libkrun and all its libraries
    //    before the apply. A later library load gets a deny.
    //  * Log and input: reads and writes on open file descriptors (pipes,
    //    the disk image, the share roots) have no check.
    // TODO(verify on macOS): boot a VM, run a long session and check the
    // system log for Sandbox denies of the VM process
    // (`log stream --predicate 'sender == "Sandbox"'`). Check especially
    // `mach-lookup` and `iokit-open` from the hypervisor library.
    (b.text, b.params)
}

/// Builder of a policy text and its parameters.
#[derive(Default)]
struct Builder {
    /// Policy text.
    text: String,
    /// Parameters (name, value).
    params: Vec<(String, String)>,
}

impl Builder {
    /// Add a line to the policy text.
    fn line(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
    }

    /// Add one rule that allows `ops` on all `paths`. Adds no rule if
    /// `paths` is empty, because a rule without filters allows all.
    /// Args:
    ///  - `ops`: Operations, separated by spaces
    ///  - `filter`: Filter with `{}` in the place of the path, for
    ///    example `(subpath {})`
    ///  - `prefix`: Parameter name prefix. The name gets the index of the
    ///    path at the end.
    ///  - `paths`: Path values for the parameters
    fn rule(&mut self, ops: &str, filter: &str, prefix: &str, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        let mut filters = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let name = format!("{prefix}_{i}");
            filters.push(filter.replace("{}", &format!("(param \"{name}\")")));
            self.params.push((name, path.clone()));
        }
        // More than one filter in one rule means "any of them".
        self.line(&format!("(allow {ops} {})", filters.join(" ")));
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the policy text builder.

    use super::*;

    /// Path with characters that would change the policy if it were in the
    /// policy text.
    const EVIL: &str = "/tmp/a\") (allow default) (\"";

    /// Make a policy with one path for each class.
    fn one_of_each() -> Policy {
        Policy {
            read_only: vec!["/ro".into()],
            read_write: vec!["/rw".into(), EVIL.into()],
            ro_files: vec!["/kernel".into()],
            rw_files: vec!["/disk.img".into()],
            unix_sockets: vec!["/run/k1.sock".into()],
        }
    }

    /// Test that paths go to the policy only as parameters, so that a path
    /// cannot add rules.
    ///   1. Make a policy with a path that has policy syntax in it
    ///   2. Check that no path is in the policy text
    ///   3. Check that each path is a parameter value
    #[test]
    fn paths_with_policy_syntax_go_only_to_parameters() {
        let policy = one_of_each();
        let (text, params) = profile(&policy);
        for path in ["/ro", "/rw", EVIL, "/kernel", "/disk.img", "/run/k1.sock"] {
            assert!(!text.contains(path), "{path} in policy text:\n{text}");
            assert!(params.iter().any(|(_, v)| v == path), "{path} not a param");
        }
        assert!(!text.contains("allow default"));
    }

    /// Test that each path class gets the correct operations and filter.
    ///   1. Make a policy with paths in all classes
    ///   2. Find the parameter name of each path
    ///   3. Check the rule that refers to the name
    #[test]
    fn each_path_class_gets_its_rule() {
        let (text, params) = profile(&one_of_each());
        let rule_of = |value: &str, filter: &str| {
            let (name, _) = params
                .iter()
                .find(|(n, v)| v == value && !n.starts_with("SOCKET_FILE"))
                .unwrap();
            let needle = format!("{filter} (param \"{name}\")");
            text.lines()
                .find(|l| l.contains(&needle))
                .unwrap_or_else(|| panic!("no rule with {needle}:\n{text}"))
                .to_string()
        };
        let ro = rule_of("/ro", "(subpath");
        assert!(ro.starts_with("(allow file-read* (subpath"));
        let rw = rule_of(EVIL, "(subpath");
        assert!(rw.starts_with("(allow file-read* file-write* (subpath"));
        assert!(rw.contains("(param \"RW_DIR_0\")"));
        let kernel = rule_of("/kernel", "(literal");
        assert!(kernel.starts_with("(allow file-read* (literal"));
        let disk = rule_of("/disk.img", "(literal");
        assert!(disk.starts_with("(allow file-read* file-write* (literal"));
        let sock = rule_of("/run/k1.sock", "(local unix-socket (path-literal");
        assert!(sock.starts_with("(allow network-bind network-inbound "));
        assert!(text.contains("(param \"SOCKET_FILE_0\")"));
    }

    /// Test that an empty policy allows no file or network access. A rule
    /// without filters would allow all objects.
    ///   1. Make the policy text for an empty policy
    ///   2. Check that it denies by default and has only the sysctl rule
    #[test]
    fn empty_policy_allows_no_files_or_sockets() {
        let (text, params) = profile(&Policy::default());
        assert!(params.is_empty());
        assert!(text.contains("(deny default)"));
        let allows: Vec<_> = text.lines().filter(|l| l.starts_with("(allow")).collect();
        assert_eq!(allows.len(), 1, "{text}");
        assert!(allows[0].starts_with("(allow sysctl-read (sysctl-name"));
    }
}
