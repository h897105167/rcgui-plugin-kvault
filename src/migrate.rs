//! 旧版主程序的 K-Vault 设置怎么搬进插件，以及搬出来的东西怎么导出、怎么恢复。
//!
//! 旧版把设置分成两处：
//!   1. rclone.conf 里那个远程的参数：`url`、`bearer_token`、`kvault*` 几个；
//!   2. 主程序按「键 = 远程名、值 `{"token": …}`」存的明文令牌副本。
//! 第 2 处与插件自己的 kv 是同一个文件——主程序拿插件 id 当命名空间，旧版恰好也叫
//! `kvault`——所以插件读一次 `kv.get_all` 就看见了它；第 1 处得经
//! `rclone.call config/get` 一个个读。
//!
//! 两条规矩贯穿整个模块：
//!   * **不写 rclone.conf**。用户随时可能回退到旧版主程序，那个远程必须还能用，
//!     所以这里只读。写进去的只有插件自己的 kv。
//!   * **不覆盖正在用的设置**。清单存在新的一格；站点的表单值只在原来是空的时候才填。
//!
//! 全部是纯函数：不发请求、不碰磁盘，于是能用构造出来的 JSON 直接测。

use serde_json::{json, Map, Value};

use crate::site;

/// 插件 kv 里存迁移清单的键。整站信息仍在 `site` 键（见 main.rs 的 `KEY`）。
pub const IMPORTED: &str = "imported";

/// 导出文件的格式号。字段形状变了就加一，读到不认识的号直接拒绝，不猜。
pub const FORMAT: u64 = 1;

/// 一次迁移最多读多少个远程的配置。每个远程要占一个 need 名额，
/// 没上限的话请求 JSON 会长到宿主管道边上；几十个远程本身已经不正常了。
pub const MAX_SCAN: usize = 32;

/// 远程参数里那个「这是 K-Vault」的标记。主程序 `KvCfg::is_marked` 用的是同一个。
const MARKER: &str = "kvault";

/// kv 记录和导出文件里放明文令牌的字段名。rclone 那边叫 `bearer_token`。
const TOKEN_FIELD: &str = "token";

/// 从 rclone 远程参数里读出来的一条设置。
///
/// `bearer` 是 rclone 存的那个 Obscured 串，不是明文：只有要拿它当新的整站令牌时
/// 才经 `config/reveal` 还原一次，其余时候只用来判断「令牌还在，只是不在我们手里」。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Found {
    pub name: String,
    pub base: String,
    pub folder: String,
    pub storage: String,
    pub dedup: bool,
    pub bearer: String,
}

/// rclone 的布尔参数是字符串 `"1"` / `"0"`，导出文件里是 `true` / `false`。
/// 两种都认，认不出来当假——宁可漏认一个开关，也不要把开关误读成开。
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        ),
        Some(Value::Number(number)) => number.as_i64().map(|n| n != 0).unwrap_or(false),
        _ => false,
    }
}

/// 这个远程是不是被标记成 K-Vault。
///
/// 只看标记位，不看类型：K-Vault 在 rclone 里就是普通 webdav，靠 `type` 认会把
/// 用户自己的 WebDAV 远程误认成 K-Vault，而插件没启用时那种误认最要命。
pub fn marked(params: &Value) -> bool {
    truthy(params.get(MARKER))
}

/// 从 `config/get` 的结果里读一条设置。没标记、或没有可用的站点地址，就返回 None。
///
/// 没有地址的记录搬过去也没用（建远程缺 `url`），所以这里直接不收，让调用方少报一条噪音。
pub fn found_of(name: &str, params: &Value) -> Option<Found> {
    if !marked(params) {
        return None;
    }
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let base = site::base_of(&text("url"));
    if base.is_empty() {
        return None;
    }
    let storage = {
        let raw = text("kvault_storage").to_ascii_lowercase();
        if raw.is_empty() {
            site::DEFAULT_STORAGE.to_string()
        } else {
            raw
        }
    };
    Some(Found {
        name: name.trim().to_string(),
        base,
        folder: text("kvault_folder"),
        storage,
        // 旧版没写这个参数时是开着的（见 kvault.rs 的默认值），所以缺省为真。
        dedup: match params.get("kvault_dedup") {
            Some(value) => truthy(Some(value)),
            None => true,
        },
        bearer: text("bearer_token"),
    })
}

