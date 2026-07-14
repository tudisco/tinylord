//! Black-box integration tests (§17).
//!
//! Each test spawns the real `tinylord` binary (so the full SQLCipher /
//! encryption path is exercised), talks to it over HTTP, and tears it down.
//! An admin principal is created offline first (a process that exits, so its
//! token is captured reliably without stdout-buffering issues), then the server
//! is started reusing the same `_system.db`.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_tinylord");

struct Server {
    child: Child,
    base: String,
    admin: String,
    // Held only to keep the temp dir (and thus the DB files) alive for the test.
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

async fn start_server(max_document_bytes: u64) -> Server {
    start_server_ext(max_document_bytes, "").await
}

/// Like `start_server`, but appends `extra_cfg` to the generated `tinylord.toml`
/// so a test can add optional sections (e.g. `[pubsub]`).
async fn start_server_ext(max_document_bytes: u64, extra_cfg: &str) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let data_dir = dir.path().join("data");
    let snap_dir = dir.path().join("snap");
    let key_file = dir.path().join("k.key");
    let config_path = dir.path().join("tinylord.toml");

    let cfg = format!(
        r#"
[server]
bind = "127.0.0.1:{port}"
data_dir = "{data}"
snapshot_dir = "{snap}"

[limits]
max_document_bytes = {maxdoc}
max_query_limit = 100
rate_per_minute = 0

[encryption]
enabled = true
key_source = "key_file"
key_file = "{key}"

[cors]
allowed_origins = ["http://localhost:5173"]

[auth]
public_registration = true
secure_cookies = false
{extra}
"#,
        data = data_dir.display(),
        snap = snap_dir.display(),
        key = key_file.display(),
        maxdoc = max_document_bytes,
        extra = extra_cfg,
    );
    std::fs::write(&config_path, cfg).unwrap();
    let config_path = config_path.to_string_lossy().to_string();

