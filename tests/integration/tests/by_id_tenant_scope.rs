//! By-id memory operations are confined to ONE tenant.
//!
//! A memory id is a global handle. An orchestrator that holds a single admin key (`<token>@:admin`,
//! no tenant) for many projects must be able to pin the tenant per request with
//! `X-Ecphoria-Tenant` — on REST and on MCP — and then update / delete / read the history of ITS
//! project's memories only. Another tenant's id answers "not found" (404 on REST, a JSON-RPC error
//! on MCP), never "forbidden", so existence does not leak.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use ecphoria_core::{CoreConfig, EcphoriaEngine};
use tower::ServiceExt;

const ADMIN: &str = "admin-key";
const BOUND: &str = "bound-key";

async fn app() -> axum::Router {
    let mut config = CoreConfig::default();
    config.memory.episodic.db_path = ":memory:".into();
    config.memory.state.db_path = ":memory:".into();
    config.memory.cognition.db_path = ":memory:".into();
    let engine = Arc::new(EcphoriaEngine::new(config).await.unwrap());
    let auth = ecphoria_gateway::auth::middleware::AuthState::new(
        vec![
            format!("{ADMIN}@:admin"),
            format!("{BOUND}@project-a:admin"),
        ],
        None,
        0,
    );
    let gw = ecphoria_gateway::server::GatewayConfig {
        auth_enabled: true,
        ..Default::default()
    };
    ecphoria_gateway::rest::router_with_engine_and_auth(engine, Some(auth), None, None, &gw)
}

