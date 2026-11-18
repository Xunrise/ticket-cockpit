//! The HTTP API on 127.0.0.1. It can type into Claude sessions, so only same-origin requests from this
//! machine are served: the Host must be localhost/127.0.0.1 and every non-GET needs `x-tk: 1`.
use crate::ctx::Ctx;
use crate::prompt::{self, Timing, TmuxPane};
use crate::tmux::PaneRef;
use crate::util::{iso_from_ms, ms_from_iso, now_ms};
use crate::{sessions, summary, tickets, tools};
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream, StreamExt};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

pub struct AppState {
    pub ctx: Ctx,
    tx: broadcast::Sender<String>,
    last: Mutex<String>,
    started_ms: i64,
    pub port: std::sync::atomic::AtomicU16,
}

impl AppState {
    pub fn new(ctx: Ctx) -> Arc<Self> {
        Arc::new(AppState {
            ctx,
            tx: broadcast::channel(16).0,
            last: Mutex::new(String::new()),
            started_ms: now_ms(),
            port: 0.into(),
        })
    }

    /// Publishes the summary to SSE listeners when it changed since the last tick. Hooks write the database
    /// from other processes, so the stream polls a digest instead of being pushed to.
    pub fn tick(&self) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let Ok(s) = summary::summary(&self.ctx) else { return };
        let s = s.to_string();
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if *last != s {
            *last = s.clone();
            let _ = self.tx.send(s);
        }
    }
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(ApiError::from)
}

fn host_ok(host: &str) -> bool {
    let Some((name, port)) = host.rsplit_once(':') else { return false };
    (name == "localhost" || name == "127.0.0.1") && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
}

async fn guard(req: Request, next: Next) -> Response {
    let host = req.headers().get("host").and_then(|h| h.to_str().ok()).unwrap_or("");
    if !host_ok(host) {
        return ApiError(StatusCode::FORBIDDEN, "bad host".into()).into_response();
    }
    if req.method() != axum::http::Method::GET && req.headers().get("x-tk").and_then(|h| h.to_str().ok()) != Some("1") {
        return ApiError(StatusCode::FORBIDDEN, "missing x-tk header".into()).into_response();
    }
    next.run(req).await
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/summary", get(summary_route))
        .route("/api/stream", get(stream_route))
        .route("/api/tools", get(|| async { Json(tools::describe()) }))
        .route("/api/tools/:name", post(tool_route))
        .route("/api/state", get(state_route))
        .route("/api/pane/:sid", get(pane_screen))
        .route("/api/pane/:sid/send", post(pane_send))
        .layer(middleware::from_fn(guard))
        .with_state(state)
}

async fn health(State(s): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "pid": std::process::id(), "port": s.port.load(std::sync::atomic::Ordering::Relaxed), "db": s.ctx.db_path,
        "started": iso_from_ms(s.started_ms), "uptime_s": (now_ms() - s.started_ms) / 1000, "version": env!("CARGO_PKG_VERSION"),
        "build": crate::util::build_id(), "commit": crate::update::installed_commit(),
    }))
}

async fn summary_route(State(s): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(blocking(move || summary::summary(&s.ctx)).await?))
}

async fn stream_route(State(s): State<Arc<AppState>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = s.tx.subscribe();
    let first = tokio::task::spawn_blocking(move || {
        summary::summary(&s.ctx).map(|v| v.to_string()).unwrap_or_else(|_| "{}".into())
    })
    .await
    .unwrap_or_default();
    let ev = |d: String| Ok(Event::default().event("summary").data(d));
    let head = stream::once(async move { ev(first) });
    let tail = BroadcastStream::new(rx)
        .filter_map(|m| async move { m.ok().map(|d| Ok(Event::default().event("summary").data(d))) });
    Sse::new(head.chain(tail)).keep_alive(KeepAlive::default())
}

