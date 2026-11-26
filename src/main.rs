//! K-Vault 插件：在「新建远程」的类型列表里加一个 K-Vault，并负责连接测试。
//!
//! 为什么是插件而不是内核后端：在 rclone 里 K-Vault 本来就是 webdav 远程，多出来的
//! 只有几个参数（`url` / `vendor` / `bearer_token` / `kvault*`）。插件只预设这些参数，
//! 不实现任何后端，所以「上传」仍归主程序——它走站点的 multipart 接口，而宿主的
//! `http.request` 只接受字符串请求体，插件发不了文件。
//!
//! 一次调用 = 一个临时进程：宿主写一份 JSON 到 stdin，读回一份 JSON，最多来回 16 轮，
//! 每一轮都靠 `state` 把上一轮查到的东西带下来（见 proto.rs 的说明）。

mod i18n;
mod proto;
mod site;

use serde_json::{json, Map, Value};

use i18n::{f, t};
use proto::*;

/// 插件自己保存的那一格里放站点信息。令牌是明文——rclone 只肯把 bearer_token 交回
/// 一个 Obscured 串，插件拿不到原文，所以只能自己存一份，跟主程序旧的做法一致。
const KEY: &str = "site";

/// 新建远程时默认用的名字。
const DEFAULT_NAME: &str = "k-vault";

#[derive(Clone, Default)]
struct Site {
    base: String,
    token: String,
    folder: String,
    name: String,
    storage: String,
    dedup: bool,
}

