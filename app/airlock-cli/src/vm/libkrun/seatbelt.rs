//! Seatbelt sandbox for the libkrun VM process.
//!
//! libkrun has no sandbox of its own, and its shared directory server does
//! not keep the guest in the shared directories. This module locks the VM
//! process to the files and sockets that the VM needs. A guest that escapes
//! a share still cannot reach other host files.
//!
//! The policy denies all by default. It allows only:
//!  * read access to read-only shares, kernel and initramfs
//!  * read and write access to read-write shares and the disk image
//!  * the vsock sockets, the only connection to airlock
//!
//! The sandbox cannot be removed. Apply it after the libraries load and
//! before libkrun builds the VM. Open files stay usable.
//!
//! All paths must be real paths, without symlinks.

use std::ffi::{CStr, CString, c_char, c_int};

use crate::util::VerifiedPath;

/// Host paths that the VM process can use. All paths must be absolute.
#[derive(Debug, Default, Clone)]
pub struct Policy {
    /// Directories with read access.
    pub read_only: Vec<String>,
    /// Directories with read and write access.
    pub read_write: Vec<String>,
    /// Files with read access.
    pub ro_files: Vec<String>,
    /// Files with read and write access.
    pub rw_files: Vec<String>,
    /// Unix sockets that the VM process creates. The parent directory must
    /// exist.
    pub unix_sockets: Vec<String>,
}

/// Flag value of `sandbox_init_with_parameters` for policy text.
const PROFILE_TEXT: u64 = 0;

// libSystem has these functions, so no link attribute is necessary. Apple
// marks the API as deprecated, but Chromium and WebKit still use it.
unsafe extern "C" {
    /// Apply the policy `profile` to the current process.
    /// Args:
    ///  - `profile`: Policy text
    ///  - `flags`: `PROFILE_TEXT`
    ///  - `parameters`: Names and values in turns, null at the end
    ///  - `errorbuf`: Error text on failure. Free it with
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

/// Apply the sandbox with `policy` to the current process.
/// Returns:
///   Error if a path is not valid or Seatbelt refuses the policy. Then the
///   process has no sandbox and the caller must stop it.
pub fn apply(policy: &Policy) -> Result<(), String> {
    check_real(policy)?;
    let (text, params) = profile(policy);
    let text = CString::new(text).map_err(|e| format!("sandbox policy: {e}"))?;
    let params = params
        .into_iter()
        .flat_map(|(name, value)| [name, value])
        .map(|s| CString::new(s).map_err(|e| format!("sandbox parameter: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let mut ptrs: Vec<*const c_char> = params.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: text and parameters are null-terminated and live until the
    // call ends. The list ends with a null.
    let rc = unsafe {
        sandbox_init_with_parameters(text.as_ptr(), PROFILE_TEXT, ptrs.as_ptr(), &raw mut err)
    };
    if rc == 0 {
        return Ok(());
    }
    let reason = if err.is_null() {
        "no reason given".to_string()
    } else {
        // SAFETY: Seatbelt gives a null-terminated text. No use after free.
        unsafe {
            let reason = CStr::from_ptr(err).to_string_lossy().into_owned();
            sandbox_free_error(err);
            reason
        }
    };
    Err(format!("sandbox apply: {reason}"))
}

/// Check that all paths of `policy` are real paths. A rule matches only the
/// real path, so a symlink would break the rule or hide the target.
/// Returns:
///   Error if a path is not UTF-8 or differs from its real path.
fn check_real(policy: &Policy) -> Result<(), String> {
    let all = [
        &policy.read_only,
        &policy.read_write,
        &policy.ro_files,
        &policy.rw_files,
        &policy.unix_sockets,
    ];
    for path in all.into_iter().flatten() {
        let real = resolve(path)?;
        if real != *path {
            return Err(format!(
                "sandbox path {path}: not a real path (real path: {real})"
            ));
        }
    }
    Ok(())
}

/// Resolve the symlinks of `path`. A missing last name stays as it is
/// (a socket before the bind).
/// Returns:
///   The resolved path, or error.
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

/// Make the policy text for `policy`.
/// Returns:
///   The text and its parameters (name, value). Paths go only to
///   parameters, so a path cannot change the policy.
fn profile(policy: &Policy) -> (String, Vec<(String, String)>) {
    let mut b = Builder::default();
    b.line("(version 1)");
    // This also denies other network, exec, fork, signals, mach-lookup and
    // iokit.
    b.line("(deny default)");

    // A rule without filter allows all objects. Each rule below needs a
    // filter.

    // The hypervisor library reads this value at the VM build.
    b.line("(allow sysctl-read (sysctl-name \"kern.hv_vmm_present\"))");

    // Shares: libkrun opens share files with `/.vol/<dev>/<ino>` paths.
    // Seatbelt checks the real path, so `/.vol` needs no rule. `/.vol` paths
    // outside the shares, or `..` from a share root, get a deny.
    b.rule("file-read*", "(subpath {})", "RO_DIR", &policy.read_only);
    b.rule(
        "file-read* file-write*",
        "(subpath {})",
        "RW_DIR",
        &policy.read_write,
    );

    // Boot files and disk image: libkrun opens them at the VM build, after
    // the apply.
    b.rule("file-read*", "(literal {})", "RO_FILE", &policy.ro_files);
    b.rule(
        "file-read* file-write*",
        "(literal {})",
        "RW_FILE",
        &policy.rw_files,
    );

    // libkrun binds one Unix socket for each vsock port. The bind needs
    // both rules. Accept, read and write need none.
    b.rule(
        "network-bind",
        "(local unix-socket (path-literal {}))",
        "SOCKET",
        &policy.unix_sockets,
    );
    b.rule(
        "file-write-create",
        "(literal {})",
        "SOCKET_FILE",
        &policy.unix_sockets,
    );

    // No rule for these, by intent:
    //  * Hypervisor: VM calls are traps that Seatbelt does not filter. The
    //    library asks for the privileged host port at vCPU start and gets a
    //    deny. The VM runs without it.
    //  * Threads, anonymous memory, getentropy: no Seatbelt check.
    //  * Libraries: loaded before the apply.
    //  * Open file descriptors: reads and writes have no check.
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

    /// Add one rule that allows `ops` on all `paths`. Adds no rule for an
    /// empty list, because a rule without filter allows all.
    /// Args:
    ///  - `ops`: Operations, separated by spaces
    ///  - `filter`: Filter with `{}` in the place of the path, for
    ///    example `(subpath {})`
    ///  - `prefix`: Parameter name prefix, followed by the path index
    ///  - `paths`: Parameter values
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

    /// Path with policy syntax.
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
        assert!(sock.starts_with("(allow network-bind (local"));
        assert!(text.contains("(allow file-write-create (literal (param \"SOCKET_FILE_0\")))"));
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
        assert!(allows[0].starts_with("(allow sysctl-read (sysctl-name "));
    }
}
