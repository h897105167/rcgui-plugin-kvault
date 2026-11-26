//! 站点那边怎么说话：地址怎么拼、字段叫什么、回应怎么读。
//!
//! 本文件是插件里唯一知道站点形状的地方。宿主只按通用能力把请求发出去、
//! 把用户选的那个文件搬过去，它不需要认识 `folderPath`、`pagination`、
//! `bearer_token` 这些名字。改站点只要改这里，宿主不用跟着动。
//!
//! 每个函数只把「要发什么」拼出来，自己不联网——这样拼出来的东西能被打印、
//! 被断言，出问题时能一眼看出是地址错了还是令牌错了。

use serde_json::{json, Map, Value};

use crate::site;

/// 站点一页最多返回多少条。
pub const PAGE: u64 = 200;

/// 翻页上限。坏掉的 `cursor` 会让站点永远回「还有下一页」，
/// 不设上限这个动作就永远结束不了，而宿主只给 16 轮。
pub const MAX_PAGES: u32 = 5;

/// 读响应体时最多读这么多字节。列表页被站点塞进大量数据时，
/// 整包解析会把这一轮的时间和内存吃完（每轮只有 120 秒）。
pub const MAX_BODY: u64 = 262_144;

/// 单次请求最多等多久。删一条记录是纯 KV 操作，给它一分钟已经很宽。
pub const TIMEOUT_MS: u64 = 60_000;

/// 上传要搬字节，给十分钟——这是宿主允许的上限，也是 100 MiB 慢速上传的余量。
pub const UPLOAD_TIMEOUT_MS: u64 = 600_000;

/// 站点索引里的一条记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 站点的主键，形如 `tg:AgACAg…` 或 `r2:dir/a.png`。
    /// 删文件只能靠它——按名字删在重名时会删错。
    pub id: String,
    pub name: String,
    /// 去掉首尾斜杠的目录；空串表示根目录。
    pub folder: String,
    pub size: u64,
    /// 站点给的原始时间文本，插件不解析也不比较，只转给用户看。
    pub uploaded: String,
}

/// 一页列表的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub entries: Vec<Entry>,
    /// 站点说还有下一页时给的游标；没有就是最后一页。
    pub next: Option<String>,
    /// 站点明确说「这是全部」时为真。有些部署只给游标不给我这个标志，
    /// 所以两个条件都要看。
    pub complete: bool,
}

/// 上传成功后站点回的那几个值。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Uploaded {
    pub name: String,
    /// 下载地址，站点回什么就是什么——可能是相对路径，展示前用 [`absolutize`] 补齐。
    pub link: String,
    /// 站点认为这次上传和自己已有的文件是同一份，于是没有真存第二份。
    pub deduped: bool,
}

/// 列一页要的地址。
///
/// 游标原样来自上一页，可能含 `+` `/` `=`，不编码的话站点会把它拆坏。
pub fn files_url(base: &str, cursor: &str) -> String {
    let mut url = format!("{}/api/v1/files?limit={}", base.trim_end_matches('/'), PAGE);
    if !cursor.is_empty() {
        url.push_str("&cursor=");
        url.push_str(&encode_all(cursor));
    }
    url
}

/// 删一条记录要的地址。
pub fn file_url(base: &str, id: &str) -> String {
    format!(
        "{}/api/v1/file/{}",
        base.trim_end_matches('/'),
        encode_id(id)
    )
}

/// 上传要的地址。
pub fn upload_url(base: &str) -> String {
    format!("{}/api/v1/upload", base.trim_end_matches('/'))
}

/// 把站点给的下载地址拼成完整地址。
///
/// 站点可能回 `/f/1` 这种相对路径。拼地址要认识站点地址，所以这件事只能由插件做：
/// 宿主拿到的是一整条地址，它不必知道站点在哪。
pub fn absolutize(base: &str, link: &str) -> String {
    let link = link.trim();
    if link.is_empty() {
        return String::new();
    }
    if link.starts_with("http://") || link.starts_with("https://") {
        return link.to_string();
    }
    let base = base.trim_end_matches('/');
    if link.starts_with('/') {
        format!("{base}{link}")
    } else {
        format!("{base}/{link}")
    }
}