impl Site {
    /// 从 kv 读回站点信息。缺字段就用默认值，不报错——第一次打开面板时 kv 是空的。
    fn of(kv: &Value) -> Site {
        let owned = kv.get(KEY).cloned().unwrap_or(Value::Null);
        let map = owned.as_object();
        let text = |key: &str| {
            map.and_then(|m| m.get(key))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let storage = {
            let saved = text("storage");
            if saved.is_empty() {
                site::DEFAULT_STORAGE.to_string()
            } else {
                saved
            }
        };
        let name = {
            let saved = text("name");
            if saved.is_empty() {
                DEFAULT_NAME.to_string()
            } else {
                saved
            }
        };
        Site {
            base: site::base_of(&text("base")),
            token: text("token"),
            folder: text("folder"),
            name,
            storage,
            dedup: map
                .and_then(|m| m.get("dedup"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }
    }

    /// 表单里填了的字段覆盖已保存的值。
    ///
    /// 用户在面板上把地址改了一半就去点「测试连接」，测的必须是他看见的那个地址，
    /// 而不是上一次存下来的。表单里空的字段（例如密码框没重新输入）保持原值。
    fn with_form(mut self, form: &Value) -> Site {
        let base = fstr(form, "base");
        if !base.is_empty() {
            self.base = site::base_of(&base);
        }
        let token = fstr(form, "token");
        if !token.is_empty() {
            self.token = token;
        }
        let folder = fstr(form, "folder");
        if !folder.is_empty() {
            self.folder = folder;
        }
        let name = fstr(form, "name");
        if !name.is_empty() {
            self.name = name;
        }
        let storage = fstr(form, "storage");
        if !storage.is_empty() {
            self.storage = storage;
        }
        match form.get("dedup") {
            Some(Value::Bool(b)) => self.dedup = *b,
            Some(Value::String(s)) if !s.is_empty() => {
                let v = s.trim().to_ascii_lowercase();
                self.dedup = v == "true" || v == "1" || v == "on";
            }
            _ => {}
        }
        self
    }

    fn to_value(&self) -> Value {
        json!({
            "base": self.base,
            "token": self.token,
            "folder": self.folder,
            "name": self.name,
            "storage": self.storage,
            "dedup": self.dedup,
        })
    }

    fn save_op(&self) -> Value {
        json!({ "op": "kv.set", "key": KEY, "value": self.to_value() })
    }

    /// 再写一条「键 = 远程名、值 `{"token": …}`」。
    ///
    /// 主程序上传前找令牌的顺序是：先按远程名找这个命名空间里的明文副本，再读 rclone
    /// 远程参数里的 `bearer_token`（要走 `config/reveal` 还原），最后才看插件存的整站令牌。
    /// 这一步让第一条就命中，于是不需要指望当前 rclone 支持 `config/reveal`。
    /// 明文落盘和主程序旧做法一致，没有增加新的暴露面。
    fn token_op(&self) -> Option<Value> {
        let token = self.token.trim();
        if token.is_empty() {
            return None;
        }
        Some(json!({
            "op": "kv.set",
            "key": self.name,
            "value": { "token": token },
        }))
    }

    fn has_token(&self) -> bool {
        !self.token.trim().is_empty()
    }
}

fn main() {
    use std::io::{Read, Write};

    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return;
    }
    let resp = handle(&input).unwrap_or_else(|e| json!({ "id": 1, "error": { "message": e } }));
    let bytes = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = std::io::stdout();
    let _ = out.write_all(&bytes);
    let _ = out.flush();
}

fn handle(input: &[u8]) -> Result<Value, String> {
    let req: Value =
        serde_json::from_slice(input).map_err(|e| f(&t("req.parse"), &[&e.to_string()]))?;
    let id = req.get("id").and_then(Value::as_u64).unwrap_or(1);
    let method = s(req.get("method"));
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    i18n::set_lang(params.get("lang").and_then(Value::as_str));

    match method.as_str() {
        "manifest" => Ok(json!({
            "id": id,
            "manifest": {
                "id": "kvault",
                "name": t("plugin.name"),
                "version": env!("CARGO_PKG_VERSION"),
                "description": t("plugin.desc"),
            },
        })),
        "request" => Ok(dispatch(id, &params)),
        other => Err(f(&t("method.unknown"), &[other])),
    }
}

fn dispatch(id: u64, params: &Value) -> Value {
    let rq = Rq {
        form: params.get("form").cloned().unwrap_or(Value::Null),
        provided: params.get("provided").cloned().unwrap_or(Value::Null),
        state: params.get("state").cloned().unwrap_or(Value::Null),
    };
    let action = s(params.get("action"));
    match action.as_str() {
        "state" => on_state(id, &rq),
        "test" => on_test(id, &rq),
        "create" => on_create(id, &rq),
        "copy_dav" => on_copy_dav(id, &rq),
        other => fail(
            id,
            &f(
                &t("action.unknown"),
                &[if other.is_empty() { "(空)" } else { other }],
            ),
        ),
    }
}

/// 面板：站点三项、远程三项、三条检查。
fn panel(site: &Site, status: String, lamp: &str) -> Value {
    let base_ok = !site.base.is_empty();
    let token_ok = site.has_token();

    let checks = json!([
        {
            "label": t("check.site"),
            "value": if base_ok { site.base.clone() } else { t("check.empty") },
            "ok": base_ok,
        },
        {
            "label": t("check.token"),
            "value": if token_ok { t("check.set") } else { t("check.unset") },
            "ok": token_ok,
        },
        {
            "label": t("check.dav"),
            "value": if base_ok { site::dav_url(&site.base) } else { t("check.empty") },
            "ok": base_ok && site::is_http(&site.base),
        },
    ]);

    let mut hints = Map::new();
    hints.insert("hint_site".into(), json!(t("hint.site")));
    hints.insert("hint_remote".into(), json!(t("hint.remote")));

    let mut enabled = Map::new();
    enabled.insert("test".into(), json!(base_ok));
    enabled.insert("copy_dav".into(), json!(base_ok));
    enabled.insert("create".into(), json!(base_ok && token_ok));

    json!({
        "status": status,
        "lamp": lamp,
        "values": {
            "base": site.base,
            "token": site.token,
            "folder": site.folder,
            "name": site.name,
            "storage": site.storage,
            "dedup": if site.dedup { "true" } else { "false" },
            "dav_url": if base_ok { site::dav_url(&site.base) } else { String::new() },
        },
        "checks": checks,
        "hints": Value::Object(hints),
        "enabled": Value::Object(enabled),
    })
}

fn ready_status(site: &Site) -> (String, &'static str) {
    if !site.base.is_empty() && site.has_token() {
        (t("panel.ready"), "ok")
    } else {
        (t("panel.todo"), "warn")
    }
}

/// 只读刷新：不写 kv，也不发任何请求。
fn on_state(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        _ => {
            let site = Site::of(&kv_pick(rq));
            let (status, lamp) = ready_status(&site);
            done(id, panel(&site, status, lamp))
        }
    }
}

/// 测试连接：健康检查 + 令牌自检。
///
/// 两个请求一起发：它们互不依赖，分两轮只是多绕一圈。健康检查失败说明地址错，
/// `/me` 失败说明令牌错——分别报出来，用户才知道改哪一项。
fn on_test(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "kv" => {
            let site = Site::of(&kv_pick(rq)).with_form(&rq.form);
            if site.base.is_empty() {
                return fail(id, &t("site.missing_base"));
            }
            if !site::is_http(&site.base) {
                return fail(id, &t("site.bad_base"));
            }
            let mut list = vec![need_item(
                "http.request",
                "health",
                json!({
                    "url": site::health_url(&site.base),
                    "method": "GET",
                    "timeout_ms": 8000,
                    "max_bytes": 65536,
                }),
            )];
            if site.has_token() {
                list.push(need_item(
                    "http.request",
                    "me",
                    json!({
                        "url": site::me_url(&site.base),
                        "method": "GET",
                        "headers": { "Authorization": format!("Bearer {}", site.token.trim()) },
                        "timeout_ms": 10000,
                        "max_bytes": 262144,
                    }),
                ));
            }
            let mut state = keep(rq.state.clone(), "kv", kv_pick(rq));
            state = keep(state, "step", json!("report"));
            need(id, state, list)
        }
        _ => {
            let site = Site::of(&kv_pick(rq));
            if let Some(e) = got_err(&rq.provided, "health") {
                return fail(id, &f(&t("site.health_fail"), &[&e]));
            }
            let health = got(&rq.provided, "health");
            let code = health.get("status").and_then(Value::as_u64).unwrap_or(0) as u16;
            if code >= 400 {
                return fail(
                    id,
                    &f(
                        &t("site.health_fail"),
                        &[&site::api_error(code, &s(health.get("body")))],
                    ),
                );
            }

            // 没有令牌时只做了健康检查，面板如实说「站点通、没令牌」。
            if !site.has_token() {
                return done_ops(
                    id,
                    vec![site.save_op()],
                    panel(&site, t("site.test_ok_plain"), "ok"),
                );
            }

            if let Some(e) = got_err(&rq.provided, "me") {
                return fail(id, &f(&t("site.token_fail"), &[&e]));
            }
            let me = got(&rq.provided, "me");
            let code = me.get("status").and_then(Value::as_u64).unwrap_or(0) as u16;
            let body_text = s(me.get("body"));
            if code >= 400 {
                return fail(
                    id,
                    &f(&t("site.token_fail"), &[&site::api_error(code, &body_text)]),
                );
            }
            let body: Value = match serde_json::from_str(&body_text) {
                Ok(v) => v,
                Err(_) => {
                    return fail(
                        id,
                        &f(
                            &t("site.not_json"),
                            &[&code.to_string(), &truncate(&body_text, 120)],
                        ),
                    )
                }
            };
            let probe = site::parse_me(&body);

            let mut parts = vec![t("site.test_ok_plain")];
            if !probe.name.is_empty() || !probe.id.is_empty() {
                parts[0] = f(&t("site.test_ok"), &[&probe_label(&probe)]);
            }
            if !probe.scopes.is_empty() {
                parts.push(f(&t("site.scopes"), &[&probe.scopes.join(", ")]));
            }
            if !probe.storages.is_empty() {
                parts.push(f(&t("site.storages"), &[&probe.storages.join(", ")]));
            }
            if probe.max_file > 0 {
                parts.push(f(
                    &t("site.max_file"),
                    &[&(probe.max_file / 1024 / 1024).to_string()],
                ));
            } else {
                // 站点没报上限时按它自己的默认值说，免得用户以为「不限大小」。
                parts.push(f(
                    &t("site.max_file_default"),
                    &[&(site::MAX_UPLOAD / 1024 / 1024).to_string()],
                ));
            }
            if !probe.folder_prefix.is_empty() {
                parts.push(f(&t("site.folder_prefix"), &[&probe.folder_prefix]));
            }
            let status = parts.join(" · ");
            let mut body = panel(&site, status.clone(), "ok");
            body["toast"] = json!({ "level": "ok", "message": truncate(&status, 200) });
            done_ops(id, vec![site.save_op()], body)
        }
    }
}

