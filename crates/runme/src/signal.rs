use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::Signal;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;

use crate::process::ProcessHandle;

/// Tracks active child processes and handles OS signals.
///
/// On SIGINT/SIGTERM: forwards the signal to all child process groups,
/// waits for them to exit, then exits the current process.
///
/// On SIGHUP: forwards SIGHUP to all children (for reload semantics).
pub struct SignalHandler {
    processes: Arc<Mutex<Vec<Arc<Mutex<ProcessHandle>>>>>,
}

impl SignalHandler {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a process handle for signal management.
    pub async fn track(&self, handle: Arc<Mutex<ProcessHandle>>) {
        self.processes.lock().await.push(handle);
    }

    /// Remove process handles that are no longer running.
    pub async fn cleanup(&self) {
        let mut procs = self.processes.lock().await;
        let mut to_remove = Vec::new();
        for (i, handle) in procs.iter().enumerate() {
            if !handle.lock().await.is_running() {
                to_remove.push(i);
            }
        }
        // Remove in reverse order to preserve indices
        for i in to_remove.into_iter().rev() {
            procs.remove(i);
        }
    }

    /// Stop all tracked processes gracefully.
    pub async fn stop_all(&self, timeout: Duration) {
        let procs = self.processes.lock().await;
        for handle in procs.iter() {
            let mut h = handle.lock().await;
            let _ = h.stop(timeout).await;
        }
    }

    /// Forward a signal to all tracked processes.
    pub async fn signal_all(&self, sig: Signal) {
        let procs = self.processes.lock().await;
        for handle in procs.iter() {
            let h = handle.lock().await;
            let _ = h.signal(sig);
        }
    }

    /// Install OS signal handlers that manage all tracked child processes.
    ///
    /// This spawns background tokio tasks that listen for SIGINT, SIGTERM,
    /// and SIGHUP. Returns a JoinHandle for the signal handler task.
    ///
    /// - SIGINT/SIGTERM: stop all children gracefully, then exit
    /// - SIGHUP: forward to all children for reload
    pub fn install(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let handler = self.clone();

        tokio::spawn(async move {
            let mut sigint =
                signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
            let mut sighup =
                signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");

            loop {
                tokio::select! {
                    _ = sigint.recv() => {
                        handler.stop_all(Duration::from_secs(5)).await;
                        std::process::exit(130); // 128 + SIGINT(2)
                    }
                    _ = sigterm.recv() => {
                        handler.stop_all(Duration::from_secs(5)).await;
                        std::process::exit(143); // 128 + SIGTERM(15)
                    }
                    _ = sighup.recv() => {
                        handler.signal_all(Signal::SIGHUP).await;
                    }
                }
            }
        })
    }
}

impl Default for SignalHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::buffer::OutputBuffer;
    use crate::process;

    #[tokio::test]
    async fn test_signal_handler_track_and_stop() {
        let handler = SignalHandler::new();

        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let handle = process::spawn("sleep 60", "test-signal", buffer)
            .await
            .unwrap();
        let handle = Arc::new(Mutex::new(handle));

        handler.track(handle.clone()).await;

        // Stop all should terminate the child
        handler.stop_all(Duration::from_secs(2)).await;

        assert!(!handle.lock().await.is_running());
    }

    #[tokio::test]
    async fn test_signal_handler_cleanup() {
        let handler = SignalHandler::new();

        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let handle = process::spawn("echo done", "test-cleanup", buffer)
            .await
            .unwrap();
        let handle = Arc::new(Mutex::new(handle));

        // Wait for it to finish
        let _ = handle.lock().await.wait().await;

        handler.track(handle.clone()).await;
        handler.cleanup().await;

        // After cleanup, the dead process should be removed
        assert!(handler.processes.lock().await.is_empty());
    }

    #[tokio::test]
    async fn test_signal_handler_forward_signal() {
        let handler = SignalHandler::new();

        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let handle = process::spawn("sleep 60", "test-forward", buffer)
            .await
            .unwrap();
        let handle = Arc::new(Mutex::new(handle));

        handler.track(handle.clone()).await;

        // Forward SIGTERM
        handler.signal_all(Signal::SIGTERM).await;

        // Give the process a moment to die
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Process should have exited
        assert!(!handle.lock().await.is_running());
    }
}
