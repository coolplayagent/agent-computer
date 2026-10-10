//! A renewal failure must not detach a still-running filesystem capture.
use agent_computer_store::{Error, Result};
use std::{future::Future, time::Duration};
use tokio::{task::JoinHandle, time::MissedTickBehavior};

pub(super) async fn joined<T, R, F>(
    mut task: JoinHandle<Result<T>>,
    mut renew: R,
    period: Duration,
) -> Result<T>
where
    R: FnMut() -> F,
    F: Future<Output = Result<()>>,
{
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval.reset();
    let mut failure = None;
    loop {
        tokio::select! {
            biased;
            result=&mut task=>{
                let result=result.map_err(|_|Error::InvalidReconcileResult)?;
                return match failure { Some(error)=>Err(error), None=>result };
            },
            _=interval.tick(), if failure.is_none()=>{
                if let Err(error)=renew().await { failure=Some(error); }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn lost_authority_waits_for_the_real_capture_thread_and_returns_no_bundle() {
        let (release, blocked) = std::sync::mpsc::channel();
        let (entered, started) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
            Ok(42)
        });
        started.await.unwrap();
        let (failed, noticed) = tokio::sync::oneshot::channel();
        let mut failed = Some(failed);
        let joined = tokio::spawn(async move {
            super::joined(
                task,
                move || {
                    failed.take().unwrap().send(()).unwrap();
                    async { Err(Error::StaleReconcileLease) }
                },
                Duration::from_millis(10),
            )
            .await
        });
        noticed.await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !joined.is_finished(),
            "filesystem capture must retain its slot"
        );
        release.send(()).unwrap();
        assert!(matches!(
            joined.await.unwrap(),
            Err(Error::StaleReconcileLease)
        ));
    }
}