    // Create the admin offline; capture the token from the exited process.
    let out = Command::new(BIN)
        .args(["--config", &config_path, "admin", "create-user", "--name", "admin", "--admin"])
        .output()
        .expect("run create-user");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let admin = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("token (shown once): ").map(str::to_string))
        .unwrap_or_else(|| panic!("no token in create-user output: {stdout}\n{}", String::from_utf8_lossy(&out.stderr)));

    // Start the server.
    let child = Command::new(BIN)
        .args(["--config", &config_path, "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");

    let base = format!("http://127.0.0.1:{port}");
    // Wait for health.
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if let Ok(r) = client.get(format!("{base}/health")).send().await {
            if r.status().is_success() {
                return Server { child, base, admin, _dir: dir };
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("server did not become healthy");
}

impl Server {
    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    /// Create a database and a principal with the given role on it; returns the
    /// principal's token.
    async fn provision(&self, db: &str, role: &str) -> String {
        let c = self.client();
        c.post(format!("{}/v1/admin/databases", self.base))
            .bearer_auth(&self.admin)
            .json(&serde_json::json!({ "name": db }))
            .send()
            .await
            .unwrap();
        let user: serde_json::Value = c
            .post(format!("{}/v1/admin/principals", self.base))
            .bearer_auth(&self.admin)
            .json(&serde_json::json!({ "name": "u" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let pid = user["id"].as_str().unwrap().to_string();
        let token = user["token"].as_str().unwrap().to_string();
        c.post(format!("{}/v1/admin/grants", self.base))
            .bearer_auth(&self.admin)
            .json(&serde_json::json!({ "principal_id": pid, "database": db, "role": role }))
            .send()
            .await
            .unwrap();
        token
    }
}

#[tokio::test]
async fn crud_and_roles() {
    let s = start_server(1_048_576).await;
    let token = s.provision("app", "write").await;
    let c = s.client();

    // Create.
    let created: serde_json::Value = c
        .post(format!("{}/v1/db/app/collections/users/documents", s.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "name": "Ada", "age": 36 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["doc"]["name"], "Ada");
    assert!(created["created_at"].as_i64().unwrap() > 0);

    // Get.
    let got: serde_json::Value = c
        .get(format!("{}/v1/db/app/collections/users/documents/{id}", s.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["doc"]["age"], 36);

    // Query.
    let q: serde_json::Value = c
        .post(format!("{}/v1/db/app/collections/users/query", s.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "filter": { "age": { "$gte": 18 } } }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(q["items"].as_array().unwrap().len(), 1);

    // A read-only token cannot write.
    let ro = s.provision("app2", "read").await;
    let status = c
        .post(format!("{}/v1/db/app2/collections/users/documents", s.base))
        .bearer_auth(&ro)
        .json(&serde_json::json!({ "x": 1 }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 403);

    // Delete then 404.
    let del = c
        .delete(format!("{}/v1/db/app/collections/users/documents/{id}", s.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(del, 204);
    let missing = c
        .get(format!("{}/v1/db/app/collections/users/documents/{id}", s.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(missing, 404);
}

#[tokio::test]
async fn unique_index_conflict() {
    let s = start_server(1_048_576).await;
    let token = s.provision("app", "admin").await;
    let c = s.client();

    c.post(format!("{}/v1/db/app/collections/u/documents", s.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "email": "a@x.io" }))
        .send()
        .await
        .unwrap();

    let idx = c
        .post(format!("{}/v1/db/app/collections/u/indexes", s.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "path": "$.email", "unique": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(idx.status(), 201);

    // Duplicate email → 409.
    let dup = c
        .post(format!("{}/v1/db/app/collections/u/documents", s.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "email": "a@x.io" }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), 409);
}

#[tokio::test]
async fn document_size_limit() {
    let s = start_server(512).await; // tiny limit
    let token = s.provision("app", "write").await;
    let c = s.client();

    let big = "x".repeat(2000);
    let status = c
        .post(format!("{}/v1/db/app/collections/c/documents", s.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "blob": big }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 413);
}

#[tokio::test]
async fn auth_errors() {
    let s = start_server(1_048_576).await;
    let c = s.client();
    // No token → 401.
    let unauth = c
        .post(format!("{}/v1/db/app/collections/c/count", s.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(unauth, 401);

    // Valid token but nonexistent db → 404.
    let token = s.provision("real", "read").await;
    let notfound = c
        .post(format!("{}/v1/db/ghost/collections/c/count", s.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(notfound, 404);
}

#[tokio::test]
async fn admin_principal_listing_is_safe_and_includes_grants() {
    let s = start_server(1_048_576).await;
    let c = s.client();
    c.post(format!("{}/v1/admin/databases", s.base))
        .bearer_auth(&s.admin)
        .json(&serde_json::json!({ "name": "app" }))
        .send()
        .await
        .unwrap();
    let created: serde_json::Value = c
        .post(format!("{}/v1/admin/principals", s.base))
        .bearer_auth(&s.admin)
        .json(&serde_json::json!({ "name": "ada", "password": "long-enough-password" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();
    c.post(format!("{}/v1/admin/grants", s.base))
        .bearer_auth(&s.admin)
        .json(&serde_json::json!({ "principal_id": id, "database": "app", "role": "write" }))
        .send()
        .await
        .unwrap();

    let listed: serde_json::Value = c
        .get(format!("{}/v1/admin/principals?name=ADA", s.base))
        .bearer_auth(&s.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user = &listed["principals"][0];
    assert_eq!(user["id"], id);
    assert_eq!(user["kind"], "browser");
    assert_eq!(user["grants"][0]["database"], "app");
    assert!(user.get("password").is_none());
    assert!(user.get("token").is_none());

    let token = s.provision("other", "read").await;
    assert_eq!(
        c.get(format!("{}/v1/admin/principals", s.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}

#[tokio::test]
async fn browser_login_refresh_logout_and_grants() {
    let s = start_server(1_048_576).await;
    let c = s.client();
    let login = c.post(format!("{}/v1/auth/register", s.base))
        .json(&serde_json::json!({ "username": "delegate", "password": "long-enough-password" })).send().await.unwrap();
    assert_eq!(login.status(), 200);
    let refresh = set_cookie(login.headers(), "tinylord_refresh");
    let csrf_cookie = set_cookie(login.headers(), "tinylord_csrf");
    assert!(login.headers().get_all("set-cookie").iter().any(|value| value.to_str().is_ok_and(|cookie| cookie.starts_with("tinylord_csrf=") && cookie.contains("Path=/;"))));
    let body: serde_json::Value = login.json().await.unwrap();
    let access = body["access_token"].as_str().unwrap();
    let me: serde_json::Value = c.get(format!("{}/v1/auth/me", s.base)).bearer_auth(access).send().await.unwrap().json().await.unwrap();
    let id = me["id"].as_str().unwrap();

    c.post(format!("{}/v1/admin/databases", s.base)).bearer_auth(&s.admin).json(&serde_json::json!({"name":"private"})).send().await.unwrap();
    c.post(format!("{}/v1/admin/grants", s.base)).bearer_auth(&s.admin).json(&serde_json::json!({"principal_id": id, "database":"private", "role":"write"})).send().await.unwrap();
    assert_eq!(c.post(format!("{}/v1/db/private/collections/workspace/documents", s.base)).bearer_auth(access).json(&serde_json::json!({"ok":true})).send().await.unwrap().status(), 201);

    let refreshed = c.post(format!("{}/v1/auth/refresh", s.base)).header("cookie", format!("{refresh}; {csrf_cookie}")).header("x-csrf-token", csrf_cookie.strip_prefix("tinylord_csrf=").unwrap()).send().await.unwrap();
    assert_eq!(refreshed.status(), 200);
    let new_refresh = set_cookie(refreshed.headers(), "tinylord_refresh");
    let new_csrf_cookie = set_cookie(refreshed.headers(), "tinylord_csrf");
    let refreshed_body: serde_json::Value = refreshed.json().await.unwrap();
    assert_ne!(refreshed_body["access_token"], body["access_token"]);
    assert_eq!(c.post(format!("{}/v1/auth/refresh", s.base)).header("cookie", format!("{refresh}; {csrf_cookie}")).header("x-csrf-token", csrf_cookie.strip_prefix("tinylord_csrf=").unwrap()).send().await.unwrap().status(), 401);
    assert_eq!(c.post(format!("{}/v1/auth/logout", s.base)).header("cookie", format!("{new_refresh}; {new_csrf_cookie}")).header("x-csrf-token", new_csrf_cookie.strip_prefix("tinylord_csrf=").unwrap()).send().await.unwrap().status(), 204);
    for _ in 0..5 {
        assert_eq!(c.post(format!("{}/v1/auth/login", s.base)).json(&serde_json::json!({ "username": "delegate", "password": "wrong-password" })).send().await.unwrap().status(), 401);
    }
    assert_eq!(c.post(format!("{}/v1/auth/login", s.base)).json(&serde_json::json!({ "username": "delegate", "password": "wrong-password" })).send().await.unwrap().status(), 429);

    let registration: serde_json::Value = c.get(format!("{}/v1/admin/auth/registration", s.base)).bearer_auth(&s.admin).send().await.unwrap().json().await.unwrap();
    assert_eq!(registration["enabled"], true);
    assert_eq!(c.put(format!("{}/v1/admin/auth/registration", s.base)).bearer_auth(&s.admin).json(&serde_json::json!({ "enabled": false })).send().await.unwrap().status(), 200);
    assert_eq!(c.post(format!("{}/v1/auth/register", s.base)).json(&serde_json::json!({ "username": "blocked-user", "password": "long-enough-password" })).send().await.unwrap().status(), 403);
    assert_eq!(c.put(format!("{}/v1/admin/auth/registration", s.base)).bearer_auth(&s.admin).json(&serde_json::json!({ "enabled": true })).send().await.unwrap().status(), 200);

    let reset_login = c.post(format!("{}/v1/auth/register", s.base)).json(&serde_json::json!({ "username": "reset-user", "password": "long-enough-password" })).send().await.unwrap();
    let reset_access = reset_login.json::<serde_json::Value>().await.unwrap()["access_token"].as_str().unwrap().to_string();
    assert_eq!(c.post(format!("{}/v1/admin/principals/password", s.base)).bearer_auth(&s.admin).json(&serde_json::json!({ "name": "reset-user", "password": "replacement-password" })).send().await.unwrap().status(), 200);
    assert_eq!(c.get(format!("{}/v1/auth/me", s.base)).bearer_auth(&reset_access).send().await.unwrap().status(), 401);
    assert_eq!(c.post(format!("{}/v1/auth/login", s.base)).json(&serde_json::json!({ "username": "reset-user", "password": "replacement-password" })).send().await.unwrap().status(), 200);
}

fn set_cookie(headers: &reqwest::header::HeaderMap, name: &str) -> String {
    headers
        .get_all("set-cookie")
        .iter()
        .find_map(|value| {
            let cookie = value.to_str().ok()?.split(';').next()?;
            cookie.starts_with(&format!("{name}=")).then(|| cookie.to_string())
        })
        .unwrap_or_else(|| panic!("missing {name} cookie"))
}

#[tokio::test]
async fn static_apps_are_isolated_and_keep_api_routes() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(first.join("index.html"), "first app").unwrap();
    std::fs::write(first.join("asset.abc.js"), "one").unwrap();
    std::fs::write(second.join("index.html"), "second app").unwrap();
    let api_port = free_port(); let first_port = free_port(); let second_port = free_port();
    let cfg = dir.path().join("tinylord.toml");
    let cfg_text = format!(
        "[server]\nbind=\"127.0.0.1:{api_port}\"\ndata_dir=\"{}\"\nsnapshot_dir=\"{}\"\n[encryption]\nenabled=true\nkey_source=\"key_file\"\nkey_file=\"{}\"\n[[static_apps]]\nname=\"first\"\nbind=\"127.0.0.1:{first_port}\"\ndirectory=\"{}\"\nspa_fallback=true\n[[static_apps]]\nname=\"second\"\nbind=\"127.0.0.1:{second_port}\"\ndirectory=\"{}\"\nspa_fallback=true\n",
        dir.path().join("data").display(), dir.path().join("snap").display(), dir.path().join("key").display(), first.display(), second.display(),
    );
    std::fs::write(&cfg, cfg_text).unwrap();
    let mut child = Command::new(BIN).args(["--config", cfg.to_str().unwrap(), "serve"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let c = reqwest::Client::new(); let first_base = format!("http://127.0.0.1:{first_port}");
    let mut ready = false;
    for _ in 0..100 { if c.get(format!("{first_base}/health")).send().await.map(|r| r.status().is_success()).unwrap_or(false) { ready = true; break; } tokio::time::sleep(Duration::from_millis(50)).await; }
    assert!(ready, "static app did not become ready");
    let library = c.get(format!("{first_base}/tinylord.js")).send().await.unwrap();
    assert_eq!(library.status(), 200);
    assert!(library.headers().get("content-type").unwrap().to_str().unwrap().starts_with("text/javascript"));
    assert_eq!(c.get(format!("{first_base}/asset.abc.js")).send().await.unwrap().text().await.unwrap(), "one");
    assert_eq!(c.get(format!("{first_base}/dashboard/today")).send().await.unwrap().text().await.unwrap(), "first app");
    assert_eq!(c.get(format!("http://127.0.0.1:{second_port}/dashboard")).send().await.unwrap().text().await.unwrap(), "second app");
    assert_eq!(c.get(format!("{first_base}/v1/unknown")).send().await.unwrap().status(), 404);
    assert_eq!(c.get(format!("{first_base}/%2e%2e/second/index.html")).send().await.unwrap().status(), 404);
    let _ = child.kill(); let _ = child.wait();
}

#[tokio::test]
async fn concurrency_serializes_without_busy() {
    // Hammer one database from many concurrent clients; every write must succeed
    // (the single writer serializes them; SQLITE_BUSY never surfaces).
    let s = start_server(1_048_576).await;
    let token = s.provision("app", "write").await;
    let base = s.base.clone();

    let mut handles = Vec::new();
    for i in 0..60 {
        let token = token.clone();
        let base = base.clone();
        handles.push(tokio::spawn(async move {
            let c = reqwest::Client::new();
            c.post(format!("{base}/v1/db/app/collections/hammer/documents"))
                .bearer_auth(&token)
                .json(&serde_json::json!({ "i": i }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), 201);
    }

    let cnt: serde_json::Value = s
        .client()
        .post(format!("{}/v1/db/app/collections/hammer/count", s.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cnt["count"], 60);
}

#[tokio::test]
async fn realtime_event_and_seq() {
    use futures::StreamExt;
    let s = start_server(1_048_576).await;
    let token = s.provision("app", "write").await;
    let base = s.base.clone();

    // Open the SSE stream.
    let resp = s
        .client()
        .get(format!("{base}/v1/db/app/collections/live/subscribe"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let mut stream = resp.bytes_stream();

    // Give the subscriber a moment, then write.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let token2 = token.clone();
    let base2 = base.clone();
    tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{base2}/v1/db/app/collections/live/documents"))
            .bearer_auth(&token2)
            .json(&serde_json::json!({ "hello": "world" }))
            .send()
            .await
            .unwrap();
    });

    // Read until we see a change event or time out.
    let mut buf = String::new();
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if buf.contains("event: change") && buf.contains("\"hello\":\"world\"") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(got, "did not receive change event; buffer was: {buf}");
    assert!(buf.contains("\"op\":\"insert\""));
    assert!(buf.contains("\"seq\":"));
}

/// Correctness guard (§17): the source must never use `INSERT OR REPLACE` and
/// must never issue a `DELETE FROM coll_...` without a WHERE — either would drop
/// update_hook events.
#[test]
fn source_avoids_hook_breaking_sql() {
    let writer = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/db/writer.rs"))
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        !writer.contains("insert or replace"),
        "INSERT OR REPLACE would skip the update_hook (§5.3)"
    );
    // Every delete against a collection table must carry an explicit WHERE.
    for line in writer.lines() {
        let l = line.trim().to_ascii_lowercase();
        if l.contains("delete from coll_") {
            assert!(
                l.contains("where"),
                "DELETE on a collection must use WHERE: {line}"
            );
        }
    }
}

/// Encryption (§20.8): a wrong key fails cleanly and never echoes the key.
/// Pure CLI test (no server); the temp dir is held for the whole test so the
/// encrypted `_system.db` stays on disk.
#[test]
fn wrong_key_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let key_file = dir.path().join("k.key");
    let snap = dir.path().join("snap");

    // key_file-source config: create-user provisions an encrypted _system.db.
    let good_cfg = dir.path().join("good.toml");
    std::fs::write(
        &good_cfg,
        format!(
            "[server]\ndata_dir=\"{}\"\nsnapshot_dir=\"{}\"\n[encryption]\nenabled=true\nkey_source=\"key_file\"\nkey_file=\"{}\"\n",
            data_dir.display(), snap.display(), key_file.display()
        ),
    )
    .unwrap();
    let out = Command::new(BIN)
        .args(["--config", good_cfg.to_str().unwrap(), "admin", "create-user", "--name", "a", "--admin"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "create-user should succeed: {}", String::from_utf8_lossy(&out.stderr));

    // env-source config with a bogus (valid-hex but wrong) key on the same DB.
    let bad_cfg = dir.path().join("bad.toml");
    std::fs::write(
        &bad_cfg,
        format!(
            "[server]\ndata_dir=\"{}\"\nsnapshot_dir=\"{}\"\n[encryption]\nenabled=true\nkey_source=\"env\"\n",
            data_dir.display(), snap.display()
        ),
    )
    .unwrap();
    let bad_key = "aa".repeat(32);
    let out = Command::new(BIN)
        .args(["--config", bad_cfg.to_str().unwrap(), "db", "list"])
        .env("TINYLORD_ENCRYPTION_KEY", &bad_key)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success(), "wrong key should fail");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("could not be opened with the configured key"),
        "expected clean error, got: {combined}"
    );
    assert!(!combined.contains(&bad_key), "key leaked into output");
}

// ---- Ephemeral pub/sub & presence ------------------------------------------

/// Read from an in-progress SSE response until `needle` appears in the
/// accumulated buffer or the timeout elapses. Returns whether it was seen.
async fn sse_wait(resp: &mut reqwest::Response, buf: &mut String, needle: &str, secs: u64) -> bool {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    buf.push_str(&String::from_utf8_lossy(&chunk));
                    if buf.contains(needle) {
                        return true;
                    }
                }
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

async fn open_channel_sub(base: &str, token: &str, channel: &str, client_id: &str) -> reqwest::Response {
    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/db/app/channels/{channel}/subscribe?client_id={client_id}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "subscribe failed: {}", resp.status());
    resp
}

#[tokio::test]
async fn pubsub_publish_excludes_sender() {
    let s = start_server(1_048_576).await;
    let token = s.provision("app", "write").await;
    let base = s.base.clone();

    // The publisher's own subscription and a second subscriber with another id.
    let mut pub_sub = open_channel_sub(&base, &token, "lobby", "pubclient").await;
    let mut other = open_channel_sub(&base, &token, "lobby", "otherclient").await;
    // Let both connections register before publishing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let published: serde_json::Value = s
        .client()
        .post(format!("{base}/v1/db/app/channels/lobby/publish"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "client_id": "pubclient", "data": { "hello": "world" } }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(published["delivered"].as_i64().unwrap() >= 1);

    // The other subscriber receives the message.
    let mut other_buf = String::new();
    assert!(
        sse_wait(&mut other, &mut other_buf, "\"hello\":\"world\"", 5).await,
        "other subscriber did not receive the message: {other_buf}"
    );
    assert!(other_buf.contains("event: message"));

    // The publisher's own subscription never receives its own message.
    let mut pub_buf = String::new();
    let saw_own = sse_wait(&mut pub_sub, &mut pub_buf, "\"hello\":\"world\"", 1).await;
    assert!(!saw_own, "publisher wrongly received its own message: {pub_buf}");
}

#[tokio::test]
async fn pubsub_presence_join_leave_and_roster() {
    let s = start_server(1_048_576).await;
    let token = s.provision("app", "write").await;
    let base = s.base.clone();
    let c = s.client();

    // A connects first and watches presence.
    let mut a = open_channel_sub(&base, &token, "room", "clientA").await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Roster shows A while connected.
    let roster: serde_json::Value = c
        .get(format!("{base}/v1/db/app/channels/room/presence"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = roster["clients"].as_array().unwrap().iter().map(|c| c["client_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"clientA"), "roster missing A: {roster}");

    // B connects; A should observe B's join.
    let b = open_channel_sub(&base, &token, "room", "clientB").await;
    let mut a_buf = String::new();
    assert!(
        sse_wait(&mut a, &mut a_buf, "\"type\":\"join\"", 5).await,
        "A did not see B join: {a_buf}"
    );
    assert!(a_buf.contains("\"client_id\":\"clientB\""));
    assert!(a_buf.contains("event: presence"));

    // Roster now shows both.
    let roster2: serde_json::Value = c
        .get(format!("{base}/v1/db/app/channels/room/presence"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids2: Vec<&str> = roster2["clients"].as_array().unwrap().iter().map(|c| c["client_id"].as_str().unwrap()).collect();
    assert!(ids2.contains(&"clientA") && ids2.contains(&"clientB"), "roster missing a client: {roster2}");

    // B disconnects; A should observe B's leave.
    drop(b);
    let mut a_buf2 = String::new();
    assert!(
        sse_wait(&mut a, &mut a_buf2, "\"type\":\"leave\"", 5).await,
        "A did not see B leave: {a_buf2}"
    );
    assert!(a_buf2.contains("\"client_id\":\"clientB\""));
}

#[tokio::test]
async fn pubsub_authz() {
    let s = start_server(1_048_576).await;
    let ro = s.provision("app", "read").await;
    let base = s.base.clone();
    let c = s.client();

    // Read-only token may open a subscription.
    let sub = c
        .get(format!("{base}/v1/db/app/channels/lobby/subscribe?client_id=reader"))
        .bearer_auth(&ro)
        .send()
        .await
        .unwrap();
    assert!(sub.status().is_success());

    // Read-only token may NOT publish.
    let forbidden = c
        .post(format!("{base}/v1/db/app/channels/lobby/publish"))
        .bearer_auth(&ro)
        .json(&serde_json::json!({ "client_id": "reader", "data": {} }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(forbidden, 403);

    // No token → 401 on both publish and subscribe.
    let no_pub = c
        .post(format!("{base}/v1/db/app/channels/lobby/publish"))
        .json(&serde_json::json!({ "client_id": "x", "data": {} }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(no_pub, 401);
    let no_sub = c
        .get(format!("{base}/v1/db/app/channels/lobby/subscribe?client_id=x"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(no_sub, 401);
}

#[tokio::test]
async fn pubsub_oversized_and_invalid_name() {
    let s = start_server(1_048_576).await;
    let token = s.provision("app", "write").await;
    let base = s.base.clone();
    let c = s.client();

    // Oversized payload (> default max_event_bytes = 65536) → 413.
    let big = "x".repeat(70_000);
    let oversize = c
        .post(format!("{base}/v1/db/app/channels/lobby/publish"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "client_id": "c", "data": { "blob": big } }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(oversize, 413);

    // Invalid channel name (leading digit) → 400 validation.
    let bad_name = c
        .post(format!("{base}/v1/db/app/channels/1bad/publish"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "client_id": "c", "data": {} }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(bad_name, 400);

    // Invalid client_id → 400 validation.
    let bad_client = c
        .post(format!("{base}/v1/db/app/channels/lobby/publish"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "client_id": "", "data": {} }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(bad_client, 400);
}

#[tokio::test]
async fn pubsub_defaults_without_config_section() {
    // `start_server` writes a config with no `[pubsub]` section; the feature must
    // still work with defaults.
    let s = start_server(1_048_576).await;
    let token = s.provision("app", "write").await;
    let base = s.base.clone();

    let mut sub = open_channel_sub(&base, &token, "lobby", "listener").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    s.client()
        .post(format!("{base}/v1/db/app/channels/lobby/publish"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "client_id": "sender", "data": { "n": 1 } }))
        .send()
        .await
        .unwrap();

    let mut buf = String::new();
    assert!(
        sse_wait(&mut sub, &mut buf, "\"n\":1", 5).await,
        "defaults round-trip failed: {buf}"
    );
}

#[tokio::test]
async fn pubsub_disabled_hides_endpoints() {
    let s = start_server_ext(1_048_576, "[pubsub]\nenabled = false\n").await;
    let token = s.provision("app", "write").await;
    let base = s.base.clone();
    let c = s.client();

    assert_eq!(
        c.post(format!("{base}/v1/db/app/channels/lobby/publish"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "client_id": "c", "data": {} }))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        c.get(format!("{base}/v1/db/app/channels/lobby/subscribe?client_id=c"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        c.get(format!("{base}/v1/db/app/channels/lobby/presence"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}
