# Enable clippy pedantic workspace-wide

Added `[workspace.lints.clippy]` with `pedantic = "deny"` and
selective allows for noisy rules. All three crates inherit via
`[lints] workspace = true`. Fixed all pedantic violations across
the workspace:

- Replaced raw pointer casts with `(&raw const ..).cast::<T>()`
- Fixed RefCell borrows held across await points (clone before await)
- Replaced `Default::default()` with explicit type defaults
- Combined identical match arms
- Collapsed nested if statements
- Replaced redundant closures with function pointers
- Changed `&PathBuf` params to `&Path`
- Boxed large enum variants
- Used `strip_prefix` instead of manual prefix stripping
- Added targeted `#[allow]` for unavoidable cases (large futures,
  module inception, too many arguments, await holding refcell)

Also added `mise lint` task (fmt check + clippy) and `mise format`.