async fn tool_route(State(s): State<Arc<AppState>>, Path(name): Path<String>, body: Bytes) -> ApiResult {
    if !tools::TOOLS.iter().any(|t| t.name == name) {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no tool {name}")));
    }
    let args: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            crate::errors::log_quiet(&s.ctx, "http", &format!("tool {name}: bad JSON: {e}"), "");
            ApiError(StatusCode::BAD_REQUEST, format!("bad JSON: {e}"))
        })?
    };
    Ok(Json(blocking(move || tools::call(&s.ctx, &name, args)).await?))
}

async fn state_route(State(s): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(blocking(move || state(&s.ctx)).await?))
}

/// Everything a frontend needs to draw the queue in one read.
pub fn state(ctx: &Ctx) -> anyhow::Result<Value> {
    let conn = ctx.db()?;
    let where_ = ctx.tmux.list_panes();
    let now = now_ms();
    let sessions: Vec<Value> = crate::db::rows(&conn, "SELECT * FROM sessions ORDER BY started DESC", [])?
        .into_iter()
        .map(|mut s| {
            let recent = s["ended"].as_str().is_none_or(|e| ms_from_iso(e).is_some_and(|m| now - m < 86_400_000));
            s["git"] = if recent {
                summary::git_info(
                    s["id"].as_str().unwrap_or(""),
                    s["cwd"].as_str().unwrap_or(""),
                    s["head_start"].as_str(),
                )
            } else {
                Value::Null
            };
            s
        })
        .collect();
    Ok(json!({
        "tickets": tickets::all(&conn)?,
        "sessions": sessions,
        "where": where_,
        "events": crate::db::rows(&conn, "SELECT * FROM events ORDER BY at, id", [])?,
        "repos": crate::db::rows(&conn, "SELECT * FROM repos ORDER BY name", [])?,
        "ready": tickets::ready(&conn, None)?.iter().map(|t| &t.key).collect::<Vec<_>>(),
        "panes": where_.keys().collect::<Vec<_>>(),
        "windows": crate::panes::windows(&ctx.tmux),
    }))
}

/// The live tmux pane of a session id or a ticket key.
fn pane_for(ctx: &Ctx, id: &str) -> anyhow::Result<Option<PaneRef>> {
    let conn = ctx.db()?;
    let sid = match tickets::find(&conn, id)? {
        Some(t) if t.session.is_some() => t.session.unwrap_or_default(),
        _ => id.to_string(),
    };
    Ok(sessions::live_pane(&conn, &ctx.tmux, &sid))
}

async fn pane_screen(State(s): State<Arc<AppState>>, Path(sid): Path<String>) -> ApiResult {
    blocking(move || {
        let screen = match pane_for(&s.ctx, &sid)? {
            Some(p) => Some(s.ctx.tmux.capture(&p, false, Some(60))?.trim_end().to_string()),
            None => None,
        };
        Ok(json!({"screen": screen}))
    })
    .await
    .map(Json)
}

async fn pane_send(State(s): State<Arc<AppState>>, Path(sid): Path<String>, Json(body): Json<Value>) -> ApiResult {
    blocking(move || {
        let r = (|| -> anyhow::Result<Value> {
            let pane = pane_for(&s.ctx, &sid)?.ok_or(prompt::SendRefused::NoPane)?;
            let screen = TmuxPane { tmux: &s.ctx.tmux, pane };
            if let Some(text) = body["text"].as_str().filter(|t| !t.is_empty()) {
                prompt::send_text(&screen, text, prompt::vim_mode(&s.ctx.claude_settings), &Timing::default())?;
            }
            let keys: Vec<String> = body["keys"]
                .as_array()
                .map(|k| k.iter().filter_map(|k| k.as_str().map(String::from)).collect())
                .unwrap_or_default();
            prompt::send_keys(&screen, &keys)?;
            Ok(json!({"ok": true}))
        })();
        if let Err(e) = &r {
            if !crate::errors::expected(e) {
                crate::errors::log_quiet(&s.ctx, "http", &format!("send to {sid}: {e:#}"), "");
            }
        }
        r
    })
    .await
    .map(Json)
}

