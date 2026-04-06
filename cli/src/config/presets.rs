use include_dir::{Dir, include_dir};

static PRESETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/config/presets");

/// Iterate all non-test bundled presets as (name, parsed value) pairs.
#[cfg(test)]
pub fn all() -> impl Iterator<Item = (String, serde_json::Value)> {
    PRESETS.files().filter_map(|f| {
        let name = f.path().file_stem()?.to_string_lossy().to_string();
        if name.starts_with("test-") {
            return None;
        }
        let toml_str = std::str::from_utf8(f.contents()).ok()?;
        let value = toml::from_str(toml_str).ok()?;
        Some((name, value))
    })
}

/// Look up a built-in preset by name.
pub fn get(name: &str) -> Option<serde_json::Value> {
    let preset = PRESETS.files().find(|f| {
        f.path()
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
            == format!("{name}.toml")
    })?;
    Some(toml::from_slice(preset.contents()).expect("built-in preset has invalid TOML"))
}
