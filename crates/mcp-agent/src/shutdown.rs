use tokio_util::sync::CancellationToken;

/// Cancels server admission after the platform interrupt signal.
///
/// # Errors
///
/// Returns an error when the operating system signal listener cannot start.
pub async fn cancel_on_signal(token: CancellationToken) -> std::io::Result<()> {
    tokio::signal::ctrl_c().await?;
    token.cancel();
    Ok(())
}