/// Serves until the process ends. Also refreshes the SSE digest every second and the call info every 5 minutes.
pub async fn serve(state: Arc<AppState>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    let st = state.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            t.tick().await;
            let st = st.clone();
            let _ = tokio::task::spawn_blocking(move || st.tick()).await;
        }
    });
    let st = state.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            t.tick().await;
            let st = st.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let fresh = crate::palaver::calls(&st.ctx.palaver_bin, &st.ctx.coach_file);
                if let Ok(mut g) = st.ctx.calls.lock() {
                    *g = fresh;
                }
            })
            .await;
        }
    });
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use crate::tmux::Tmux;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app(tag: &str) -> (Dir, Arc<AppState>, Router) {
        let d = Dir::new();
        let state = AppState::new(Ctx::for_test(d.path(), Tmux::private(tag)));
        let r = router(state.clone());
        (d, state, r)
    }

    fn req(method: &str, uri: &str, host: Option<&str>, tk: bool, body: &str) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(h) = host {
            b = b.header("host", h);
        }
        if tk {
            b = b.header("x-tk", "1");
        }
        b.header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()
    }

    async fn send(r: &Router, rq: Request<Body>) -> (StatusCode, Value) {
        let res = r.clone().oneshot(rq).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn rejects_foreign_hosts_and_unguarded_writes() {
        let (_d, _s, r) = app("http-guard");
        for host in [
            None,
            Some("evil.example.com"),
            Some("localhost.evil.com:80"),
            Some("localhost"),
            Some("127.0.0.1.evil:1"),
            Some("example.com:4747"),
        ] {
            let (st, body) = send(&r, req("GET", "/api/summary", host, false, "")).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{host:?}");
            assert_eq!(body["error"], "bad host");
        }
        let (st, body) = send(&r, req("POST", "/api/tools/settings.get", Some("localhost:4747"), false, "{}")).await;
        assert_eq!((st, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("missing x-tk header")));
        let (st, _) = send(&r, req("POST", "/api/tools/settings.get", Some("localhost:4747"), true, "{}")).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = send(&r, req("GET", "/api/summary", Some("127.0.0.1:4747"), false, "")).await;
        assert_eq!(st, StatusCode::OK, "GET needs no x-tk");
    }

    #[tokio::test]
    async fn serves_tools_summary_state_and_errors() {
        let (_d, s, r) = app("http-tools");
        let h = Some("localhost:1");
        let (st, tools) = send(&r, req("GET", "/api/tools", h, false, "")).await;
        assert_eq!(st, StatusCode::OK);
        assert!(tools
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "ticket.for_change" && t["params"]["branch"].is_string()));
        let conn = s.ctx.db().unwrap();
        tickets::create(
            &conn,
            tickets::NewTicket {
                repo: "demo".into(),
                title: "first".into(),
                project: Some("P".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let (_, sum) = send(&r, req("GET", "/api/summary", h, false, "")).await;
        assert_eq!(sum["ready"][0]["key"], "DEM-1");
        assert!(sum["focus"].is_object() && sum["calls"].is_object());
        let (_, state) = send(&r, req("GET", "/api/state", h, false, "")).await;
        assert_eq!(state["tickets"][0]["key"], "DEM-1");
        assert_eq!(state["ready"], json!(["DEM-1"]));
        assert!(state["sessions"].is_array() && state["where"].is_object());
        let (st, ov) = send(&r, req("POST", "/api/tools/project.overview", h, true, r#"{"project":"P"}"#)).await;
        assert_eq!((st, ov["todo"][0]["key"].as_str()), (StatusCode::OK, Some("DEM-1")));
        let (st, e) = send(&r, req("POST", "/api/tools/project.overview", h, true, r#"{"project":"nope"}"#)).await;
        assert_eq!((st, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("no project nope")));
        let (st, _) = send(&r, req("POST", "/api/tools/nope.nothing", h, true, "{}")).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = send(&r, req("POST", "/api/tools/settings.get", h, true, "{not json")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, hb) = send(&r, req("GET", "/api/health", h, false, "")).await;
        assert_eq!((st, hb["pid"].as_u64()), (StatusCode::OK, Some(std::process::id() as u64)));
        assert_eq!(hb["build"], crate::util::build_id());
        assert_eq!(hb["commit"], crate::update::installed_commit());
    }

    #[tokio::test]
    async fn pane_endpoints_report_missing_panes() {
        let (_d, _s, r) = app("http-pane");
        let h = Some("localhost:1");
        let (st, b) = send(&r, req("GET", "/api/pane/unknown", h, false, "")).await;
        assert_eq!((st, b["screen"].is_null()), (StatusCode::OK, true));
        let (st, b) = send(&r, req("POST", "/api/pane/unknown/send", h, true, r#"{"text":"hi"}"#)).await;
        assert_eq!((st, b["error"].as_str()), (StatusCode::BAD_REQUEST, Some("no live tmux pane for this session")));
    }

    #[tokio::test]
    async fn pane_send_refuses_a_draft_through_a_real_pane() {
        let Some(g) = crate::tmux::testutil::guard() else { return };
        let d = Dir::new();
        let ctx = Ctx::for_test(d.path(), g.0.clone());
        let fixtures = format!("{}/tests/fixtures/prompt", env!("CARGO_MANIFEST_DIR"));
        crate::panes::cockpit_window(&g.0, "W", "/tmp", &[], &format!("cat {fixtures}/draft.txt; sleep 30")).unwrap();
        let id =
            g.0.run(crate::tmux::Server::Cockpit, &["list-panes", "-t", "=cockpit:W", "-F", "#{pane_id}"]).unwrap();
        let conn = ctx.db().unwrap();
        conn.execute(
            "INSERT INTO sessions(id,repo,cwd,tmux_pane,state) VALUES('s1','r','/tmp',?, 'turn')",
            [format!("cockpit:{id}")],
        )
        .unwrap();
        let r = router(AppState::new(ctx));
        assert!(crate::tmux::testutil::wait_until(|| g
            .0
            .capture(&PaneRef::cockpit(&id), false, None)
            .unwrap()
            .contains("login bug")));
        let (st, b) =
            send(&r, req("POST", "/api/pane/s1/send", Some("localhost:1"), true, r#"{"text":"continue"}"#)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(b["error"].as_str().unwrap().contains("fix the login bug"), "{b}");
        let (_, screen) = send(&r, req("GET", "/api/pane/s1", Some("localhost:1"), false, "")).await;
        assert!(screen["screen"].as_str().unwrap().contains("fix the login bug"));
        let (st, b) = send(&r, req("POST", "/api/pane/s1/send", Some("localhost:1"), true, r#"{"keys":["x"]}"#)).await;
        assert_eq!((st, b["error"].as_str()), (StatusCode::BAD_REQUEST, Some("unsupported key x")));
    }

    async fn next_event(body: &mut Body) -> String {
        let f = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame()).await.unwrap().unwrap().unwrap();
        String::from_utf8_lossy(&f.into_data().unwrap()).into_owned()
    }

    #[tokio::test]
    async fn stream_sends_on_connect_and_when_the_summary_changes() {
        let (_d, s, r) = app("http-sse");
        let res = r.clone().oneshot(req("GET", "/api/stream", Some("localhost:1"), false, "")).await.unwrap();
        assert_eq!(res.headers()["content-type"], "text/event-stream");
        let mut body = res.into_body();
        let first = next_event(&mut body).await;
        assert!(first.starts_with("event: summary\ndata: {"), "{first}");
        s.tick(); // first digest
        assert!(next_event(&mut body).await.contains("\"ready\":[]"));
        s.tick(); // unchanged: nothing is sent
        let conn = s.ctx.db().unwrap();
        tickets::create(
            &conn,
            tickets::NewTicket { repo: "demo".into(), title: "new work".into(), ..Default::default() },
        )
        .unwrap();
        s.tick();
        assert!(next_event(&mut body).await.contains("new work"));
    }
}
