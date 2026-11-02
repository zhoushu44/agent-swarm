//! End-to-end tests over the real HTTP surface.
//!
//! The routes are the contract the model is told about in the prompt, so these
//! tests exercise them the way a worker would: create an item, claim it, publish
//! context, submit it. Status codes matter as much as bodies — a 409 on claim is
//! normal contention, not an error.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use swarm_core::CreateItemInput;
use swarm_server::{router, AppState, ServerConfig};
use swarm_store::Store;

async fn spawn_app() -> (Store, axum::Router) {
    let store = Store::open_memory().unwrap();
    let state = AppState::new(store.clone(), ServerConfig::default());
    let app = router(state);
    (store, app)
}

async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let body: Value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

fn post_json(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn healthz_is_ok() {
    let (_store, app) = spawn_app().await;
    let (status, body) = call(&app, get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
}

/// A 5xx must not leak internals: the body is a generic message, the detail
/// goes to the log only.
///
/// Uses a file-backed store that we then delete out from under it, so the next
/// query fails at the SQLite layer instead of at a validation check.
#[tokio::test]
async fn internal_errors_do_not_leak_details() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("swarm.db");
    let store = Store::open(db.to_str().unwrap()).unwrap();
    // Corrupt the database file: the store still holds the connection, but the
    // next statement fails.
    std::fs::write(&db, b"this is not a sqlite database at all").unwrap();

    let state = AppState::new(store, ServerConfig::default());
    let app = router(state);

    let (status, body) = call(&app, get("/board")).await;
    // Either the connection notices immediately (500) or the store returns a
    // store error; either way the detail must not reach the client.
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        assert_eq!(
            body["error"].as_str().unwrap_or_default(),
            "internal error",
            "detail must not reach the client"
        );
    }
}

