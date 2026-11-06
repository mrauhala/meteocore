//! `ServerState::reload_lock` lives as long as the reload, not as long as the
//! request that started it (#1402).

use super::{reload_handler, serve_reload, AdminState};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "reload-test-token";

/// A live state over one CSV collection: a reload rebuilds it with no poll
/// loop and no network.
fn state_over_csv(dir: &Path) -> AdminState {
    let csv = dir.join("weather.csv");
    std::fs::write(&csv, include_str!("../../../testdata/weather.csv")).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
host = "127.0.0.1"
port = 8000
[[collections]]
id = "stations"
title = "Stations"
description = "test"
data_path = {csv:?}
apis = ["features"]
"#
        ),
    )
    .unwrap();
    let mut state = crate::watcher::tests::build_state(&config_path);
    Arc::get_mut(&mut state)
        .expect("the state has one owner before any reload")
        .admin_token = Some(TOKEN.to_string());
    state
}

/// `POST /admin/collections/reload` with a valid token.
async fn post_reload(
    state: &AdminState,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().unwrap(),
    );
    reload_handler(State(state.clone()), headers)
        .await
        .map(IntoResponse::into_response)
}

/// A client that disconnects takes the handler's future with it, not the
/// reload that future spawned. The reload keeps the lock to its end: the next
/// request hears it is busy, and whoever waits, as the watcher and shutdown
/// do, gets the lock only after it.
#[tokio::test]
async fn a_reload_keeps_the_lock_after_its_client_disconnects() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_over_csv(dir.path());

    let (started, reload_started) = tokio::sync::oneshot::channel();
    let (release, reload_released) = std::sync::mpsc::channel::<()>();
    let finished = Arc::new(AtomicBool::new(false));
    let reload_finished = finished.clone();
    let mut handler = Box::pin(serve_reload(&state, move |state| {
        let _ = started.send(());
        // An error means a failed test dropped `release`: finish anyway.
        let _ = reload_released.recv();
        let outcome = super::do_reload(state);
        reload_finished.store(true, Ordering::SeqCst);
        outcome
    }));
    tokio::select! {
        _ = reload_started => {}
        _ = &mut handler => panic!("the handler answered before its reload ran"),
    }

    // What hyper does when the client goes away.
    drop(handler);

    // The reload is parked on `release`, so neither check can race its end.
    assert!(
        state.reload_lock.try_lock().is_err(),
        "the lock went with the handler while its reload still runs"
    );
    let (status, Json(body)) = post_reload(&state)
        .await
        .expect_err("a second reload must not start while the first runs");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "A reload is already in progress");

    release.send(()).unwrap();
    // The bound only catches a lock that never comes back; a passing run
    // takes as long as the reload.
    let turn = tokio::time::timeout(Duration::from_secs(60), state.reload_lock.lock())
        .await
        .expect("the lock came back when the reload ended");
    assert!(
        finished.load(Ordering::SeqCst),
        "a waiter got the lock before the reload ended"
    );
    drop(turn);

    // A client that stays hears only after the lock is free, so it can reload
    // again at once.
    let response = post_reload(&state).await.expect("the next reload runs");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(state.reload_lock.try_lock().is_ok());
}

/// A panicking reload is the handler's failed task, and the lock goes with
/// the unwinding task: a Tokio mutex does not poison, so nothing stays wedged.
#[tokio::test]
async fn a_panicking_reload_answers_500_and_frees_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_over_csv(dir.path());

    let (status, Json(body)) = serve_reload(&state, |_| panic!("deliberate panic in a test"))
        .await
        .expect_err("a panicking reload is an error");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "Reload task failed");
    assert!(state.reload_lock.try_lock().is_ok());

    let response = post_reload(&state).await.expect("the next reload runs");
    assert_eq!(response.status(), StatusCode::OK);
}
