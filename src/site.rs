//! 站点（K-Vault 服务）那一侧的接口形状。
//!
//! 插件自己不联网：WASI 里没有 socket，请求都由宿主的 `http.request` 发出。
//! 这里只做三件事——拼地址、解析回来的 JSON、算一个 rclone 远程该带哪些参数。

use serde_json::{Map, Value};

/// 站点对单文件的上限（100 MB）。令牌策略可能更小，那种情况服务端回 413
/// 并带上真实上限，所以这个值只用于提前提示，不作为拒绝依据。
pub const MAX_UPLOAD: u64 = 100 * 1024 * 1024;

/// 新建远程时默认选哪个存储。
pub const DEFAULT_STORAGE: &str = "telegram";

/// `/api/v1/me` 里跟插件有关的部分。读不到的字段留空，不抛错——
/// 站点版本比插件新时，少一个字段不该让整个面板变红。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Probe {
    pub id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub storages: Vec<String>,
    /// 0 表示站点没说明上限。
    pub max_file: u64,
    pub folder_prefix: String,
}

/// 站点根地址：去掉首尾空白、末尾的 `/`，以及粘地址时常带上的 `/dav`。
///
/// 用户从站点复制的往往就是 WebDAV 地址（带 `/dav`），而 API 在站点根上，
/// 两种写法都得能用，所以入口统一成根地址。
pub fn base_of(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    let without_dav = match trimmed.strip_suffix("/dav") {
        Some(rest) => rest.trim_end_matches('/'),
        None => trimmed,
    };
    without_dav.to_string()
}

