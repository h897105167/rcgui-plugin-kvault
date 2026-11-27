//! 接在真 socket 上跑的测试：插件的每个请求都真的发出去、真的收回来。
//!
//! 假宿主只能证明插件「说要发什么」，证明不了那条请求本身能不能跑通：
//! 请求行拼错、头写错、body 拼错，假宿主一律看不出来。所以这里起一个
//! 本机 HTTP 服务，把插件发出的字节原样收下，再按站点的样子回话。

use super::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// 服务端收到的一条请求，原样拆开存着，供测试核对。
#[derive(Clone, Debug)]
struct Seen {
    line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> String {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

/// 本机服务。`base` 是插件要填进表单的站点地址。
struct Srv {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Srv {
    fn start() -> Srv {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑不上本机端口");
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, flag) = (seen.clone(), stop.clone());
        thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((sock, _)) => {
                        let _ = serve_one(sock, &log);
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Srv {
            base: format!("http://127.0.0.1:{port}"),
            seen,
            stop,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// 收一条请求，按站点的样子回话。
fn serve_one(mut sock: TcpStream, log: &Arc<Mutex<Vec<Seen>>>) -> std::io::Result<()> {
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    // 先读到头部结束，再按 Content-Length 收满 body。
    loop {
        let n = sock.read(&mut buf)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(end) = find(&raw, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..end]).to_string();
            let want: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.eq_ignore_ascii_case("content-length") {
                        v.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if raw.len() >= end + 4 + want {
                break;
            }
        }
    }
    let end = find(&raw, b"\r\n\r\n").unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..end]).to_string();
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let body = if raw.len() > end + 4 {
        raw[end + 4..].to_vec()
    } else {
        Vec::new()
    };
    let seen = Seen {
        line: line.clone(),
        headers,
        body,
    };
    let path = line.split(' ').nth(1).unwrap_or("").to_string();
    log.lock().unwrap().push(seen);

    let payload = if path.starts_with("/api/v1/files") {
        if path.contains("cursor=p2") {
            r#"{"success":true,"data":{"files":[{"id":"r2:b.txt","name":"b.txt","folderPath":"/album","size":12,"uploadedAt":"2026-01-02"}],"pagination":{"listComplete":true}}}"#
        } else {
            r#"{"success":true,"data":{"files":[{"id":"tg:1","name":"a.png","folderPath":"/album","size":2048,"uploadedAt":"2026-01-01"}],"pagination":{"listComplete":false,"cursor":"p2"}}}"#
        }
    } else if path.starts_with("/api/v1/file/") {
        r#"{"success":true}"#
    } else if path.starts_with("/api/v1/upload") {
        // 只回相对路径：拼成完整地址是插件的事。
        r#"{"success":true,"file":{"name":"picked.txt"},"links":{"download":"/f/9"},"deduplicated":false}"#
    } else {
        r#"{"error":{"code":"NOT_FOUND","message":"没有这个接口"}}"#
    };
    let status = if path.starts_with("/api/") {
        "200 OK"
    } else {
        "404 Not Found"
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    sock.write_all(reply.as_bytes())?;
    sock.flush()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 把插件指定的请求真的发出去，收回状态码和 body。
fn send(srv: &Srv, method: &str, url: &str, headers: &Map<String, Value>, body: &[u8]) -> Value {
    if !url.starts_with(&srv.base) {
        // 插件拼错地址时不要静默当成网络错误，直接让测试看到差在哪。
        panic!("插件把请求发到了别的地方：{url}");
    }
    let path = &url[srv.base.len()..];
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {}\r\n", v.as_str().unwrap_or("")));
    }
    if !body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut sock = TcpStream::connect(&srv.base["http://".len()..]).expect("连不上本机服务");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    sock.write_all(head.as_bytes()).unwrap();
    sock.write_all(body).unwrap();
    sock.flush().unwrap();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, payload) = match text.split_once("\r\n\r\n") {
        Some(pair) => pair,
        None => return json!({ "status": 0, "body": text }),
    };
    let status: u64 = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    json!({ "status": status, "body": payload })
}

/// 按 host 的做法把一次上传拼成 multipart。
///
/// 关键点：分块里的字节只来自 `grant`（用户这次选中的那个文件），
/// 插件给的 `source.form_key` 只是拿来查这张授权表的。
fn multipart(upload: &Value, grant: &HashMap<String, PathBuf>) -> (Vec<u8>, String) {
    let boundary = "kvault-wire-boundary";
    let mut body = Vec::new();
    for field in upload["fields"].as_array().cloned().unwrap_or_default() {
        let name = field["name"].as_str().unwrap_or("");
        let value = field["value"].as_str().unwrap_or("");
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    let form_key = upload["source"]["form_key"].as_str().unwrap_or("");
    let path = grant
        .get(form_key)
        .unwrap_or_else(|| panic!("宿主没有给 {form_key} 这个字段授权"));
    let bytes = std::fs::read(path).expect("读不到测试文件");
    let file_name = upload["file"]["name"].as_str().unwrap_or("x");
    let mime = upload["file"]["mime"].as_str().unwrap_or("");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: {mime}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (body, boundary.to_string())
}

/// 跑一个动作：每一轮都把 need 换成真的动作，直到插件说完了。
fn run(
    srv: &Srv,
    kv: Value,
    action: &str,
    form: Value,
    yes: bool,
    grant: &HashMap<String, PathBuf>,
) -> Value {
    let mut state = Value::Null;
    let mut provided = Value::Null;
    for _ in 0..16 {
        let out = dispatch(
            1,
            &json!({
                "action": action,
                "form": form,
                "state": state,
                "provided": provided,
                "lang": "zh",
            }),
        );
        let needs = out["need"].as_array().cloned().unwrap_or_default();
        if needs.is_empty() {
            return out["result"]["body"].clone();
        }
        let mut answers = Map::new();
        for item in needs {
            let cap = item["cap"].as_str().unwrap_or("");
            let key = item["key"].as_str().unwrap_or("");
            let params = &item["params"];
            let reply = match cap {
                "kv.get_all" => kv.clone(),
                "ui.confirm" => json!({ "ok": yes }),
                "http.request" => {
                    let method = params["method"].as_str().unwrap_or("GET");
                    let url = params["url"].as_str().unwrap_or("");
                    let headers = params["headers"].as_object().cloned().unwrap_or_default();
                    let body = params["body"].as_str().unwrap_or("").as_bytes().to_vec();
                    send(srv, method, url, &headers, &body)
                }
                "http.upload" => {
                    let (body, boundary) = multipart(params, grant);
                    let url = params["url"].as_str().unwrap_or("");
                    let mut headers = params["headers"].as_object().cloned().unwrap_or_default();
                    headers.insert(
                        "Content-Type".into(),
                        json!(format!("multipart/form-data; boundary={boundary}")),
                    );
                    let mut reply = send(srv, "POST", url, &headers, &body);
                    reply["size"] = json!(body.len() as u64);
                    reply
                }
                other => json!({ "_error": format!("测试里没有这个能力: {other}") }),
            };
            answers.insert(key.to_string(), reply);
        }
        state = out["state"].clone();
        provided = Value::Object(answers);
    }
    panic!("动作在 16 轮内没有结束");
}

/// 测试要读的「用户选中的文件」放在构建目录里：系统临时目录在沙箱下可能不让写。
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("wire");
    std::fs::create_dir_all(&dir).expect("建不出临时目录");
    dir.join(name)
}

fn form_for(srv: &Srv) -> Value {
    json!({
        "base": format!("{}/", srv.base),
        "token": "tok-wire",
        "folder": "album",
        "name": "my-vault",
        "storage": "r2",
        "dedup": true,
    })
}

#[test]
fn a_real_listing_walks_two_pages_over_a_socket() {
    let srv = Srv::start();
    let grant = HashMap::new();
    let body = run(&srv, json!({}), "list", form_for(&srv), true, &grant);
    let listing = body["values"]["listing"].as_str().unwrap_or("");
    assert!(listing.contains("a.png"), "第一页没被列出来：{listing}");
    assert!(listing.contains("b.txt"), "第二页没被列出来：{listing}");
    // 两页就是两条请求，而且都带着插件给的那条认证头。
    let seen = srv.seen();
    assert_eq!(seen.len(), 2, "页数不对：{seen:?}");
    assert_eq!(seen[0].header("authorization"), "Bearer tok-wire");
    assert!(
        seen[1].line.contains("cursor=p2"),
        "没把游标带给站点：{}",
        seen[1].line
    );
}

#[test]
fn a_real_delete_asks_first_and_sends_the_encoded_key() {
    let srv = Srv::start();
    let grant = HashMap::new();
    let mut form = form_for(&srv);
    form["id"] = json!("webdav:my file#1.png");
    let body = run(&srv, json!({}), "remove", form, true, &grant);
    assert_eq!(body["lamp"], "ok");
    let seen = srv.seen();
    assert_eq!(seen.len(), 1, "只该有一条删除请求：{seen:?}");
    assert!(
        seen[0]
            .line
            .starts_with("DELETE /api/v1/file/webdav:my%20file%231.png "),
        "主键没编码对：{}",
        seen[0].line
    );
    assert_eq!(seen[0].header("authorization"), "Bearer tok-wire");
}

#[test]
fn a_real_upload_streams_only_the_file_the_user_picked() {
    let srv = Srv::start();
    // 宿主这一侧的用户授权：只有面板里这个字段对应的文件能出去。
    let picked = scratch("picked.png");
    std::fs::write(&picked, b"\x89PNG\r\n\x1a\nbytes-of-a-picture").unwrap();
    let mut grant = HashMap::new();
    grant.insert("file".to_string(), picked.clone());

    let mut form = form_for(&srv);
    form["file"] = json!("C:/Users/someone/Pictures/picked.png");
    let body = run(&srv, json!({}), "upload", form, true, &grant);
    assert_eq!(body["lamp"], "ok", "{body}");
    // 站点只回了 `/f/9`，拼成完整地址是插件做的。
    assert_eq!(
        body["values"]["link"].as_str().unwrap_or(""),
        format!("{}/f/9", srv.base)
    );

    let seen = srv.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].line, "POST /api/v1/upload HTTP/1.1");
    let raw = String::from_utf8_lossy(&seen[0].body).to_string();
    assert!(raw.contains("filename=\"picked.png\""), "文件名没带上");
    assert!(raw.contains("bytes-of-a-picture"), "文件字节没带上");
    assert!(
        raw.contains("name=\"storage\""),
        "站点策略要的 storage 字段没带"
    );
    assert!(raw.contains("name=\"folderPath\""), "目标目录没带");
    assert!(raw.contains("name=\"deduplicate\""), "去重开关没带");
    // 插件给的是授权表里的字段名，不是它自己编的路径。
    assert!(raw.contains("name=\"file\""), "文件字段名没带");
}

#[test]
fn a_real_upload_is_not_sent_when_the_user_says_no() {
    let srv = Srv::start();
    let picked = scratch("never.txt");
    std::fs::write(&picked, b"never").unwrap();
    let mut grant = HashMap::new();
    grant.insert("file".to_string(), picked);
    let mut form = form_for(&srv);
    form["file"] = json!("never.txt");
    // 上传不做二次确认，所以 yes 在这里无关；真正要保证的是「用户没点上传就一条字节都不发」。
    let body = run(&srv, json!({}), "plan", form, false, &grant);
    assert_eq!(body["lamp"], "ok");
    assert_eq!(srv.requests(), 0, "看请求不该碰网络");
}

#[test]
fn nothing_is_sent_when_the_plugin_has_no_site_configured() {
    let srv = Srv::start();
    let grant = HashMap::new();
    // 没配站点、也没有旧设置里的记录：插件应当当场停下，一个字节都不发。
    let body = run(&srv, json!({}), "list", json!({}), true, &grant);
    assert_eq!(body["lamp"], "err");
    assert_eq!(srv.requests(), 0, "没配站点还发了请求");
    let status = body["status"].as_str().unwrap_or("");
    assert!(
        status.contains("站点") || status.contains("地址"),
        "提示没说清缺什么：{status}"
    );
}

#[test]
fn an_old_record_migrated_into_the_inventory_can_be_listed() {
    let srv = Srv::start();
    let grant = HashMap::new();
    // 旧 K-Vault 远程被迁移后的样子：地址在 url 里，令牌单独存一份。
    let kv = json!({
        "imported": {
            "old-vault": {
                "name": "old-vault",
                "base": srv.base.clone(),
                "folder": "album",
                "storage": "r2",
                "dedup": true,
                "dav": format!("{}/dav", srv.base),
                "type": "webdav",
                "marked": true,
            }
        },
        "site": { "base": "", "token": "", "folder": "", "name": "", "storage": "", "dedup": true },
        "old-vault": { "token": "tok-old" },
    });
    let body = run(
        &srv,
        kv,
        "list",
        json!({ "remote": "old-vault" }),
        true,
        &grant,
    );
    assert_eq!(body["lamp"], "ok", "{body}");
    let listing = body["values"]["listing"].as_str().unwrap_or("");
    assert!(
        listing.contains("a.png"),
        "迁移来的记录没列出文件：{listing}"
    );
    assert_eq!(srv.seen()[0].header("authorization"), "Bearer tok-old");
}
