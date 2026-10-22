//! `kill_detached_children` signals EVERY tracked group, so its test runs in a
//! process of its own, where no other test's children can be caught by it.

use ai_dev_companion::exec::{RunOptions, detached_child_count, kill_detached_children, run};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn kill_detached_children_signals_every_tracked_group() {
    let token = CancellationToken::new();
    let opts = RunOptions { cancel: Some(token.clone()), allow_failure: true, ..Default::default() };
    let handle = tokio::spawn(async move { run("sleep", &["30".to_string()], opts).await });
    for _ in 0..40 {
        if detached_child_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(detached_child_count(), 1);
    kill_detached_children(15);
    // The child died of the signal: no exit code, and the cancel token never fired.
    let r = handle.await.unwrap().unwrap();
    assert_eq!(r.code, -1);
    assert!(!token.is_cancelled());
    assert_eq!(detached_child_count(), 0);
}