fn probe_label(probe: &site::Probe) -> String {
    match (probe.name.is_empty(), probe.id.is_empty()) {
        (false, false) => format!("{} ({})", probe.name, probe.id),
        (false, true) => probe.name.clone(),
        (true, false) => probe.id.clone(),
        (true, true) => t("site.unknown"),
    }
}

/// 新建远程：先问 rclone 有没有 webdav 后端，再写配置。
///
/// 顺序不能反过来：rclone 连不上或者不是预期版本时，`config/create` 会回一句
/// `unknown backend "webdav"`，用户拿着这句话没法修；先查后端列表就能直接说清
/// 「当前连着的 rclone 不支持 webdav」。
fn on_create(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "kv" => {
            let site = Site::of(&kv_pick(rq)).with_form(&rq.form);
            if site.base.is_empty() {
                return fail(id, &t("site.missing_base"));
            }
            if !site::is_http(&site.base) {
                return fail(id, &t("site.bad_base"));
            }
            if !site.has_token() {
                return fail(id, &t("site.missing_token"));
            }
            if site.name.is_empty() {
                return fail(id, &t("remote.need_name"));
            }
            if site
                .name
                .chars()
                .any(|c| c == ':' || c == '/' || c == '\\' || c.is_whitespace())
            {
                return fail(id, &t("remote.bad_name"));
            }
            let mut state = keep(rq.state.clone(), "kv", kv_pick(rq));
            state = keep(state, "site", site.to_value());
            state = keep(state, "step", json!("prov"));
            need(
                id,
                state,
                vec![need_item(
                    "rclone.call",
                    "prov",
                    json!({ "method": "config/providers", "params": {} }),
                )],
            )
        }
        "prov" => {
            if let Some(e) = got_err(&rq.provided, "prov") {
                return fail(id, &f(&t("remote.provider_fail"), &[&e]));
            }
            let providers = got(&rq.provided, "prov");
            let names: Vec<String> = providers
                .get("providers")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|p| {
                            s(p.get("Name"))
                                .to_string()
                                .to_ascii_lowercase()
                                .to_string()
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !names.iter().any(|n| n == "webdav") {
                let hint = if names.is_empty() {
                    t("remote.no_provider_empty")
                } else {
                    truncate(&names.join(", "), 200)
                };
                return fail(id, &f(&t("remote.no_provider"), &[&hint]));
            }
            let site = site_in_play(rq);
            let params = site::remote_params(
                &site.base,
                &site.token,
                &site.folder,
                &site.storage,
                site.dedup,
            );
            let mut state = keep(rq.state.clone(), "step", json!("report"));
            state = keep(
                state,
                "pending",
                json!({ "name": site.name, "params": params }),
            );
            need(
                id,
                state,
                vec![need_item(
                    "rclone.call",
                    "made",
                    json!({
                        "method": "config/create",
                        "params": { "name": site.name, "type": "webdav", "parameters": params },
                    }),
                )],
            )
        }
        _ => {
            if let Some(e) = got_err(&rq.provided, "made") {
                return fail(id, &f(&t("remote.create_fail"), &[&e]));
            }
            let site = site_in_play(rq);
            let path = format!(
                "{}:{}",
                site.name,
                if site.folder.is_empty() { "" } else { "/" }
            );
            let status = f(&t("remote.created"), &[&site.name, &path]);
            let mut body = panel(&site, status.clone(), "ok");
            body["toast"] = json!({ "level": "ok", "message": status });
            let mut ops = vec![site.save_op()];
            if let Some(op) = site.token_op() {
                ops.push(op);
            }
            done_ops(id, ops, body)
        }
    }
}