async fn send(
    app: &axum::Router,
    key: &str,
    tenant: Option<&str>,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = tenant {
        b = b.header("X-Ecphoria-Tenant", t);
    }
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(body.to_string())
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Call an MCP tool; `Ok(result JSON)` or `Err(error message)`.
async fn mcp(
    app: &axum::Router,
    key: &str,
    tenant: Option<&str>,
    tool: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (status, resp) = send(
        app,
        key,
        tenant,
        "POST",
        "/mcp",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":tool,"arguments":args}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    if let Some(e) = resp.get("error") {
        return Err(e["message"].as_str().unwrap_or_default().to_string());
    }
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    Ok(serde_json::from_str(text).unwrap())
}

/// Seed one memory (with a superseded version, so history has two entries) in `tenant`.
async fn seed(app: &axum::Router, tenant: &str, what: &str) -> String {
    let first = mcp(
        app,
        ADMIN,
        Some(tenant),
        "add_memory",
        serde_json::json!({"content": format!("{what} v1"), "subject": "deploy.target"}),
    )
    .await
    .unwrap();
    assert!(first["memory"]["id"].is_string());
    let second = mcp(
        app,
        ADMIN,
        Some(tenant),
        "add_memory",
        serde_json::json!({"content": format!("{what} v2"), "subject": "deploy.target"}),
    )
    .await
    .unwrap();
    second["memory"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn mcp_by_id_tools_refuse_another_tenants_memory() {
    let app = app().await;
    let a = seed(&app, "project-a", "A ships on k8s").await;
    let _b = seed(&app, "project-b", "B ships on vms").await;

    // Pinned to project-b by the header: A's id does not exist, whatever the tool.
    for tool in ["memory_history", "memory_provenance", "delete_memory"] {
        let err = mcp(
            &app,
            ADMIN,
            Some("project-b"),
            tool,
            serde_json::json!({"id": a}),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "memory not found", "{tool}");
    }
    let err = mcp(
        &app,
        ADMIN,
        Some("project-b"),
        "update_memory",
        serde_json::json!({"id": a, "content": "hijacked"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "memory not found");
    let err = mcp(
        &app,
        ADMIN,
        Some("project-b"),
        "memory_feedback",
        serde_json::json!({"id": a, "verdict": "wrong"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "memory not found");

    // A `tenant_id` argument cannot widen a header-pinned scope.
    let err = mcp(
        &app,
        ADMIN,
        Some("project-b"),
        "delete_memory",
        serde_json::json!({"id": a, "tenant_id": "project-a"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "memory not found");

    // Nothing was touched.
    let hist = mcp(
        &app,
        ADMIN,
        Some("project-a"),
        "memory_history",
        serde_json::json!({"id": a}),
    )
    .await
    .unwrap();
    assert_eq!(hist["count"], 2);
    assert!(hist["history"][1]["content"]
        .as_str()
        .unwrap()
        .ends_with("v2"));
}

#[tokio::test]
async fn mcp_by_id_tools_work_inside_the_tenant() {
    let app = app().await;
    let a = seed(&app, "project-a", "A ships on k8s").await;

    let hist = mcp(
        &app,
        ADMIN,
        Some("project-a"),
        "memory_history",
        serde_json::json!({"id": a}),
    )
    .await
    .unwrap();
    assert_eq!(hist["count"], 2);

    let upd = mcp(
        &app,
        ADMIN,
        Some("project-a"),
        "update_memory",
        serde_json::json!({"id": a, "content": "A ships on nomad"}),
    )
    .await
    .unwrap();
    assert_eq!(upd["content"], "A ships on nomad");

    let del = mcp(
        &app,
        ADMIN,
        Some("project-a"),
        "delete_memory",
        serde_json::json!({"id": a}),
    )
    .await
    .unwrap();
    assert_eq!(del["deleted"], true);
    let gone = mcp(
        &app,
        ADMIN,
        Some("project-a"),
        "memory_history",
        serde_json::json!({"id": a}),
    )
    .await
    .unwrap_err();
    assert_eq!(gone, "memory not found");
}

#[tokio::test]
async fn mcp_tenant_id_argument_scopes_by_id_tools_without_a_header() {
    let app = app().await;
    let a = seed(&app, "project-a", "A ships on k8s").await;
    // No header (legacy caller) but the call names its tenant: another tenant's id is not found.
    let err = mcp(
        &app,
        ADMIN,
        None,
        "delete_memory",
        serde_json::json!({"id": a, "tenant_id": "project-b"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "memory not found");
    let ok = mcp(
        &app,
        ADMIN,
        None,
        "memory_history",
        serde_json::json!({"id": a, "tenant_id": "project-a"}),
    )
    .await
    .unwrap();
    assert_eq!(ok["count"], 2);
}

#[tokio::test]
async fn mcp_header_is_a_hard_scope_for_every_tool() {
    let app = app().await;
    // The argument says project-a, the header says project-b: the header wins.
    mcp(
        &app,
        ADMIN,
        Some("project-b"),
        "add_memory",
        serde_json::json!({"content": "lands in b", "tenant_id": "project-a"}),
    )
    .await
    .unwrap();
    // Reading as project-a while the argument asks for project-b: still project-a (empty).
    let in_a = mcp(
        &app,
        ADMIN,
        Some("project-a"),
        "get_memories",
        serde_json::json!({"tenant_id": "project-b"}),
    )
    .await
    .unwrap();
    assert_eq!(in_a["count"], 0);
    let in_b = mcp(
        &app,
        ADMIN,
        Some("project-b"),
        "get_memories",
        serde_json::json!({}),
    )
    .await
    .unwrap();
    assert_eq!(in_b["count"], 1);
}

#[tokio::test]
async fn a_tenant_bound_key_beats_the_header() {
    let app = app().await;
    let a = seed(&app, "project-a", "A ships on k8s").await;
    let b = seed(&app, "project-b", "B ships on vms").await;
    // `bound-key` is bound to project-a: a header naming project-b changes nothing.
    let err = mcp(
        &app,
        BOUND,
        Some("project-b"),
        "delete_memory",
        serde_json::json!({"id": b}),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "memory not found");
    let ok = mcp(
        &app,
        BOUND,
        Some("project-b"),
        "memory_history",
        serde_json::json!({"id": a}),
    )
    .await
    .unwrap();
    assert_eq!(ok["count"], 2);
}

#[tokio::test]
async fn rest_by_id_routes_honour_the_tenant_header() {
    let app = app().await;
    let a = seed(&app, "project-a", "A ships on k8s").await;
    let id = &a;

    for (method, suffix, body) in [
        ("GET", "", serde_json::Value::Null),
        ("GET", "/history", serde_json::Value::Null),
        ("GET", "/provenance", serde_json::Value::Null),
        ("PATCH", "", serde_json::json!({"content": "hijacked"})),
        ("POST", "/feedback", serde_json::json!({"verdict": "wrong"})),
        ("DELETE", "", serde_json::Value::Null),
    ] {
        let (status, _) = send(
            &app,
            ADMIN,
            Some("project-b"),
            method,
            &format!("/api/v1/memories/{id}{suffix}"),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {suffix}");
    }

    // Untouched, and reachable from its own tenant.
    let (status, got) = send(
        &app,
        ADMIN,
        Some("project-a"),
        "GET",
        &format!("/api/v1/memories/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(got["content"].as_str().unwrap().ends_with("v2"));
    let (status, hist) = send(
        &app,
        ADMIN,
        Some("project-a"),
        "GET",
        &format!("/api/v1/memories/{id}/history"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hist["count"], 2);
    let (status, _) = send(
        &app,
        ADMIN,
        Some("project-a"),
        "PATCH",
        &format!("/api/v1/memories/{id}"),
        serde_json::json!({"importance": 0.9}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &app,
        ADMIN,
        Some("project-a"),
        "DELETE",
        &format!("/api/v1/memories/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
