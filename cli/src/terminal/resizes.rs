use tokio::signal::unix::Signal;

pub fn resizes() -> anyhow::Result<Signal> {
    let s = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::window_change()
    )?;
    Ok(s)
}