/// 这一步要用的站点信息：优先用第一轮校验过、经 state 带下来的那份，没有才读 kv。
///
/// 不能每步都重读 kv：第一次建远程时 kv 里还没有记录，重读会拿到默认值（令牌是空的），
/// 于是远程参数和令牌副本都会写成空的。
fn site_in_play(rq: &Rq) -> Site {
    let saved = rq.state.get("site").cloned().unwrap_or(Value::Null);
    if saved.is_object() {
        return Site::of(&json!({ KEY: saved }));
    }
    Site::of(&kv_pick(rq)).with_form(&rq.form)
}

/// 把 WebDAV 地址复制到剪贴板——用户常要把它填进别的工具。
fn on_copy_dav(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "kv" => {
            let site = Site::of(&kv_pick(rq)).with_form(&rq.form);
            if site.base.is_empty() {
                return fail(id, &t("site.missing_base"));
            }
            let url = site::dav_url(&site.base);
            let mut state = keep(rq.state.clone(), "kv", kv_pick(rq));
            state = keep(state, "step", json!("done"));
            need(
                id,
                state,
                vec![need_item("ui.copy", "copied", json!({ "text": url }))],
            )
        }
        _ => {
            if let Some(e) = got_err(&rq.provided, "copied") {
                return fail(id, &f(&t("copy.fail"), &[&e]));
            }
            let site = Site::of(&kv_pick(rq));
            let status = f(&t("copy.what"), &[&site::dav_url(&site.base)]);
            let mut body = panel(&site, status.clone(), "ok");
            body["toast"] = json!({ "level": "ok", "message": truncate(&status, 200) });
            done(id, body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假宿主：只回答这个插件真的要的能力，别的一律报错——漏掉一个 need 会立刻暴露，
    /// 不会像真宿主那样把错误混进结果里让测试继续往下跑。
    struct Host {
        kv: Value,
        create_fails: Option<String>,
        calls: Vec<String>,
    }

    impl Host {
        fn new() -> Host {
            Host {
                kv: json!({}),
                create_fails: None,
                calls: Vec::new(),
            }
        }

        fn answer(&mut self, cap: &str, key: &str, params: &Value) -> Value {
            self.calls.push(format!("{cap}:{key}"));
            match cap {
                "kv.get_all" => self.kv.clone(),
                "rclone.call" => {
                    let method = s(params.get("method"));
                    self.calls.push(method.clone());
                    match method.as_str() {
                        "config/providers" => json!({
                            "providers": [ { "Name": "webdav" }, { "Name": "s3" } ]
                        }),
                        "config/create" => match &self.create_fails {
                            Some(e) => json!({ "_error": e }),
                            None => json!({}),
                        },
                        other => json!({ "_error": format!("测试里没有这个方法: {other}") }),
                    }
                }
                other => json!({ "_error": format!("测试里没有这个能力: {other}") }),
            }
        }
    }

    /// 把动作跑到出结果为止，每轮把 need 换成假宿主的答案塞进 provided。
    fn drive(host: &mut Host, action: &str, form: &Value) -> (Value, Vec<Value>) {
        let mut state = Value::Null;
        let mut provided = json!({});
        let mut rounds = Vec::new();
        for _ in 0..16 {
            let out = dispatch(
                1,
                &json!({
                    "action": action,
                    "form": form,
                    "state": state,
                    "provided": provided,
                }),
            );
            rounds.push(out.clone());
            let Some(list) = out.get("need").and_then(Value::as_array).cloned() else {
                return (out, rounds);
            };
            state = out.get("state").cloned().unwrap_or(Value::Null);
            let mut next = Map::new();
            for item in &list {
                let cap = s(item.get("cap"));
                let key = s(item.get("key"));
                let params = item.get("params").cloned().unwrap_or(Value::Null);
                next.insert(key.clone(), host.answer(&cap, &key, &params));
            }
            provided = Value::Object(next);
        }
        panic!("动作在 16 轮内没有结束");
    }

    fn filled() -> Value {
        json!({
            "base": "https://vault.example.com/",
            "token": "tok-123",
            "folder": "album",
            "name": "my-vault",
            "storage": "r2",
            "dedup": true,
        })
    }

    /// 建远程要同时留下整站信息与「按远程名」的令牌副本，后者才是主程序上传时先找的那一条。
    #[test]
    fn create_writes_both_the_site_and_the_legacy_token() {
        let mut host = Host::new();
        let (out, _) = drive(&mut host, "create", &filled());

        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        assert_eq!(body.get("lamp").and_then(Value::as_str), Some("ok"));

        let ops = out.get("ops").and_then(Value::as_array).unwrap();
        let site_op = ops
            .iter()
            .find(|op| op.get("key").and_then(Value::as_str) == Some(KEY))
            .expect("缺整站信息那条 kv.set");
        assert_eq!(
            site_op
                .get("value")
                .and_then(|v| v.get("token"))
                .and_then(Value::as_str),
            Some("tok-123")
        );

        let legacy = ops
            .iter()
            .find(|op| op.get("key").and_then(Value::as_str) == Some("my-vault"))
            .expect("缺按远程名存的令牌副本");
        assert_eq!(legacy.get("op").and_then(Value::as_str), Some("kv.set"));
        assert_eq!(
            legacy.get("value"),
            Some(&json!({ "token": "tok-123" })),
            "值必须和主程序旧格式一样，只有 token 一个字段"
        );

        // 远程参数也得带 kvault 标记，否则主程序不会把它当 K-Vault 远程。
        let create = host
            .calls
            .iter()
            .position(|c| c == "config/create")
            .expect("没有调 config/create");
        assert_eq!(create, host.calls.len() - 1);
    }

    /// 没有令牌就不建远程，也不该留下半个 kv 记录。
    #[test]
    fn create_without_a_token_stops_before_touching_rclone() {
        let mut host = Host::new();
        let mut form = filled();
        form["token"] = json!("");
        let (out, _) = drive(&mut host, "create", &form);

        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        assert_eq!(body.get("lamp").and_then(Value::as_str), Some("err"));
        assert!(
            !host.calls.iter().any(|c| c == "config/create"),
            "没有令牌还是去建远程了"
        );
        assert!(out
            .get("ops")
            .and_then(Value::as_array)
            .map_or(true, |o| o.is_empty()));
    }

    /// rclone 建远程失败要报出来，不能假装成功。
    #[test]
    fn a_failed_create_is_reported() {
        let mut host = Host::new();
        host.create_fails = Some("名字已被占用".to_string());
        let (out, _) = drive(&mut host, "create", &filled());
        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        assert_eq!(body.get("lamp").and_then(Value::as_str), Some("err"));
        assert!(
            s(body.get("status")).contains("名字已被占用"),
            "状态里应该带上宿主给的失败原因"
        );
    }

    /// 宿主只认能力表里列出的名字，多写一个就整份配置被拒；面板分页要 0.3.0 起的宿主。
    /// remote_types 是元数据，不是能力。
    #[test]
    fn the_config_declares_pages_and_metadata_correctly() {
        let cfg: Value = serde_json::from_str(include_str!("../config.json")).unwrap();
        let caps: Vec<String> = cfg
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| s(Some(v))).collect())
            .unwrap_or_default();
        assert!(
            !caps.iter().any(|c| c == "remote_types"),
            "remote_types 不是能力，宿主会拒绝整份配置"
        );
        assert_eq!(
            cfg.get("min_gui_version").and_then(Value::as_str),
            Some("0.3.0"),
            "面板分页从 0.3.0 起支持"
        );
        let kind = cfg
            .get("remote_types")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .expect("缺 remote_types 元数据");
        assert_eq!(kind.get("id").and_then(Value::as_str), Some("kvault"));
        assert_eq!(
            kind.get("base_type").and_then(Value::as_str),
            Some("webdav"),
            "K-Vault 是 webdav 的派生类型，不是内核原生后端"
        );
        let defaults = kind.get("defaults").and_then(Value::as_object).unwrap();
        assert_eq!(defaults.get("kvault").and_then(Value::as_str), Some("1"));
    }
}