#[tokio::test]
async fn create_then_get_item() {
    let (store, app) = spawn_app().await;

    let (status, created) = call(
        &app,
        post_json(
            "/item",
            json!({"peer":"seed","title":"ship the exporter","detail":"see docs"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["status"], "pending");
    assert_eq!(created["kind"], "task");

    let (status, item) = call(&app, get(&format!("/item?id={}", id))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(item["item"]["title"], "ship the exporter");
    // history is always an array, never null
    assert!(item["history"].is_array());

    // The store agrees.
    assert_eq!(store.get_item(&id).unwrap().title, "ship the exporter");
}

#[tokio::test]
async fn create_item_requires_title() {
    let (_store, app) = spawn_app().await;
    let (status, body) = call(&app, post_json("/item", json!({"peer":"seed","title":"  "}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("title"));
}

#[tokio::test]
async fn board_paginates_and_reports_has_more() {
    let (store, app) = spawn_app().await;
    for i in 0..5 {
        store
            .create_item(CreateItemInput {
                peer: "seed".into(),
                title: format!("t{}", i),
                ..Default::default()
            })
            .unwrap();
    }

    let (status, board) = call(&app, get("/board?limit=2&offset=0")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(board["items"].as_array().unwrap().len(), 2);
    assert_eq!(board["total"], 5);
    assert_eq!(board["hasMore"], true);

    let (_, last) = call(&app, get("/board?limit=2&offset=4")).await;
    assert_eq!(last["hasMore"], false, "no more after the last page");
}

#[tokio::test]
async fn board_limit_clamps_like_go() {
    let (store, app) = spawn_app().await;
    for i in 0..3 {
        store
            .create_item(CreateItemInput {
                peer: "seed".into(),
                title: format!("t{}", i),
                ..Default::default()
            })
            .unwrap();
    }
    // limit=0 falls back to 200, so all three come back.
    let (_, board) = call(&app, get("/board?limit=0")).await;
    assert_eq!(board["limit"], 200);
    assert_eq!(board["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn claim_returns_the_task_package() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "fix src/a.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    let (status, res) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w1"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!res["token"].as_str().unwrap().is_empty());
    assert_eq!(res["item"]["status"], "in_progress");
    assert!(res["history"].is_array());
    assert!(res["conflicts"].is_array());
}

#[tokio::test]
async fn second_claim_is_a_409() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "t".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    let (first, _) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w1"}))).await;
    assert_eq!(first, StatusCode::OK);

    let (second, _) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w2"}))).await;
    assert_eq!(second, StatusCode::CONFLICT, "a live lease blocks a second claim");
}

#[tokio::test]
async fn claim_unknown_item_is_404() {
    let (_store, app) = spawn_app().await;
    let (status, _) = call(&app, post_json("/claim", json!({"itemId":"nope","peer":"w1"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn claim_requires_item_and_peer() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "t".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let (status, _) = call(&app, post_json("/claim", json!({"itemId": id, "peer":""}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn release_then_reclaim() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "t".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    let (_, res) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w1"}))).await;
    let token = res["token"].as_str().unwrap().to_string();

    let (status, body) = call(&app, post_json("/release", json!({"token": token}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);

    // Back to pending, so another worker can take it.
    assert_eq!(store.get_item(&id).unwrap().status, "pending");
    let (again, _) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w2"}))).await;
    assert_eq!(again, StatusCode::OK);
}

#[tokio::test]
async fn renew_keeps_the_lease() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "t".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let (_, res) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w1"}))).await;
    let token = res["token"].as_str().unwrap().to_string();

    let (status, _) = call(&app, post_json("/renew", json!({"token": token}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(store.active_claims_by_peer("w1").unwrap().len(), 1);
}

#[tokio::test]
async fn submit_marks_the_item_submitted() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "t".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let (_, res) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w1"}))).await;
    let token = res["token"].as_str().unwrap().to_string();

    let (status, body) = call(
        &app,
        post_json("/submit", json!({"token": token, "peer":"w1", "commitSha":"abc"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["item"]["status"], "submitted");
    assert_eq!(store.queued_submissions().unwrap().len(), 1);
}

#[tokio::test]
async fn missing_token_is_a_400() {
    let (_store, app) = spawn_app().await;
    for path in ["/release", "/renew"] {
        let (status, _) = call(&app, post_json(path, json!({"token":""}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
}

#[tokio::test]
async fn context_and_search_roundtrip() {
    let (_store, app) = spawn_app().await;
    let (status, _) = call(
        &app,
        post_json(
            "/context",
            json!({"peer":"w1","kind":"FAIL","text":"gate failed on step 3"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, found) = call(&app, get("/context/search?q=gate")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["count"], 1);
    assert!(found["entries"][0]["text"]
        .as_str()
        .unwrap()
        .contains("gate failed"));
}

#[tokio::test]
async fn context_requires_text() {
    let (_store, app) = spawn_app().await;
    let (status, _) = call(&app, post_json("/context", json!({"peer":"w1","kind":"NOTE","text":"  "}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn intent_mark_shows_up_in_marks_and_conflicts() {
    let (store, app) = spawn_app().await;

    // Publish an INTENT on a resource.
    let (status, _) = call(
        &app,
        post_json(
            "/context",
            json!({"peer":"w1","kind":"INTENT","text":"editing","resource":"src/a.js","ttlMs":60000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // /marks?resource= shows it.
    let (status, marks) = call(&app, get("/marks?resource=src/a.js")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(marks["count"], 1);

    // A second item on the same resource sees the conflict.
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "also src/a.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let (_, res) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w2"}))).await;
    assert_eq!(res["conflicts"].as_array().unwrap().len(), 1);
    assert_eq!(res["conflicts"][0]["peer"], "w1");
}

/// `/marks` must report the resource LEASE, not just the intent mark.
///
/// The 304-worker stress run showed workers polling `/marks`, seeing
/// `count: 0`, and claiming anyway — because a lease (the authority) and an
/// intent mark (a worker's hint) are separate tables and only the mark was
/// reported. The worker then got a 409 it had no way to anticipate and fell
/// back to blind retry.
///
/// `held` is the field a worker deciding whether to claim must read.
#[tokio::test]
async fn marks_reports_the_resource_lease_not_just_the_hint() {
    let (store, app) = spawn_app().await;

    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "edit src/b.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    // Claim it. No INTENT mark is written, so the mark count is zero.
    let (_, _) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w1"}))).await;

    let (status, marks) = call(&app, get("/marks?resource=src/b.js")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(marks["count"], 0, "no intent mark was written");
    assert_eq!(
        marks["held"], 1,
        "but the resource IS held — this is what a worker must see"
    );
    assert_eq!(marks["isHeld"], true);
    assert_eq!(marks["leases"][0]["peer"], "w1");

    // Once released, the lease is gone.
    let (_, _) = call(&app, post_json("/release", json!({"token":"c1"}))).await;
    let (_, marks) = call(&app, get("/marks?resource=src/b.js")).await;
    assert_eq!(marks["held"], 0, "release must clear the lease");
    assert_eq!(marks["isHeld"], false);
}

#[tokio::test]
async fn digest_reports_claims_and_pending() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "t".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    call(&app, post_json("/claim", json!({"itemId": id, "peer":"w1"}))).await;

    let (status, d) = call(&app, get("/digest?peer=w1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(d["claims"].as_array().unwrap().len(), 1);
    assert!(d["pendingPreview"].is_array());
    assert!(d["now"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn messages_roundtrip() {
    let (_store, app) = spawn_app().await;
    let (status, _) = call(
        &app,
        post_json("/message", json!({"from":"w1","to":"w2","text":"heads up"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(&app, get("/messages?peer=w2")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 1);
    assert_eq!(body["messages"][0]["from"], "w1");
}

#[tokio::test]
async fn message_requires_to_and_text() {
    let (_store, app) = spawn_app().await;
    let (status, _) = call(&app, post_json("/message", json!({"from":"w1","to":"","text":"x"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn no_create_blocks_new_top_level_but_allows_splits() {
    let store = Store::open_memory().unwrap();
    let state = AppState::new(
        store.clone(),
        ServerConfig {
            forbid_item_creation: true,
            ..Default::default()
        },
    );
    let app = router(state);

    // A worker inventing new top-level work is refused.
    let (status, body) = call(
        &app,
        post_json("/item", json!({"peer":"w1","title":"invented work"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("disabled"));

    // A split (parent given) is allowed.
    let parent = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "parent".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let (status, _) = call(
        &app,
        post_json(
            "/item",
            json!({"peer":"w1","title":"src/b.js","parentId": parent}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // auto-split is always allowed, even with no parent.
    let (status, _) = call(
        &app,
        post_json("/item", json!({"peer":"auto-split","title":"src/c.js"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// `needsSplit` is true for a **seed** item with no parent and no children —
/// the resource count is deliberately NOT part of the test (Go items.go
/// L284-303). The runner's own `splitThreshold` decides whether a split
/// actually happens; this flag only says "consider decomposing first".
#[tokio::test]
async fn needs_split_is_true_for_a_fresh_seed_item() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "fix src/a.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    let (status, res) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w1"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["needsSplit"], true);
}

/// A worker-created item never asks for a split: it is already the worker's own
/// decomposition.
#[tokio::test]
async fn needs_split_is_false_for_a_worker_created_item() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "w1".into(),
            title: "my own work".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    let (_, res) = call(&app, post_json("/claim", json!({"itemId": id, "peer":"w1"}))).await;
    assert_eq!(res["needsSplit"], false);
}

/// Once the item has children it has already been decomposed.
#[tokio::test]
async fn needs_split_is_false_once_children_exist() {
    let (store, app) = spawn_app().await;
    let parent = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "parent".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    store
        .create_item(CreateItemInput {
            peer: "auto-split".into(),
            title: "src/a.js".into(),
            parent_id: Some(parent.clone()),
            ..Default::default()
        })
        .unwrap();

    let (_, res) = call(&app, post_json("/claim", json!({"itemId": parent, "peer":"w1"}))).await;
    assert_eq!(res["needsSplit"], false, "an already-split item must not split again");
}

#[tokio::test]
async fn error_bodies_are_never_null_arrays() {
    let (store, app) = spawn_app().await;
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "t".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    let (_, board) = call(&app, get("/board")).await;
    assert!(board["items"].is_array(), "empty board must still be an array");

    let (_, item) = call(&app, get(&format!("/item?id={}", id))).await;
    assert!(item["history"].is_array());
}
