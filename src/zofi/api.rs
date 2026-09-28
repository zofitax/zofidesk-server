//! The HTTP API the RustDesk client already speaks: login, session checks, the legacy address
//! book and the device heartbeat. Routes the client probes but ZofiDesk does not implement yet
//! answer in the shape that makes the client fall back quietly.

use super::store::Db;
use axum::{
    body::Bytes,
    extract::Extension,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router, TypedHeader,
};
use headers::{authorization::Bearer, Authorization};
use hbb_common::{log, tokio, ResultType};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};

type Auth = Option<TypedHeader<Authorization<Bearer>>>;

const LAST_SEEN_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

pub async fn serve(addr: SocketAddr, db: Db) -> ResultType<()> {
    let flush_db = db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(LAST_SEEN_FLUSH_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(err) = flush_db.flush_last_seen().await {
                log::error!("Failed to save device last-seen times: {err}");
            }
        }
    });
    let server = axum::Server::try_bind(&addr)?.serve(router(db).into_make_service());
    log::info!("ZofiDesk API listening on {addr}");
    server.await?;
    Ok(())
}

pub(crate) fn router(db: Db) -> Router {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/currentUser", post(current_user))
        .route("/api/login-options", get(login_options))
        .route("/api/heartbeat", post(heartbeat))
        .route("/api/sysinfo", post(sysinfo))
        .route("/api/sysinfo_ver", post(sysinfo_ver))
        .route("/api/ab", get(get_address_book).post(save_address_book))
        .route("/api/users", get(empty_list))
        .route("/api/peers", get(empty_list))
        .route("/api/device-group/accessible", get(empty_list))
        .layer(Extension(db))
}

async fn login(Extension(db): Extension<Db>, body: Bytes) -> Response {
    let req = parse_body(&body);
    let login_type = str_field(&req, "type");
    if !login_type.is_empty() && login_type != "account" {
        return error(StatusCode::OK, "Tipo de inicio de sesión no soportado");
    }
    let username = str_field(&req, "username");
    let password = str_field(&req, "password");
    let user = match db.check_password(&username, &password).await {
        Ok(Some(user)) => user,
        Ok(None) => return error(StatusCode::OK, "Usuario o contraseña incorrectos"),
        Err(err) => return internal_error(err),
    };
    if !user.active {
        return error(StatusCode::OK, "Esta cuenta está desactivada");
    }
    match db
        .create_token(user.id, &str_field(&req, "id"), &str_field(&req, "uuid"))
        .await
    {
        Ok(token) => {
            log::info!("User {} signed in from device {}", user.username, str_field(&req, "id"));
            json_response(json!({
                "type": "access_token",
                "access_token": token,
                "user": user_json(&user),
            }))
        }
        Err(err) => internal_error(err),
    }
}

async fn logout(Extension(db): Extension<Db>, auth: Auth) -> Response {
    if let Some(TypedHeader(Authorization(bearer))) = auth {
        if let Err(err) = db.revoke_token(bearer.token()).await {
            return internal_error(err);
        }
    }
    json_response(json!({}))
}

async fn current_user(Extension(db): Extension<Db>, auth: Auth) -> Response {
    match authenticate(&db, auth).await {
        Ok(user) => json_response(user_json(&user)),
        Err(response) => response,
    }
}

async fn login_options() -> Response {
    json_response(json!([]))
}

async fn heartbeat(Extension(db): Extension<Db>, body: Bytes) -> Response {
    let id = str_field(&parse_body(&body), "id");
    if id.is_empty() {
        return json_response(json!({}));
    }
    match db.touch_device(&id).await {
        // Ask the client to upload its system info, e.g. after the database was reset.
        Ok(true) => json_response(json!({ "sysinfo": true })),
        Ok(false) => json_response(json!({})),
        Err(err) => internal_error(err),
    }
}

async fn sysinfo(Extension(db): Extension<Db>, body: Bytes) -> Response {
    let id = str_field(&parse_body(&body), "id");
    if id.is_empty() {
        return text_response("ID_NOT_FOUND");
    }
    match db.save_device_info(&id, &String::from_utf8_lossy(&body)).await {
        Ok(()) => text_response("SYSINFO_UPDATED"),
        Err(err) => internal_error(err),
    }
}

// The client skips re-uploading unchanged system info while this value stays the same.
async fn sysinfo_ver() -> Response {
    text_response("1")
}

async fn get_address_book(Extension(db): Extension<Db>, auth: Auth) -> Response {
    let user = match authenticate(&db, auth).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    match db.load_address_book(user.id).await {
        Ok(Some(data)) => json_response(json!({ "data": data })),
        Ok(None) => json_response(Value::Null),
        Err(err) => internal_error(err),
    }
}

async fn save_address_book(Extension(db): Extension<Db>, auth: Auth, body: Bytes) -> Response {
    let user = match authenticate(&db, auth).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let data = str_field(&parse_body(&body), "data");
    match db.save_address_book(user.id, &data).await {
        Ok(()) => json_response(Value::Null),
        Err(err) => internal_error(err),
    }
}

async fn empty_list() -> Response {
    json_response(json!({ "total": 0, "data": [] }))
}