/// 把主键编码进路径。
///
/// 保留 `:` 与 `/`：站点把整段路径当作主键查表，把 `tg:…` 编码成 `tg%3A…`
/// 会让它查不到；而空格、`%`、引号这类字符又必须编码，否则请求行会被拆断。
pub fn encode_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        let keep =
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b':' | b'/');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 游标放在查询串里，只留 RFC 3986 的非保留字符。
fn encode_all(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 认证头。站点只认 Bearer；空令牌在这里是错误，往上抛给调用方报「缺令牌」，
/// 发一个空头过去只会换回 401，用户看不出是哪里没填。
pub fn auth_headers(token: &str) -> Map<String, Value> {
    let mut headers = Map::new();
    headers.insert(
        "Authorization".into(),
        json!(format!("Bearer {}", token.trim())),
    );
    headers
}

/// 列一页文件要的 `http.request` 参数。
pub fn list_request(base: &str, token: &str, cursor: &str) -> Value {
    json!({
        "method": "GET",
        "url": files_url(base, cursor),
        "headers": Value::Object(auth_headers(token)),
        "timeout_ms": TIMEOUT_MS,
        "max_bytes": MAX_BODY,
    })
}

/// 删一条记录要的 `http.request` 参数。
pub fn delete_request(base: &str, token: &str, id: &str) -> Value {
    json!({
        "method": "DELETE",
        "url": file_url(base, id),
        "headers": Value::Object(auth_headers(token)),
        "timeout_ms": TIMEOUT_MS,
        "max_bytes": 65_536,
    })
}

/// 解析一页列表。
///
/// 站点有两种外壳：老部署把字段放在顶层，新部署套一层 `data`。
/// 两种都要认，否则换个部署版本列表就空了。
pub fn parse_files(v: &Value) -> Page {
    let root = v.get("data").filter(|d| d.is_object()).unwrap_or(v);
    let list = root
        .get("files")
        .or_else(|| v.get("files"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entries: Vec<Entry> = list
        .iter()
        .filter_map(|item| {
            let id = text(item.get("id"));
            if id.is_empty() {
                return None;
            }
            Some(Entry {
                id,
                name: text(item.get("name")),
                folder: text(item.get("folderPath")).trim_matches('/').to_string(),
                size: item.get("size").and_then(Value::as_u64).unwrap_or(0),
                uploaded: text(item.get("uploadedAt")),
            })
        })
        .collect();
    let pag = root.get("pagination").or_else(|| v.get("pagination"));
    let complete = pag
        .and_then(|p| p.get("listComplete"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let next = pag
        .and_then(|p| p.get("cursor"))
        .map(|x| text(Some(x)))
        .filter(|c| !c.is_empty());
    Page {
        entries,
        next,
        complete,
    }
}

/// 解析上传成功的回应，读不出关键字段就返回 `None`（调用方据此报「回应看不懂」，
/// 不假装成功——用户会以为文件传上去了）。
pub fn parse_upload(v: &Value) -> Option<Uploaded> {
    let root = v.get("data").filter(|d| d.is_object()).unwrap_or(v);
    let name = root
        .pointer("/file/name")
        .map(|x| text(Some(x)))
        .filter(|n| !n.is_empty())?;
    let link = root
        .pointer("/links/download")
        .map(|x| text(Some(x)))
        .unwrap_or_default();
    Some(Uploaded {
        name,
        link,
        deduped: is_true(root.get("deduplicated")),
    })
}

/// 幂等键的开头。
///
/// 站点靠这个键判断「这次上传是不是刚才那次的重试」——键一样就不存第二份。
/// 键必须对内容敏感，而对重试不敏感，所以插件只给出「去处 + 文件名」这段，
/// 真正的字节长度和内容摘要由宿主补上（只有它能读到文件）。
///
/// 插件不自己算内容摘要：它读不到文件，也不知道大小，硬凑一个键会让重试
/// 变成新的一次上传，站点上就多出一份同样的东西。
pub fn idem_seed(base: &str, storage: &str, folder: &str, name: &str) -> String {
    let seed = format!("{base}|{storage}|{folder}|{name}");
    format!("kvault-{:016x}", fnv1a(&seed))
}

/// 令牌遮起来之后，把这次要发的请求整段打印出来。
///
/// 用户点「看请求」时看到的就是这个：地址、方法、头、表单字段、能力调用顺序。
/// 有了它，「插件到底想干什么」不用读代码也能核对。
pub fn plan(base: &str, token: &str, folder: &str, storage: &str, dedup: bool, id: &str) -> String {
    let token_shown = mask(token);
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("站点 {}", base.trim_end_matches('/')));
    lines.push(format!("令牌 {}", token_shown));
    lines.push(format!(
        "目录 {}",
        if folder.is_empty() { "(根)" } else { folder }
    ));

    let list = list_request(base, token, "");
    lines.push(String::new());
    lines.push("① 列文件".into());
    lines.push(format!(
        "  {} {}",
        list["method"].as_str().unwrap_or(""),
        list["url"].as_str().unwrap_or("")
    ));
    lines.push(format!("  Authorization: Bearer {token_shown}"));

    if !id.is_empty() {
        let del = delete_request(base, token, id);
        lines.push(String::new());
        lines.push("② 删文件".into());
        lines.push(format!(
            "  {} {}",
            del["method"].as_str().unwrap_or(""),
            del["url"].as_str().unwrap_or("")
        ));
    }

    let up = upload_request(base, token, "文件名", folder, storage, dedup);
    lines.push(String::new());
    lines.push("③ 上传".into());
    lines.push(format!(
        "  {} {}",
        up["method"].as_str().unwrap_or(""),
        up["url"].as_str().unwrap_or("")
    ));
    lines.push(format!(
        "  文件部分 file，类型 {}",
        up["file"]["mime"].as_str().unwrap_or("")
    ));
    for field in up["fields"].as_array().cloned().unwrap_or_default() {
        lines.push(format!(
            "  字段 {}={}",
            field["name"].as_str().unwrap_or(""),
            field["value"].as_str().unwrap_or("")
        ));
    }
    lines.push(format!(
        "  幂等键 {} + 宿主补的字节摘要",
        up["idem_seed"].as_str().unwrap_or("")
    ));
    lines.push(String::new());
    lines.push(
        "调用顺序：kv.get_all → http.request（每页一次）→ ui.confirm → http.request → http.upload"
            .into(),
    );
    lines.push("字节只由宿主从用户这次挑的那一个文件流出去；插件拿不到任何本地路径。".into());
    lines.join("\n")
}

fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 按扩展名给个 Content-Type。给不准时退回 `application/octet-stream`：
/// 站点按它决定预览方式，给错比给空好，但给空也比给错 HTML 好。
pub fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

/// 一次上传要交给宿主的完整说明。
///
/// `source.form_key` 是关键：插件只说自己要的是「面板里 `file` 那一栏用户挑的文件」，
/// 不说本地路径。宿主自己记录那次挑选，据此把字节流出去；插件即便想读
/// `C:\Windows\…` 也没有地方可写，因为它手里从来没有路径这个概念。
pub fn upload_request(
    base: &str,
    token: &str,
    file_name: &str,
    folder: &str,
    storage: &str,
    dedup: bool,
) -> Value {
    let headers = auth_headers(token);
    let mut fields = vec![json!({"name": "storage", "value": storage})];
    if !folder.is_empty() {
        fields.push(json!({"name": "folderPath", "value": folder}));
    }
    if dedup {
        fields.push(json!({"name": "deduplicate", "value": "true"}));
    }
    json!({
        "method": "POST",
        "url": upload_url(base),
        "headers": Value::Object(headers),
        "source": { "form_key": "file" },
        "file": { "field": "file", "name": file_name, "mime": mime_for(file_name) },
        "fields": fields,
        // 宿主把这段和它算出的字节摘要接起来，作为 Idempotency-Key 发出去。
        // 插件不自己发这个头：它读不到文件，凑出来的键每次重试都不一样。
        "idem_seed": idem_seed(base, storage, folder, file_name),
        // 两个上限分开放：`max_file_bytes` 是这个文件最多多大，`max_bytes`
        // 与 `http.request` 同名同义，管回包收多少就截断。
        "max_file_bytes": site::MAX_UPLOAD,
        "timeout_ms": UPLOAD_TIMEOUT_MS,
        "max_bytes": 65_536,
    })
}

/// 把一条记录拆成可以塞进 `state` 的值。
pub fn entry_to_value(entry: &Entry) -> Value {
    json!({
        "id": entry.id,
        "name": entry.name,
        "folder": entry.folder,
        "size": entry.size,
        "uploaded": entry.uploaded,
    })
}

/// 从 `state` 里读回记录；形状不对的丢掉，不让一个坏条目把整页结果毁掉。
pub fn entry_of(v: &Value) -> Option<Entry> {
    let id = text(v.get("id"));
    if id.is_empty() {
        return None;
    }
    Some(Entry {
        id,
        name: text(v.get("name")),
        folder: text(v.get("folder")),
        size: v.get("size").and_then(Value::as_u64).unwrap_or(0),
        uploaded: text(v.get("uploaded")),
    })
}

/// 给用户看的清单，一行一条：先目录后名字，再大小，最后主键。
pub fn listing(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut lines = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = if entry.folder.is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", entry.folder, entry.name)
        };
        lines.push(format!("{path}\t{}\t{}", human_size(entry.size), entry.id));
    }
    lines.join("\n")
}

/// 字节数换成人能读的写法。列表里全是 `10485760` 这种数字没人看得懂。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 令牌只露出前 4 位，用来核对「用的是哪一个令牌」而不泄露它。
pub fn mask(token: &str) -> String {
    let token = token.trim();
    if token.is_empty() {
        return "(空)".into();
    }
    let head: String = token.chars().take(4).collect();
    format!("{head}…({} 字符)", token.chars().count())
}

fn text(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn is_true(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim(), "1" | "true" | "yes" | "on"),
        Some(Value::Number(n)) => n.as_u64().map(|n| n > 0).unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://vault.example.com";

    #[test]
    fn a_page_url_carries_the_limit_and_the_cursor() {
        assert_eq!(
            files_url(BASE, ""),
            "https://vault.example.com/api/v1/files?limit=200"
        );
        // 站点给的游标常带 `=` 与 `/`，不编码会被当成查询串边界。
        assert_eq!(
            files_url(BASE, "a+b/c="),
            "https://vault.example.com/api/v1/files?limit=200&cursor=a%2Bb%2Fc%3D"
        );
        // 用户把地址写成带斜杠的样子也要能用。
        assert_eq!(
            files_url("https://x.test/", ""),
            "https://x.test/api/v1/files?limit=200"
        );
    }

    #[test]
    fn a_primary_key_keeps_its_colon_but_encodes_the_rest() {
        assert_eq!(encode_id("tg:AgACAgQAAxkB"), "tg:AgACAgQAAxkB");
        assert_eq!(encode_id("r2:dir/a.png"), "r2:dir/a.png");
        assert_eq!(
            encode_id("webdav:my file#1.png"),
            "webdav:my%20file%231.png"
        );
    }

    #[test]
    fn a_relative_download_path_is_completed_against_the_site() {
        assert_eq!(absolutize(BASE, "/f/1"), "https://vault.example.com/f/1");
        assert_eq!(absolutize("https://x.test/", "f/1"), "https://x.test/f/1");
        // 站点已经给了整条地址就别动它——拼一次会把路径拼坏。
        assert_eq!(
            absolutize(BASE, "https://cdn.test/f/1"),
            "https://cdn.test/f/1"
        );
        assert_eq!(absolutize(BASE, "  "), "");
    }

    #[test]
    fn a_page_without_a_cursor_is_the_last_page() {
        let page = parse_files(&json!({
            "files": [{ "id": "tg:1", "name": "a.png", "folderPath": "/album/", "size": 2048 }],
            "pagination": { "listComplete": true },
        }));
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].id, "tg:1");
        assert_eq!(page.entries[0].folder, "album", "目录两头的斜杠要去掉");
        assert_eq!(page.entries[0].size, 2048);
        assert_eq!(page.next, None);
        assert!(page.complete);
    }

    #[test]
    fn a_page_wrapped_in_data_still_reads() {
        let page = parse_files(&json!({
            "success": true,
            "data": {
                "files": [{ "id": "r2:x.bin", "name": "x.bin", "size": 1 }],
                "pagination": { "cursor": "next-1", "listComplete": false },
            },
        }));
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.next.as_deref(), Some("next-1"));
        assert!(!page.complete);
    }

    #[test]
    fn a_record_without_an_id_is_dropped() {
        // 没有主键就没法删也没法定位，留着只会让用户点了没反应。
        let page = parse_files(&json!({ "files": [{ "name": "no-id.png" }] }));
        assert!(page.entries.is_empty());
    }

    #[test]
    fn the_idem_seed_is_stable_per_destination_and_name() {
        let a = idem_seed(BASE, "telegram", "album", "a.png");
        let b = idem_seed(BASE, "telegram", "album", "a.png");
        assert_eq!(a, b, "重试必须拿到同一个开头，否则站点会存两份");
        assert_ne!(
            a,
            idem_seed(BASE, "r2", "album", "a.png"),
            "换个存储就不是同一份"
        );
        assert_ne!(
            a,
            idem_seed(BASE, "telegram", "other", "a.png"),
            "换个目录也不是"
        );
        assert_ne!(
            a,
            idem_seed(BASE, "telegram", "album", "b.png"),
            "换个文件名不是同一份"
        );
        assert!(
            a.starts_with("kvault-") && a.len() == 23,
            "键长固定：前缀 7 + 16 位十六进制"
        );
    }

    #[test]
    fn an_upload_always_names_a_storage_and_only_names_a_folder_when_there_is_one() {
        let with = upload_request(BASE, "tok-123", "a.png", "album", "r2", true);
        assert_eq!(
            with["url"],
            json!("https://vault.example.com/api/v1/upload")
        );
        assert_eq!(with["file"]["field"], json!("file"));
        assert_eq!(with["file"]["mime"], json!("image/png"));
        assert_eq!(
            with["source"]["form_key"],
            json!("file"),
            "插件只说栏位，不给路径"
        );
        assert_eq!(with["headers"]["Authorization"], json!("Bearer tok-123"));
        let fields = with["fields"].as_array().unwrap();
        let names: Vec<&str> = fields.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["storage", "folderPath", "deduplicate"]);
        assert!(
            with["idem_seed"].as_str().unwrap().starts_with("kvault-"),
            "幂等键的开头由插件给，摘要由宿主补"
        );
        // 插件不发这个头：它读不到文件，自己算出来的键每次重试都不一样。
        assert!(with["headers"].get("Idempotency-Key").is_none());
        // 两个上限不能混：一个是文件大小，一个是回包截断，宿主实现时各走各的路。
        assert_eq!(with["max_file_bytes"], json!(site::MAX_UPLOAD));
        assert_eq!(with["max_bytes"], json!(65_536));
        assert!(with.get("max_body").is_none(), "回包上限叫 max_bytes");

        // 根目录上传不能带空的 folderPath：站点会把它当成一个名叫空的目录。
        let root = upload_request(BASE, "tok-123", "b.bin", "", "telegram", false);
        let fields = root["fields"].as_array().unwrap();
        let names: Vec<&str> = fields.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["storage"]);
        assert!(root["headers"].get("Idempotency-Key").is_none());
    }

    #[test]
    fn the_plan_shows_the_requests_without_the_token() {
        let text = plan(BASE, "kvault_secret_value", "album", "r2", true, "tg:1");
        assert!(
            !text.contains("kvault_secret_value"),
            "计划是给人看的，不该带着令牌"
        );
        assert!(text.contains("kvau…"), "遮起来也要看得出这是哪一份");
        assert!(text.contains("https://vault.example.com/api/v1/upload"));
        assert!(text.contains("DELETE"), "给了 id 就要把删除请求也列出来");
        assert!(text.contains("kvault-"));

        let no_id = plan(BASE, "t", "", "telegram", false, "");
        assert!(!no_id.contains("DELETE"), "没给 id 就不列删除");
    }

    #[test]
    fn an_upload_reply_is_read_through_its_data_layer() {
        let out = parse_upload(&json!({
            "success": true,
            "data": {
                "file": { "name": "a.png" },
                "links": { "download": "/api/v1/file/tg:1" },
                "deduplicated": "true",
            },
        }))
        .unwrap();
        assert_eq!(out.name, "a.png");
        assert_eq!(out.link, "/api/v1/file/tg:1");
        assert!(out.deduped);
    }

    #[test]
    fn an_upload_reply_without_a_name_is_not_a_success() {
        // 只看 HTTP 200 会漏掉站点「存了但没说是哪一份」的情况。
        assert!(parse_upload(&json!({ "success": true })).is_none());
    }

    #[test]
    fn a_listing_puts_the_path_first_and_the_key_last() {
        let entries = vec![
            Entry {
                id: "tg:1".into(),
                name: "a.png".into(),
                folder: "album".into(),
                size: 2048,
                uploaded: String::new(),
            },
            Entry {
                id: "r2:2".into(),
                name: "root.txt".into(),
                folder: String::new(),
                size: 0,
                uploaded: String::new(),
            },
        ];
        let text = listing(&entries);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "album/a.png\t2.0 KiB\ttg:1");
        assert_eq!(lines[1], "root.txt\t0 B\tr2:2");
        assert_eq!(
            listing(&[]),
            "",
            "没有文件就不给内容，面板会显示空白而不是假的一行"
        );
    }

    #[test]
    fn a_record_survives_a_trip_through_state() {
        let entry = Entry {
            id: "tg:1".into(),
            name: "a.png".into(),
            folder: "album".into(),
            size: 7,
            uploaded: "2026-01-01".into(),
        };
        assert_eq!(entry_of(&entry_to_value(&entry)), Some(entry));
        assert_eq!(entry_of(&json!({ "name": "no-id" })), None);
    }

    #[test]
    fn the_token_is_only_shown_by_its_head() {
        assert_eq!(mask("kvault_abcdefgh"), "kvau…(15 字符)");
        assert_eq!(mask("   "), "(空)");
    }

    #[test]
    fn a_mutation_and_a_list_ask_for_their_own_methods() {
        let list = list_request(BASE, "t", "");
        assert_eq!(list["method"], json!("GET"));
        let del = delete_request(BASE, "t", "tg:1");
        assert_eq!(del["method"], json!("DELETE"));
        assert_eq!(
            del["url"],
            json!("https://vault.example.com/api/v1/file/tg:1")
        );
        assert_eq!(del["max_bytes"], json!(65_536));
    }
}