/// 远程名列表。宿主两种形状都出现过：文档写的是裸数组 `["a","b"]`，
/// 实际 `caps.rs` 回的是 `{"remotes": [...], "source": "rc"}`。都认。
///
/// 列表里的元素可能是 `name:type`（`config/listremotes` 的原样输出），
/// 所以按 `:` 切一刀。重名的只留第一次出现的位置，顺序按宿主给的来。
pub fn remotes_of(value: &Value) -> Vec<String> {
    let items: &[Value] = match value {
        Value::Array(items) => items,
        Value::Object(map) => map
            .get("remotes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let raw = match item {
            Value::String(text) => text.clone(),
            Value::Object(map) => map
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        };
        let name = raw.split(':').next().unwrap_or("").trim().to_string();
        if !name.is_empty() && !out.iter().any(|seen| seen == &name) {
            out.push(name);
        }
    }
    out
}

/// kv 里已经记着的清单。没有或者不是对象就当空的。
pub fn imported_of(kv: &Value) -> Map<String, Value> {
    kv.get(IMPORTED)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// 主程序借这个命名空间存了两格自己的东西：`form` 是面板上正在输入的表单，
/// `_` 开头的是插件回的状态（`_state`）。它们不是远程名——`form` 里还带着用户
/// 刚敲下的令牌，当成远程记录搬走就会凭空多出一条叫 `form` 的远程，导出时连
/// 令牌一起写进文件。这两种格子一律不当作令牌副本。
fn is_host_slot(name: &str) -> bool {
    name == "form" || name.starts_with('_')
}

/// 旧版留下的明文令牌副本：`{"<远程名>": {"token": "…"}}`。
///
/// `skip` 是插件自己那一格（整站信息），它不是远程名，不能当令牌记录搬走。
/// 结果按名字排序，这样同样的输入总是导出同样的文件，便于用户 diff。
pub fn legacy_tokens(kv: &Value, skip: &str) -> Vec<(String, String)> {
    let Some(map) = kv.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in map {
        if name == skip || is_host_slot(name) {
            continue;
        }
        let token = value
            .get(TOKEN_FIELD)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if token.is_empty() {
            continue;
        }
        out.push((name.clone(), token));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 清单里一条记录。
///
/// `token_source` 说明令牌现在在哪：`kv` = 明文副本就在插件 kv 里，
/// `rclone` = 只在 rclone.conf 里（能用 `config/reveal` 取回），
/// `none` = 没有令牌（用账号密码认证的站点就是这样）。
/// 记录里不放令牌本身，清单因此可以随便看、随便导出。
pub fn entry(found: &Found, token_source: &str) -> Value {
    json!({
        "name": found.name,
        "base": found.base,
        "folder": found.folder,
        "storage": found.storage,
        "dedup": found.dedup,
        "token_source": token_source,
    })
}

/// 把清单里的一条设置变成 `Site` 存的形状，好让面板直接拿它填表单。
pub fn site_value(found: &Found, token: &str) -> Value {
    json!({
        "base": found.base,
        "token": token.trim(),
        "folder": found.folder,
        "name": found.name,
        "storage": found.storage,
        "dedup": found.dedup,
    })
}

/// 把一条记录装进 `state`——`state` 里只能放 JSON，所以要跨轮带过去就得先转一次。
pub fn to_value(found: &Found) -> Value {
    json!({
        "name": found.name,
        "base": found.base,
        "folder": found.folder,
        "storage": found.storage,
        "dedup": found.dedup,
        "bearer": found.bearer,
    })
}

/// `to_value` 的反向操作，同时也读清单里的记录（那种记录没有 `bearer`，令牌另存）。
///
/// 没有站点地址的记录直接当不存在：空地址填出来只会让人以为迁移成功了。
pub fn from_value(value: &Value) -> Option<Found> {
    let base = value
        .get("base")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if base.is_empty() {
        return None;
    }
    let text = |key: &str| -> String {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let storage = text("storage").to_lowercase();
    Some(Found {
        name: text("name"),
        base: base.to_string(),
        folder: text("folder"),
        storage: if storage.is_empty() {
            site::DEFAULT_STORAGE.to_string()
        } else {
            storage
        },
        dedup: match value.get("dedup") {
            Some(Value::Bool(flag)) => *flag,
            Some(other) => truthy(Some(other)),
            None => true,
        },
        bearer: text("bearer"),
    })
}

/// 导出文件。
///
/// 卸载插件会连 kv 一起删（宿主 §7.6 的「干净卸载」），所以导出是令牌唯一的退路。
/// `with_token` 为假时整站信息和令牌表里都不带令牌——那种文件可以贴进工单里。
pub fn export(
    site_kv: Option<&Map<String, Value>>,
    imported: &Map<String, Value>,
    tokens: &[(String, String)],
    with_token: bool,
) -> Value {
    let mut site_out = Map::new();
    if let Some(map) = site_kv {
        for (key, value) in map {
            if !with_token && key == TOKEN_FIELD {
                continue;
            }
            site_out.insert(key.clone(), value.clone());
        }
    }
    let mut tokens_out = Map::new();
    if with_token {
        for (name, token) in tokens {
            tokens_out.insert(name.clone(), Value::String(token.clone()));
        }
    }
    json!({
        "plugin": "kvault",
        "format": FORMAT,
        "token_included": with_token,
        "site": Value::Object(site_out),
        "imported": Value::Object(imported.clone()),
        "tokens": Value::Object(tokens_out),
    })
}

/// 读不回来的两种原因，调用方按语言包翻成人话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bad {
    /// 根本不是 JSON（粘贴时少了一截、或者混进了别的东西）。
    Json,
    /// 是 JSON，但不是本插件导出的那份（`plugin` / `format` 不对，或者字段形状不认识）。
    Shape,
}

/// 解析出来的内容。缺哪一节就是空的，不是错。
#[derive(Debug, Default)]
pub struct Doc {
    pub site: Option<Map<String, Value>>,
    pub imported: Map<String, Value>,
    pub tokens: Vec<(String, String)>,
}

fn known_source(source: &str) -> String {
    match source {
        "kv" | "rclone" => source.to_string(),
        _ => "none".to_string(),
    }
}

/// 读一份导出文件。
///
/// 只认 JSON 对象，且 `plugin == "kvault"`、`format` 是这一版认识的号——
/// 以后格式变了我能拒绝，而不是把半懂的文件写进 kv 把用户数据搅坏。
/// 清单里单条记录形状不对也算 `Shape`：宁可整份拒绝，也不写一半。
pub fn parse(text: &str) -> Result<Doc, Bad> {
    let value: Value = serde_json::from_str(text).map_err(|_| Bad::Json)?;
    let map = value.as_object().ok_or(Bad::Shape)?;
    let right_plugin = map.get("plugin").and_then(Value::as_str) == Some("kvault");
    let right_format = map.get("format").and_then(Value::as_u64) == Some(FORMAT);
    if !right_plugin || !right_format {
        return Err(Bad::Shape);
    }
    let site = map
        .get("site")
        .and_then(Value::as_object)
        .cloned()
        .filter(|section| !section.is_empty());
    let mut tokens: Vec<(String, String)> = Vec::new();
    if let Some(table) = map.get("tokens") {
        let table = table.as_object().ok_or(Bad::Shape)?;
        for (name, value) in table {
            let token = value.as_str().unwrap_or("").trim().to_string();
            if token.is_empty() {
                continue;
            }
            tokens.push((name.trim().to_string(), token));
        }
        tokens.sort_by(|a, b| a.0.cmp(&b.0));
    }
    let mut imported = Map::new();
    if let Some(entries) = map.get("imported") {
        let entries = entries.as_object().ok_or(Bad::Shape)?;
        for (name, value) in entries {
            let record = value.as_object().ok_or(Bad::Shape)?;
            let text = |key: &str| {
                record
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            let found = Found {
                name: name.trim().to_string(),
                base: site::base_of(&text("base")),
                folder: text("folder"),
                storage: {
                    let raw = text("storage").to_ascii_lowercase();
                    if raw.is_empty() {
                        site::DEFAULT_STORAGE.to_string()
                    } else {
                        raw
                    }
                },
                dedup: match record.get("dedup") {
                    Some(flag) => truthy(Some(flag)),
                    None => true,
                },
                bearer: String::new(),
            };
            if found.name.is_empty() || found.base.is_empty() {
                return Err(Bad::Shape);
            }
            // 文件里带的令牌表才是权威：有令牌就说明它搬得回去，否则沿用文件里那句说明。
            let source = if tokens.iter().any(|(name, _)| name == &found.name) {
                "kv".to_string()
            } else {
                known_source(&text("token_source"))
            };
            imported.insert(found.name.clone(), entry(&found, &source));
        }
    }
    Ok(Doc {
        site,
        imported,
        tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, Value)]) -> Value {
        let mut map = Map::new();
        for (key, value) in pairs {
            map.insert((*key).to_string(), value.clone());
        }
        Value::Object(map)
    }

    fn webdav() -> Value {
        params(&[
            ("type", json!("webdav")),
            ("url", json!("https://vault.example.com/dav")),
            ("bearer_token", json!("OBSCURED-TOKEN")),
            ("kvault", json!("1")),
            ("kvault_folder", json!("album")),
            ("kvault_storage", json!("R2")),
            ("kvault_dedup", json!("0")),
        ])
    }

    /// 认 K-Vault 只看标记位。没有标记的 webdav 远程（用户自己的 Nextcloud）
    /// 必须原样留在 rclone 里，不能被插件当成自己的东西。
    #[test]
    fn only_the_marker_makes_a_remote_kvault() {
        assert!(marked(&webdav()));

        let mut plain = webdav();
        plain.as_object_mut().unwrap().remove("kvault");
        assert!(!marked(&plain));
        assert_eq!(found_of("nextcloud", &plain), None);

        let mut off = webdav();
        off["kvault"] = json!("0");
        assert!(!marked(&off));
        assert_eq!(found_of("off", &off), None);

        let mut yes = webdav();
        yes["kvault"] = json!(true);
        assert!(found_of("yes", &yes).is_some());
    }

    /// 用户粘进 rclone 的往往是带 `/dav` 的 WebDAV 地址；清单里要存站点根，
    /// 否则面板会把它再拼一次 `/dav` 变成 `/dav/dav`。
    #[test]
    fn the_site_root_is_stripped_of_dav() {
        let found = found_of("my-vault", &webdav()).unwrap();
        assert_eq!(found.base, "https://vault.example.com");
        assert_eq!(found.folder, "album");
        assert_eq!(found.storage, "r2");
        assert!(!found.dedup, "kvault_dedup=0 应该是关");
        assert_eq!(found.bearer, "OBSCURED-TOKEN");
    }

    /// 旧版没写 `kvault_dedup` 时是开着的，迁移不能因为参数缺失就把它关掉。
    #[test]
    fn a_missing_dedup_switch_stays_on() {
        let mut bare = webdav();
        let map = bare.as_object_mut().unwrap();
        map.remove("kvault_dedup");
        map.remove("kvault_storage");
        let found = found_of("bare", &bare).unwrap();
        assert!(found.dedup);
        assert_eq!(found.storage, site::DEFAULT_STORAGE);
    }

    /// 没有可用地址的记录搬过去也建不出远程，宁可少报一条，也不留个点不动的条目。
    #[test]
    fn a_marked_remote_without_an_address_is_skipped() {
        let mut headless = webdav();
        headless.as_object_mut().unwrap().remove("url");
        assert_eq!(found_of("headless", &headless), None);
    }

    /// 宿主两种形状都出现过：文档里的裸数组，和 `caps.rs` 实际回的带 `remotes` 的对象。
    /// 只认一种，另一种机器上迁移就会静默地一条都找不到。
    #[test]
    fn the_remote_list_is_read_in_both_shapes() {
        let bare = json!(["k-vault", "gdrive"]);
        assert_eq!(remotes_of(&bare), vec!["k-vault", "gdrive"]);

        let wrapped = json!({"remotes": ["k-vault:webdav", "gdrive:drive"], "source": "rc"});
        assert_eq!(remotes_of(&wrapped), vec!["k-vault", "gdrive"]);

        let objects = json!({"remotes": [{"name": "a"}, {"name": "b:webdav"}]});
        assert_eq!(remotes_of(&objects), vec!["a", "b"]);

        // 重复的名字只留一份，顺序按宿主给的来。
        let dupes = json!(["b", "a", "b"]);
        assert_eq!(remotes_of(&dupes), vec!["b", "a"]);

        assert_eq!(remotes_of(&Value::Null), Vec::<String>::new());
        assert_eq!(remotes_of(&json!({"source": "conf"})), Vec::<String>::new());
    }

    /// 明文令牌副本里混着插件自己的 `site` 那一格和空令牌，都不能当远程记录搬走。
    /// 主程序存的 `form` / `_state` 同样不能当成远程名。
    #[test]
    fn legacy_tokens_skip_the_site_slot_and_blanks() {
        let kv = json!({
            "site": {"base": "https://vault.example.com", "token": "SITE-TOKEN"},
            // 用户在面板上刚敲进去的表单：里面有令牌，但它不是一条远程
            "form": {"base": "https://vault.example.com/", "token": "TYPED-TOKEN"},
            // 插件回的状态：长得像远程记录也不许搬
            "_state": {"status": "已就绪", "token": "STATE-TOKEN"},
            "b-remote": {"token": "B-TOKEN"},
            "a-remote": {"token": "A-TOKEN"},
            "empty": {"token": "  "},
            "noise": {"other": 1}
        });
        assert_eq!(
            legacy_tokens(&kv, "site"),
            vec![
                ("a-remote".to_string(), "A-TOKEN".to_string()),
                ("b-remote".to_string(), "B-TOKEN".to_string()),
            ]
        );
    }

    /// 清单里不存令牌本身——它只是个索引，导出时再决定要不要带令牌。
    #[test]
    fn the_inventory_never_carries_the_token() {
        let found = found_of("my-vault", &webdav()).unwrap();
        let record = entry(&found, "rclone");
        assert_eq!(record["token_source"], json!("rclone"));
        assert_eq!(record["base"], json!("https://vault.example.com"));
        assert!(record.get("token").is_none());
        assert!(!record.to_string().contains("OBSCURED-TOKEN"));
    }

    /// 导出默认抹掉令牌：这份文本经常被贴进工单、聊天、issue 里。
    #[test]
    fn export_hides_tokens_unless_asked() {
        let found = found_of("my-vault", &webdav()).unwrap();
        let mut imported = Map::new();
        imported.insert("my-vault".into(), entry(&found, "kv"));
        let mut site_kv = Map::new();
        site_kv.insert("base".into(), json!("https://vault.example.com"));
        site_kv.insert("token".into(), json!("SITE-TOKEN"));
        let tokens = vec![("my-vault".to_string(), "PLAIN-TOKEN".to_string())];

        let redacted = export(Some(&site_kv), &imported, &tokens, false);
        assert_eq!(redacted["token_included"], json!(false));
        assert!(redacted["site"].get("token").is_none());
        assert_eq!(redacted["site"]["base"], json!("https://vault.example.com"));
        assert!(redacted["tokens"].as_object().unwrap().is_empty());
        assert!(!redacted.to_string().contains("PLAIN-TOKEN"));

        let full = export(Some(&site_kv), &imported, &tokens, true);
        assert_eq!(full["site"]["token"], json!("SITE-TOKEN"));
        assert_eq!(full["tokens"]["my-vault"], json!("PLAIN-TOKEN"));
    }

    /// 导出再读回来，清单和令牌都得原样回来；读回来之后能被面板直接用。
    #[test]
    fn an_export_round_trips_through_parse() {
        let found = found_of("my-vault", &webdav()).unwrap();
        let mut imported = Map::new();
        imported.insert("my-vault".into(), entry(&found, "kv"));
        let mut site_kv = Map::new();
        site_kv.insert("base".into(), json!("https://vault.example.com"));
        site_kv.insert("token".into(), json!("SITE-TOKEN"));
        site_kv.insert("dedup".into(), json!(true));
        let tokens = vec![("my-vault".to_string(), "PLAIN-TOKEN".to_string())];

        let text = export(Some(&site_kv), &imported, &tokens, true).to_string();
        let doc = parse(&text).expect("自己导出的东西必须能读回来");

        let site = doc.site.expect("整站信息要回来");
        assert_eq!(site["base"], json!("https://vault.example.com"));
        assert_eq!(site["token"], json!("SITE-TOKEN"));
        assert_eq!(
            doc.tokens,
            vec![("my-vault".to_string(), "PLAIN-TOKEN".to_string())]
        );

        let record = doc.imported.get("my-vault").expect("清单要回来");
        assert_eq!(record["base"], json!("https://vault.example.com"));
        assert_eq!(record["storage"], json!("r2"));
        assert_eq!(record["dedup"], json!(false));
        // 文件里带了令牌，说明它搬得回去，来源就该是 kv。
        assert_eq!(record["token_source"], json!("kv"));
    }

    /// 粘贴别的东西（截断的 JSON、别的插件的导出、格式号不认识）都要整份拒绝，
    /// 不能把半懂的内容写进 kv 把用户的数据搅坏。
    #[test]
    fn parse_rejects_foreign_or_truncated_files() {
        assert_eq!(parse("{").unwrap_err(), Bad::Json);
        assert_eq!(parse("[]").unwrap_err(), Bad::Shape);
        assert_eq!(
            parse(r#"{"plugin":"other","format":1}"#).unwrap_err(),
            Bad::Shape
        );
        assert_eq!(
            parse(r#"{"plugin":"kvault","format":99,"site":{}}"#).unwrap_err(),
            Bad::Shape
        );
        // 清单里有一条形状不对，整份不认。
        assert_eq!(
            parse(r#"{"plugin":"kvault","format":1,"imported":{"a":"nope"}}"#).unwrap_err(),
            Bad::Shape
        );
        // 没有地址的记录同样不认：搬过去也建不出远程。
        assert_eq!(
            parse(r#"{"plugin":"kvault","format":1,"imported":{"a":{}}}"#).unwrap_err(),
            Bad::Shape
        );
    }

    /// 一条记录要能跨轮带走再带回来：中间那轮只经过 JSON，字段少一个就会丢设置。
    #[test]
    fn a_found_survives_a_trip_through_state() {
        let found = Found {
            name: "my-vault".into(),
            base: "https://vault.example.com".into(),
            folder: "album".into(),
            storage: "r2".into(),
            dedup: false,
            bearer: "OBSCURED".into(),
        };
        assert_eq!(from_value(&to_value(&found)), Some(found));
    }

    /// 清单里存的是没有 `bearer` 的记录，也得读得出来；空地址的记录直接当不存在。
    #[test]
    fn an_inventory_record_reads_back_without_a_bearer() {
        let found = from_value(&json!({
            "name": "old",
            "base": "https://old.example.com/",
            "storage": "",
            "dedup": true,
            "token_source": "rclone",
        }))
        .expect("有地址就该读出来");
        assert_eq!(found.name, "old");
        assert_eq!(found.storage, site::DEFAULT_STORAGE);
        assert_eq!(found.bearer, "");
        assert_eq!(from_value(&json!({"name": "x"})), None);
    }
}
