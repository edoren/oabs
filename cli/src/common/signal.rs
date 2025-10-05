use thiserror::Error;
use tokio::io;

#[derive(Error, Debug)]
pub enum SignalError {
    #[error("Failed to register signal handler")]
    RegisterFailed(#[from] io::Error),
}

pub async fn close() -> Result<(), SignalError> {
    #[cfg(not(target_os = "windows"))]
    {
        signal::ctrl_c().await?;
        cancellation_token.cancel();
        cancellation_token.cancelled().await;
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        use tokio::signal;

        let mut ctrl_c_signal = signal::windows::ctrl_c()?;
        let mut ctrl_close_signal = signal::windows::ctrl_close()?;
        let mut ctrl_break_signal = signal::windows::ctrl_break()?;
        let mut ctrl_logoff_signal = signal::windows::ctrl_logoff()?;
        let mut ctrl_shutdown_signal = signal::windows::ctrl_shutdown()?;
        tokio::select! {
            _ = ctrl_c_signal.recv() => { },
            _ = ctrl_close_signal.recv() => { },
            _ = ctrl_break_signal.recv() => { },
            _ = ctrl_logoff_signal.recv() => { },
            _ = ctrl_shutdown_signal.recv() => { },
        }
        return Ok(());
    }
}