/// 只接受 http/https：rclone 的 webdav 后端也只认这两种。
pub fn is_http(base: &str) -> bool {
    let lower = base.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// 给 rclone 的 `url`。rclone 拿到的是 `…/dav`，不要传站点根。
pub fn dav_url(base: &str) -> String {
    format!("{}/dav", base_of(base))
}

pub fn health_url(base: &str) -> String {
    format!("{}/api/health", base_of(base))
}

pub fn me_url(base: &str) -> String {
    format!("{}/api/v1/me", base_of(base))
}

/// 把站点的失败响应变成一句人话。
///
/// 站点在 `error.code` / `error.message` 里放原因（例如
/// `POLICY_SIZE_EXCEEDED`），拿不到就只报状态码，绝不编一个原因。
pub fn api_error(status: u16, body: &str) -> String {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = value
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or("");
    match (code.is_empty(), message.is_empty()) {
        (false, false) => format!("HTTP {status} · {code}: {message}"),
        (false, true) => format!("HTTP {status} · {code}"),
        (true, false) => format!("HTTP {status} · {message}"),
        (true, true) => format!("HTTP {status}"),
    }
}

fn text_of(map: &Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn list_of(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `/api/v1/me` 的响应长这样：`{"success":true,"data":{"token":{…}}}`。
///
/// 老版本少一层 `data`，所以每一层都试一次。`policies` 可能在 token 里，
/// 也可能在 `data` 或根上，同样逐层找。
pub fn parse_me(body: &Value) -> Probe {
    let empty = Map::new();
    let node = body
        .pointer("/data/token")
        .and_then(Value::as_object)
        .or_else(|| body.get("token").and_then(Value::as_object))
        .or_else(|| body.get("data").and_then(Value::as_object))
        .or_else(|| body.as_object())
        .unwrap_or(&empty);

    let policies = node
        .get("policies")
        .and_then(Value::as_object)
        .or_else(|| body.pointer("/data/policies").and_then(Value::as_object))
        .or_else(|| body.get("policies").and_then(Value::as_object))
        .unwrap_or(&empty);

    Probe {
        id: text_of(node, "id"),
        name: text_of(node, "name"),
        scopes: list_of(node.get("scopes")),
        storages: list_of(policies.get("allowedStorages")),
        max_file: policies
            .get("maxFileSize")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        folder_prefix: text_of(policies, "folderPrefix"),
    }
}

/// 一个 rclone webdav 远程需要的参数。
///
/// `bearer_token` 只在有令牌时写入：写一个空字符串会把 rclone 里已经存着的
/// 令牌抹掉，而 rclone 读回来的又只是 Obscured 串，用户没法自己补救。
///
/// `vendor=other` 关掉 owncloud/nextcloud 探测——K-Vault 是一棵普通 WebDAV 树。
/// `kvault=1` 是标记位，主程序和插件都靠它认出这类远程。
pub fn remote_params(
    base: &str,
    token: &str,
    folder: &str,
    storage: &str,
    dedup: bool,
) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("url".into(), Value::String(dav_url(base)));
    params.insert("vendor".into(), Value::String("other".into()));
    let token = token.trim();
    if !token.is_empty() {
        params.insert("bearer_token".into(), Value::String(token.into()));
    }
    params.insert("kvault".into(), Value::String("1".into()));
    params.insert(
        "kvault_folder".into(),
        Value::String(folder.trim().trim_matches('/').to_string()),
    );
    let storage = storage.trim();
    params.insert(
        "kvault_storage".into(),
        Value::String(if storage.is_empty() {
            DEFAULT_STORAGE.to_string()
        } else {
            storage.to_string()
        }),
    );
    params.insert(
        "kvault_dedup".into(),
        Value::String(if dedup { "1" } else { "0" }.into()),
    );
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base_accepts_the_url_users_actually_paste() {
        assert_eq!(base_of("https://kv.example.com"), "https://kv.example.com");
        assert_eq!(
            base_of("  https://kv.example.com/  "),
            "https://kv.example.com"
        );
        assert_eq!(
            base_of("https://kv.example.com/dav"),
            "https://kv.example.com"
        );
        assert_eq!(
            base_of("https://kv.example.com/dav/"),
            "https://kv.example.com"
        );
        assert_eq!(base_of(""), "");
    }

    #[test]
    fn dav_url_is_the_only_place_dav_is_appended() {
        assert_eq!(
            dav_url("https://kv.example.com"),
            "https://kv.example.com/dav"
        );
        assert_eq!(
            dav_url("https://kv.example.com/dav"),
            "https://kv.example.com/dav"
        );
    }

    #[test]
    fn api_urls_hang_off_the_site_root() {
        assert_eq!(
            health_url("https://kv.example.com/dav"),
            "https://kv.example.com/api/health"
        );
        assert_eq!(
            me_url("https://kv.example.com/"),
            "https://kv.example.com/api/v1/me"
        );
    }

    #[test]
    fn only_http_and_https_are_accepted() {
        assert!(is_http("https://kv.example.com"));
        assert!(is_http("HTTP://kv.example.com"));
        assert!(!is_http("kv.example.com"));
        assert!(!is_http("ftp://kv.example.com"));
        assert!(!is_http(""));
    }

    #[test]
    fn probe_reads_the_nested_token_object() {
        let body = json!({
            "success": true,
            "data": {"token": {
                "id": "tok_1",
                "name": "rcgui",
                "scopes": ["upload", "read"],
                "policies": {
                    "allowedStorages": ["telegram", "r2"],
                    "maxFileSize": 15728640,
                    "folderPrefix": "rcgui/"
                }
            }}
        });
        let probe = parse_me(&body);
        assert_eq!(probe.id, "tok_1");
        assert_eq!(probe.name, "rcgui");
        assert_eq!(probe.scopes, vec!["upload", "read"]);
        assert_eq!(probe.storages, vec!["telegram", "r2"]);
        assert_eq!(probe.max_file, 15728640);
        assert_eq!(probe.folder_prefix, "rcgui/");
    }

    #[test]
    fn probe_tolerates_an_older_shape_without_policies() {
        let probe = parse_me(&json!({"token": {"id": "tok_2"}}));
        assert_eq!(probe.id, "tok_2");
        assert!(probe.storages.is_empty());
        assert_eq!(probe.max_file, 0);
    }

    #[test]
    fn probe_of_an_unexpected_body_is_empty_not_a_panic() {
        let probe = parse_me(&json!("nonsense"));
        assert_eq!(probe, Probe::default());
    }

    #[test]
    fn remote_params_never_write_an_empty_token() {
        let without = remote_params("https://kv.example.com/dav", "", "", "", true);
        assert!(!without.contains_key("bearer_token"));
        assert_eq!(without["url"], json!("https://kv.example.com/dav"));
        assert_eq!(without["vendor"], json!("other"));
        assert_eq!(without["kvault"], json!("1"));
        assert_eq!(without["kvault_storage"], json!("telegram"));
        assert_eq!(without["kvault_dedup"], json!("1"));

        let with = remote_params(
            "https://kv.example.com",
            " kvault_a_b ",
            "pics/",
            "r2",
            false,
        );
        assert_eq!(with["bearer_token"], json!("kvault_a_b"));
        assert_eq!(with["kvault_folder"], json!("pics"));
        assert_eq!(with["kvault_storage"], json!("r2"));
        assert_eq!(with["kvault_dedup"], json!("0"));
    }

    #[test]
    fn api_error_prefers_the_site_code_then_falls_back_to_the_status() {
        let body = json!({"error": {"code": "POLICY_SIZE_EXCEEDED", "message": "too big"}});
        assert_eq!(
            api_error(413, &body.to_string()),
            "HTTP 413 · POLICY_SIZE_EXCEEDED: too big"
        );
        assert_eq!(api_error(500, "not json"), "HTTP 500");
        assert_eq!(
            api_error(404, &json!({"message": "gone"}).to_string()),
            "HTTP 404 · gone"
        );
    }

    /// 面板上的存储下拉就是这一份清单（config.json 里写死的选项）。
    /// 站点以后多一个存储时，改 config.json，再把这里跟着改——测试会先失败。
    const STORAGES: [&str; 7] = [
        "telegram",
        "r2",
        "s3",
        "discord",
        "huggingface",
        "webdav",
        "github",
    ];

    /// 面板上的存储下拉写着 config.json 里的选项，代码里的 STORAGES 是同一份清单。
    /// 两边改了一处忘了另一处，用户就会选到站点不认的存储名。
    #[test]
    fn the_storage_dropdown_matches_the_list_in_code() {
        let raw = include_str!("../config.json");
        let cfg: Value = serde_json::from_str(raw).expect("config.json 不是合法 JSON");
        let controls = cfg["plugin_setting"]["controls"]
            .as_array()
            .expect("config.json 缺少 controls");
        let storage = controls
            .iter()
            .find(|c| c["key"] == json!("storage"))
            .expect("config.json 里没有 storage 控件");
        let values: Vec<&str> = storage["options"]
            .as_array()
            .expect("storage 控件不是 select（缺少 options）")
            .iter()
            .filter_map(|o| o["value"].as_str())
            .collect();
        assert_eq!(values, STORAGES.to_vec());
        assert!(STORAGES.contains(&DEFAULT_STORAGE));
    }
}