async fn authenticate(db: &Db, auth: Auth) -> Result<super::store::User, Response> {
    let token = match auth {
        Some(TypedHeader(Authorization(bearer))) => bearer.token().to_owned(),
        None => return Err(error(StatusCode::UNAUTHORIZED, "Sesión no iniciada")),
    };
    match db.user_for_token(&token).await {
        Ok(Some(user)) => Ok(user),
        Ok(None) => Err(error(StatusCode::UNAUTHORIZED, "La sesión ha caducado")),
        Err(err) => Err(internal_error(err)),
    }
}

fn user_json(user: &super::store::User) -> Value {
    json!({
        "name": user.username,
        "display_name": user.display_name,
        "email": "",
        "note": "",
        "status": if user.active { 1 } else { 0 },
        "is_admin": user.is_admin,
    })
}

// The client does not always send `Content-Type: application/json`, so bodies are parsed by hand.
fn parse_body(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn str_field(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn json_response(value: Value) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

fn text_response(text: &'static str) -> Response {
    (StatusCode::OK, text).into_response()
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        json!({ "error": message }).to_string(),
    )
        .into_response()
}

fn internal_error(err: hbb_common::anyhow::Error) -> Response {
    log::error!("ZofiDesk API error: {err}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "Error interno del servidor")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zofi::store::tests::temp_db;

    struct TestServer {
        base: String,
        client: reqwest::Client,
    }

    async fn start(db: Db) -> TestServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = axum::Server::from_tcp(listener)
            .unwrap()
            .serve(router(db).into_make_service());
        tokio::spawn(server);
        TestServer {
            base: format!("http://{addr}"),
            client: reqwest::Client::new(),
        }
    }

    impl TestServer {
        async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (u16, String) {
            let mut req = self.client.post(format!("{}{path}", self.base)).body(body.to_string());
            if let Some(token) = token {
                req = req.bearer_auth(token);
            }
            let resp = req.send().await.unwrap();
            (resp.status().as_u16(), resp.text().await.unwrap())
        }

        async fn get(&self, path: &str, token: Option<&str>) -> (u16, String) {
            let mut req = self.client.get(format!("{}{path}", self.base));
            if let Some(token) = token {
                req = req.bearer_auth(token);
            }
            let resp = req.send().await.unwrap();
            (resp.status().as_u16(), resp.text().await.unwrap())
        }

        async fn login(&self, username: &str, password: &str) -> Value {
            let body = json!({ "username": username, "password": password, "id": "111", "uuid": "u" });
            let (status, text) = self.post("/api/login", None, body).await;
            assert_eq!(status, 200);
            serde_json::from_str(&text).unwrap()
        }
    }

    #[tokio::test]
    async fn login_session_and_logout() {
        let temp = temp_db().await;
        temp.db.add_user("luis", "password1", true).await.unwrap();
        let server = start(temp.db.clone()).await;

        let failed = server.login("luis", "wrong-pass").await;
        assert!(failed["error"].is_string());

        let ok = server.login("LUIS", "password1").await;
        assert_eq!(ok["type"], "access_token");
        assert_eq!(ok["user"]["name"], "luis");
        assert_eq!(ok["user"]["is_admin"], true);
        let token = ok["access_token"].as_str().unwrap().to_owned();

        let (status, text) = server.post("/api/currentUser", Some(&token), json!({})).await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["name"], "luis");

        server.post("/api/logout", Some(&token), json!({})).await;
        let (status, _) = server.post("/api/currentUser", Some(&token), json!({})).await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn disabled_account_cannot_sign_in() {
        let temp = temp_db().await;
        temp.db.add_user("ana", "password1", false).await.unwrap();
        temp.db.set_active("ana", false).await.unwrap();
        let server = start(temp.db.clone()).await;
        assert!(server.login("ana", "password1").await["error"].is_string());
    }

    #[tokio::test]
    async fn address_book_requires_session() {
        let temp = temp_db().await;
        temp.db.add_user("ana", "password1", false).await.unwrap();
        let server = start(temp.db.clone()).await;
        assert_eq!(server.get("/api/ab", None).await.0, 401);

        let token = server.login("ana", "password1").await["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(server.get("/api/ab", Some(&token)).await, (200, "null".to_owned()));
        let (status, _) = server
            .post("/api/ab", Some(&token), json!({ "data": "{\"peers\":[]}" }))
            .await;
        assert_eq!(status, 200);
        let (_, text) = server.get("/api/ab", Some(&token)).await;
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["data"], "{\"peers\":[]}");
    }

    #[tokio::test]
    async fn heartbeat_asks_for_sysinfo_once() {
        let temp = temp_db().await;
        let server = start(temp.db.clone()).await;
        let (_, text) = server.post("/api/heartbeat", None, json!({ "id": "111" })).await;
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["sysinfo"], true);
        let (_, text) = server
            .post("/api/sysinfo", None, json!({ "id": "111", "os": "Windows" }))
            .await;
        assert_eq!(text, "SYSINFO_UPDATED");
        let (_, text) = server.post("/api/heartbeat", None, json!({ "id": "111" })).await;
        assert_eq!(text, "{}");
    }

    #[tokio::test]
    async fn unimplemented_features_fall_back_quietly() {
        let temp = temp_db().await;
        let server = start(temp.db.clone()).await;
        assert_eq!(server.get("/api/ab/settings", None).await.0, 404);
        let (status, text) = server.get("/api/users?current=1&pageSize=100", None).await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["total"], 0);
        assert_eq!(server.get("/api/login-options", None).await.1, "[]");
    }
}
