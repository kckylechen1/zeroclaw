use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::task::AbortOnDropHandle;

async fn blocking_cron_child(
    children: &TaskTracker,
) -> (JoinHandle<()>, Arc<AtomicBool>, std::sync::mpsc::Sender<()>) {
    let ended = Arc::new(AtomicBool::new(false));
    let child_ended = Arc::clone(&ended);
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let tracked = children.track_future(async move {
        started.send(()).unwrap();
        // Deliberately exercise a synchronous section that cannot observe
        // abort until its next yield. The daemon must keep a join barrier
        // outside the parent task that its grace-window expiry aborts.
        let _ = release_rx.recv();
        tokio::task::yield_now().await;
        child_ended.store(true, Ordering::SeqCst);
    });
    let child = AbortOnDropHandle::new(zeroclaw_spawn::spawn!(tracked));
    let parent = zeroclaw_spawn::spawn!(async move {
        let _child = child;
        std::future::pending::<()>().await;
    });
    started_rx.await.unwrap();
    (parent, ended, release)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_component_abort_waits_for_cron_child_destruction() {
    let children = TaskTracker::new();
    let (parent, ended, release) = blocking_cron_child(&children).await;
    let parent_abort = parent.abort_handle();
    let stop_tracker = children.clone();
    let stopping = AbortOnDropHandle::new(zeroclaw_spawn::spawn!(async move {
        stop_components(
            vec![parent],
            &stop_tracker,
            COMPONENT_SHUTDOWN_GRACE,
            CRON_CHILD_SHUTDOWN_TIMEOUT,
        )
        .await
    }));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !parent_abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !stopping.is_finished(),
        "the 500ms parent abort alone is not a child join"
    );
    assert!(!children.is_empty());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), stopping)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        children.is_empty(),
        "reload requires actual child destruction"
    );
    assert!(
        !ended.load(Ordering::SeqCst),
        "cancelled child resumed after yielding"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_cron_shutdown_blocks_reload_instead_of_clearing_live_claims() {
    let children = TaskTracker::new();
    let (parent, ended, release) = blocking_cron_child(&children).await;
    let result = stop_components(
        vec![parent],
        &children,
        Duration::from_millis(10),
        Duration::from_millis(10),
    )
    .await;
    assert_eq!(
        result.unwrap_err().to_string(),
        crate::i18n::get_required_cli_string("daemon-cron-shutdown-incomplete"),
    );
    assert!(
        !children.is_empty(),
        "the error reports a real unfinished child"
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), children.wait())
        .await
        .unwrap();
    assert!(!ended.load(Ordering::SeqCst));
}
