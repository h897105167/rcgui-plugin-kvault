//! K-Vault 插件：在「新建远程」的类型列表里加一个 K-Vault，负责连接测试，
//! 并且自己处理站点上的文件（列表、删除、上传）。
//!
//! 为什么这些事在插件里而不是内核：在 rclone 里 K-Vault 本来就是 webdav 远程，
//! 多出来的只有几个参数（`url` / `vendor` / `bearer_token` / `kvault*`）。站点怎么
//! 认令牌、怎么分页、怎么删、上传表单里有哪几个字段，全是这个站点自己的约定，
//! 只有插件知道。宿主只做通用的事：把插件写好的请求发出去，以及在用户挑了某个
//! 文件之后把那一个文件的字节按 multipart 流出去——插件从头到尾拿不到本地路径。
//!
//! 一次调用 = 一个临时进程：宿主写一份 JSON 到 stdin，读回一份 JSON，最多来回 16 轮，
//! 每一轮都靠 `state` 把上一轮查到的东西带下来（见 proto.rs 的说明）。

mod api;
mod i18n;
mod migrate;
mod proto;
mod site;

#[cfg(test)]
mod wire;

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
        "migrate" => on_migrate(id, &rq),
        "adopt" => on_adopt(id, &rq),
        "export" => on_export(id, &rq),
        "restore" => on_restore(id, &rq),
        "list" => on_list(id, &rq),
        "remove" => on_remove(id, &rq),
        "upload" => on_upload(id, &rq),
        "plan" => on_plan(id, &rq),
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
    hints.insert("hint_migrate".into(), json!(t("hint.migrate")));
    hints.insert("hint_data".into(), json!(t("hint.data")));
    hints.insert("hint_files".into(), json!(t("hint.files")));

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

/// 面板上「选哪一条」的输入框：迁移完会填上第一条，之后「用这条填表单」靠它。
const PICK: &str = "pick";

/// 用一条迁移记录拼出能直接填进表单的整站信息。
fn site_from_found(found: &migrate::Found, token: &str) -> Site {
    Site::of(&json!({ KEY: migrate::site_value(found, token) }))
}

/// 表单里的开关两种形状都收：宿主交的是字符串 `"true"`/`"false"`，测试里常直接给布尔。
fn form_flag(form: &Value, key: &str) -> bool {
    migrate::truthy(form.get(key))
}

fn state_found(state: &Value) -> Option<migrate::Found> {
    migrate::from_value(state.get("found").unwrap_or(&Value::Null))
}

fn state_founds(state: &Value) -> Vec<migrate::Found> {
    state
        .get("founds")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(migrate::from_value).collect())
        .unwrap_or_default()
}

fn state_strings(state: &Value, key: &str) -> Vec<String> {
    state
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 从旧版主程序把设置搬过来。
///
/// 只写插件自己的 kv，一个字节也不往 rclone.conf 里写：用户随时可能退回旧版主程序，
/// 那个远程必须还在、还能用。整站表单值也只在原来是空的时候才填，不覆盖正在用的设置。
///
/// 轮数不随远程个数增长：列远程一轮，各远程的配置凑在同一个 need 里，再一轮收尾。
fn on_migrate(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "kv" => {
            let mut state = keep(rq.state.clone(), "kv", kv_pick(rq));
            state = keep(state, "step", json!("remotes"));
            need(
                id,
                state,
                vec![need_item("rclone.remotes", "remotes", json!({}))],
            )
        }
        "remotes" => {
            let names = migrate::remotes_of(&got(&rq.provided, "remotes"));
            // rclone.remotes 读不到（控制台没连上、RC 密码不对）时退一步问 RC 要名单。
            // 只退一次：再失败就该照实说读不到，而不是在两步之间来回绕到轮数用完。
            if names.is_empty()
                && got_err(&rq.provided, "remotes").is_some()
                && state_s(&rq.state, "retry").is_empty()
            {
                let mut state = keep(rq.state.clone(), "retry", json!("1"));
                state = keep(state, "step", json!("remotes"));
                return need(
                    id,
                    state,
                    vec![need_item(
                        "rclone.call",
                        "remotes",
                        json!({ "method": "config/listremotes", "params": {} }),
                    )],
                );
            }
            if names.is_empty() {
                return fail(id, &t("data.migrate_none"));
            }
            let mut state = keep(rq.state.clone(), "names", json!(names.clone()));
            state = keep(state, "step", json!("cfg"));
            let items: Vec<Value> = names
                .iter()
                .take(migrate::MAX_SCAN)
                .enumerate()
                .map(|(i, name)| {
                    need_item(
                        "rclone.call",
                        &format!("cfg:{i}"),
                        json!({ "method": "config/get", "params": { "name": name } }),
                    )
                })
                .collect();
            need(id, state, items)
        }
        "cfg" => {
            let names = state_strings(&rq.state, "names");
            let mut founds = Vec::new();
            let mut notes = Vec::new();
            for (i, name) in names.iter().enumerate() {
                let key = format!("cfg:{i}");
                if let Some(err) = got_err(&rq.provided, &key) {
                    // 一个远程读不出来不该拖垮整次迁移：剩下的照样搬，这条记进备注。
                    notes.push(format!("{name}: {}", truncate(&err, 80)));
                    continue;
                }
                if let Some(found) = migrate::found_of(name, &got(&rq.provided, &key)) {
                    founds.push(found);
                }
            }
            let kv = kv_pick(rq);
            let legacy = migrate::legacy_tokens(&kv, KEY);
            // 只有整站表单还空着才需要拿一个令牌填进去；已经有设置就不碰它。
            let seed_needs_token = Site::of(&kv).base.is_empty()
                && founds
                    .first()
                    .map(|seed| {
                        !seed.bearer.is_empty()
                            && !legacy.iter().any(|(name, _)| name == &seed.name)
                    })
                    .unwrap_or(false);
            let mut state = keep(
                rq.state.clone(),
                "founds",
                json!(founds.iter().map(migrate::to_value).collect::<Vec<_>>()),
            );
            state = keep(state, "notes", json!(notes));
            if seed_needs_token {
                let bearer = founds[0].bearer.clone();
                state = keep(state, "step", json!("rvl"));
                return need(
                    id,
                    state,
                    vec![need_item(
                        "rclone.call",
                        "rvl",
                        json!({ "method": "config/reveal", "params": { "value": bearer } }),
                    )],
                );
            }
            finish_migrate(id, rq, &founds, &notes, "")
        }
        _ => {
            let founds = state_founds(&rq.state);
            let notes = state_strings(&rq.state, "notes");
            // 还原失败就当没有令牌：地址和参数照样填，用户补一次令牌就能用。
            let revealed = match got_err(&rq.provided, "rvl") {
                Some(_) => String::new(),
                None => got(&rq.provided, "rvl")
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            };
            finish_migrate(id, rq, &founds, &notes, &revealed)
        }
    }
}

/// 迁移收尾：写清单，必要时顺手把整站表单填上。
fn finish_migrate(
    id: u64,
    rq: &Rq,
    founds: &[migrate::Found],
    notes: &[String],
    revealed: &str,
) -> Value {
    let kv = kv_pick(rq);
    let legacy = migrate::legacy_tokens(&kv, KEY);
    let mut imported = migrate::imported_of(&kv);
    for found in founds {
        let source = if legacy.iter().any(|(name, _)| name == &found.name) {
            "kv"
        } else if found.bearer.is_empty() {
            "none"
        } else {
            "rclone"
        };
        imported.insert(found.name.clone(), migrate::entry(found, source));
    }

    let mut ops = Vec::new();
    if !founds.is_empty() {
        ops.push(json!({
            "op": "kv.set",
            "key": migrate::IMPORTED,
            "value": Value::Object(imported.clone()),
        }));
    }

    let mut site = Site::of(&kv);
    let mut seeded = String::new();
    if site.base.is_empty() {
        if let Some(seed) = founds.first() {
            let token = legacy
                .iter()
                .find(|(name, _)| name == &seed.name)
                .map(|(_, token)| token.clone())
                .unwrap_or_else(|| revealed.to_string());
            ops.push(json!({
                "op": "kv.set",
                "key": KEY,
                "value": migrate::site_value(seed, &token),
            }));
            site = site_from_found(seed, &token);
            seeded = seed.name.clone();
        }
    }

    let total = founds.len().to_string();
    let known = imported.len().to_string();
    let mut status = if founds.is_empty() {
        t("data.migrate_none")
    } else {
        f("data.migrate_ok", &[&total, &known])
    };
    if !seeded.is_empty() {
        status = format!("{}；{}", status, f("data.seeded", &[&seeded]));
    }
    if !notes.is_empty() {
        let joined = truncate(&notes.join("、"), 120);
        status = format!("{}；{}", status, f("data.migrate_notes", &[&joined]));
    }
    let lamp = if founds.is_empty() { "warn" } else { "ok" };
    let mut body = panel(&site, status.clone(), lamp);
    // 「记录名」留给下一步的 adopt 用：用户自己填过的就不动，否则先把第一条摆上去，
    // 免得他回头还得从清单里手抄一个名字。整站是这次填上的时候优先放那一条。
    let typed = fstr(&rq.form, PICK);
    let pick = if !seeded.is_empty() {
        seeded.clone()
    } else if !typed.is_empty() {
        typed
    } else {
        founds.first().map(|f| f.name.clone()).unwrap_or_default()
    };
    body["values"]["pick"] = json!(pick);
    body["toast"] = json!({ "level": lamp, "message": truncate(&status, 200) });
    done_ops(id, ops, body)
}

/// 把清单里的一条记录填进表单，什么都不写。
///
/// 要分两步是因为令牌可能只在 rclone.conf 里（Obscured 串），得经 `config/reveal` 还原一次。
/// 远程已经删掉了也不要紧：地址和参数照样填得上，令牌用户自己补。
fn on_adopt(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "kv" => {
            let kv = kv_pick(rq);
            let pick = fstr(&rq.form, PICK);
            if pick.is_empty() {
                return fail(id, &t("data.need_pick"));
            }
            let found = match migrate::imported_of(&kv)
                .get(&pick)
                .and_then(migrate::from_value)
            {
                Some(found) => found,
                None => return fail(id, &f("data.adopt_missing", &[&pick])),
            };
            if let Some((_, token)) = migrate::legacy_tokens(&kv, KEY)
                .iter()
                .find(|(name, _)| name == &pick)
            {
                // 明文副本就在插件 kv 里，不用再问 rclone。
                return adopt_done(id, &found, token);
            }
            let mut state = keep(rq.state.clone(), "kv", kv);
            state = keep(state, "found", migrate::to_value(&found));
            state = keep(state, "step", json!("cfg"));
            need(
                id,
                state,
                vec![need_item(
                    "rclone.call",
                    "cfg",
                    json!({ "method": "config/get", "params": { "name": pick } }),
                )],
            )
        }
        "cfg" => {
            let found = match state_found(&rq.state) {
                Some(found) => found,
                None => return fail(id, &f("data.adopt_missing", &[&fstr(&rq.form, PICK)])),
            };
            if got_err(&rq.provided, "cfg").is_some() {
                return adopt_done(id, &found, "");
            }
            let bearer = got(&rq.provided, "cfg")
                .get("bearer_token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if bearer.is_empty() {
                // 账号密码认证的站点本来就没有 bearer，地址和参数已经够了。
                return adopt_done(id, &found, "");
            }
            let state = keep(rq.state.clone(), "step", json!("rvl"));
            need(
                id,
                state,
                vec![need_item(
                    "rclone.call",
                    "rvl",
                    json!({ "method": "config/reveal", "params": { "value": bearer } }),
                )],
            )
        }
        _ => {
            let found = match state_found(&rq.state) {
                Some(found) => found,
                None => return fail(id, &f("data.adopt_missing", &[&fstr(&rq.form, PICK)])),
            };
            let token = got(&rq.provided, "rvl")
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            adopt_done(id, &found, &token)
        }
    }
}

/// 填好表单就收手：不写 kv，也不碰 rclone——用户点「建远程」才算落章。
fn adopt_done(id: u64, found: &migrate::Found, token: &str) -> Value {
    let site = site_from_found(found, token);
    let status = if site.has_token() {
        f("data.adopt_ok", &[&site.name])
    } else {
        f("data.adopt_none", &[&site.name])
    };
    let mut body = panel(&site, status.clone(), "warn");
    body["values"]["pick"] = json!(found.name);
    body["toast"] = json!({ "level": "ok", "message": truncate(&status, 200) });
    done(id, body)
}

/// 导出一份能被「恢复」读回来的文件，同时塞进剪贴板。
///
/// 卸载插件会连 kv 一起删，所以导出是令牌唯一的退路。默认不带令牌——那种文件可以
/// 直接贴进工单里问人。
fn on_export(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "kv" => {
            let kv = kv_pick(rq);
            let with_token = form_flag(&rq.form, "with_token");
            let site_kv = kv.get(KEY).and_then(Value::as_object);
            let imported = migrate::imported_of(&kv);
            let tokens = migrate::legacy_tokens(&kv, KEY);
            let empty = site_kv.map(|m| m.is_empty()).unwrap_or(true) && imported.is_empty();
            if empty {
                return fail(id, &t("data.export_empty"));
            }
            let doc = migrate::export(site_kv, &imported, &tokens, with_token);
            let text = serde_json::to_string_pretty(&doc).unwrap_or_default();
            let mut state = keep(rq.state.clone(), "kv", kv);
            state = keep(state, "text", json!(text));
            state = keep(state, "with_token", json!(with_token));
            state = keep(state, "step", json!("copy"));
            // 复制走 need 不走 op：op 的返回值会被丢掉，复制失败就没人知道了。
            need(
                id,
                state,
                vec![need_item("ui.copy", "copied", json!({ "text": text }))],
            )
        }
        _ => {
            let text = rq
                .state
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let with_token = migrate::truthy(rq.state.get("with_token"));
            let failed = got_err(&rq.provided, "copied");
            let status = match &failed {
                Some(err) => f("data.copy_fail", &[&truncate(err, 80)]),
                None if with_token => t("data.export_ok"),
                None => t("data.export_plain"),
            };
            let lamp = if failed.is_some() { "warn" } else { "ok" };
            let mut body = panel(&Site::of(&kv_pick(rq)), status.clone(), lamp);
            body["values"]["dump"] = json!(text);
            body["toast"] = json!({ "level": lamp, "message": truncate(&status, 200) });
            done(id, body)
        }
    }
}

/// 把导出的文件读回来。
///
/// 现有设置是用户此刻正在用的：整站表单只在原来是空的时候才让文件填，清单则按条目合并。
fn on_restore(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        _ => {
            let kv = kv_pick(rq);
            let text = fstr(&rq.form, "dump");
            if text.is_empty() {
                return fail(id, &t("data.restore_empty"));
            }
            let doc = match migrate::parse(&text) {
                Ok(doc) => doc,
                Err(bad) => {
                    let why = match bad {
                        migrate::Bad::Json => t("data.bad_json"),
                        migrate::Bad::Shape => t("data.bad_shape"),
                    };
                    return fail(id, &f("data.restore_bad", &[&why]));
                }
            };
            let existing = migrate::imported_of(&kv);
            let mut imported = existing.clone();
            let mut added = 0;
            for (name, record) in &doc.imported {
                if !existing.contains_key(name) {
                    added += 1;
                }
                imported.insert(name.clone(), record.clone());
            }
            let mut ops = Vec::new();
            if !imported.is_empty() {
                ops.push(json!({
                    "op": "kv.set",
                    "key": migrate::IMPORTED,
                    "value": Value::Object(imported.clone()),
                }));
            }
            let site = Site::of(&kv);
            let mut next = site.clone();
            let mut kept = false;
            if site.base.is_empty() {
                if let Some(section) = doc.site.as_ref() {
                    let restored = Site::of(&json!({ KEY: Value::Object(section.clone()) }));
                    ops.push(restored.save_op());
                    next = restored;
                }
            } else if doc.site.is_some() {
                kept = true;
            }
            for (name, token) in &doc.tokens {
                ops.push(json!({
                    "op": "kv.set",
                    "key": name,
                    "value": { "token": token },
                }));
            }
            let count = added.to_string();
            let mut status = f("data.restore_ok", &[&count]);
            if kept {
                status = format!("{}；{}", status, t("data.restore_site_kept"));
            }
            let mut body = panel(&next, status.clone(), "ok");
            body["toast"] = json!({ "level": "ok", "message": truncate(&status, 200) });
            done_ops(id, ops, body)
        }
    }
}

// ---------------- 站点上的文件 ----------------
//
// 列表、删除、上传都在这里。站点地址、认证头、表单字段名由 api.rs 定，宿主
// 不知道其中任何一个；它只按通用能力把请求发出去，或者把用户挑的那一个文件
// 的字节按 multipart 流出去。

/// 这次要动哪个站点、哪个目录。
struct Target {
    base: String,
    token: String,
    folder: String,
    storage: String,
    dedup: bool,
}

/// 从表单里定目标：点名一条迁移记录就动它，没点名就动当前表单配的站点。
///
/// `path` 永远压过记录里的目录——用户临时想删别的目录里的东西时，不该被迫
/// 先去改记录。
fn target_of(rq: &Rq) -> Result<Target, String> {
    let site = site_in_play(rq);
    let want = fstr(&rq.form, "remote");
    let path = fstr(&rq.form, "path");
    if want.is_empty() {
        if site.base.is_empty() {
            return Err(t("site.missing_base"));
        }
        if site.token.is_empty() {
            return Err(t("site.missing_token"));
        }
        return Ok(Target {
            folder: if path.is_empty() { site.folder } else { path },
            base: site.base,
            token: site.token,
            storage: site.storage,
            dedup: site.dedup,
        });
    }
    let kv = kv_pick(rq);
    let imported = migrate::imported_of(&kv);
    let record = imported
        .get(&want)
        .ok_or_else(|| f(&t("files.no_remote"), &[&want]))?;
    let found = migrate::from_value(record).ok_or_else(|| f(&t("files.bad_record"), &[&want]))?;
    let token = token_for(&kv, &want, &site);
    if token.is_empty() {
        return Err(f(&t("files.no_token"), &[&want]));
    }
    Ok(Target {
        folder: if path.is_empty() { found.folder } else { path },
        base: found.base,
        token,
        storage: found.storage,
        dedup: found.dedup,
    })
}

/// 这条记录的令牌在哪。
///
/// 先看「按远程名存的那一份」——用户当初就是为这个远程建的它，最贴近这个站点；
/// 找不到再退回站点表单里那一份。绝大多数人只有一个站点，两处其实同源。
fn token_for(kv: &Value, name: &str, site: &Site) -> String {
    let own = s(kv.get(name).and_then(|v| v.get("token")));
    if own.is_empty() {
        site.token.clone()
    } else {
        own
    }
}

fn target_state(state: Value, target: &Target) -> Value {
    keep(
        state,
        "target",
        json!({
            "base": target.base,
            "token": target.token,
            "folder": target.folder,
            "storage": target.storage,
            "dedup": target.dedup,
        }),
    )
}

fn target_from_state(state: &Value) -> Target {
    let v = state.get("target").cloned().unwrap_or(Value::Null);
    Target {
        base: s(v.get("base")),
        token: s(v.get("token")),
        folder: s(v.get("folder")),
        storage: s(v.get("storage")),
        dedup: v.get("dedup").and_then(Value::as_bool).unwrap_or(true),
    }
}

/// 把 `http.request` 回来的字符串体解析成 JSON。
fn body_json(body: &str) -> Result<Value, String> {
    serde_json::from_str::<Value>(body).map_err(|e| f(&t("files.bad_body"), &[&e.to_string()]))
}

/// 问宿主再要一页清单。
fn ask_page(id: u64, target: &Target, state: Value, cursor: &str) -> Value {
    need(
        id,
        state,
        vec![need_item(
            "http.request",
            "page",
            api::list_request(&target.base, &target.token, cursor),
        )],
    )
}

fn state_entries(state: &Value) -> Vec<api::Entry> {
    state
        .get("items")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(api::entry_of).collect())
        .unwrap_or_default()
}

fn finish_list(id: u64, rq: &Rq, items: &[api::Entry]) -> Value {
    let site = site_in_play(rq);
    let empty = items.is_empty();
    let status = if empty {
        t("files.empty")
    } else {
        f(&t("files.listed"), &[&items.len().to_string()])
    };
    let mut body = panel(&site, status.clone(), if empty { "warn" } else { "ok" });
    body["values"]["listing"] = json!(api::listing(items));
    body["toast"] = json!({ "level": "ok", "message": truncate(&status, 200) });
    done(id, body)
}

/// 列站点上的文件。
///
/// 站点一次最多给 200 条，翻页靠它给的 cursor；最多翻 `api::MAX_PAGES` 页，
/// 免得一个坏 cursor 让这次调用一直转下去。
fn on_list(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "kv" => {
            let target = match target_of(rq) {
                Ok(t) => t,
                Err(msg) => return fail(id, &msg),
            };
            let mut state = keep(rq.state.clone(), "kv", kv_pick(rq));
            state = target_state(state, &target);
            state = keep(state, "step", json!("page"));
            state = keep(state, "pages", json!(0));
            state = keep(state, "items", json!([]));
            ask_page(id, &target, state, "")
        }
        _ => {
            if let Some(e) = got_err(&rq.provided, "page") {
                return fail(id, &f(&t("files.list_fail"), &[&e]));
            }
            let res = got(&rq.provided, "page");
            let status = res.get("status").and_then(Value::as_u64).unwrap_or(0);
            let raw = s(res.get("body"));
            if status >= 400 {
                return fail(id, &site::api_error(status as u16, &raw));
            }
            let value = match body_json(&raw) {
                Ok(v) => v,
                Err(msg) => return fail(id, &msg),
            };
            let parsed = api::parse_files(&value);
            let complete = parsed.complete;
            let cursor = parsed.next.clone().unwrap_or_default();
            let mut items = state_entries(&rq.state);
            items.extend(parsed.entries);
            let pages = rq.state.get("pages").and_then(Value::as_u64).unwrap_or(0) + 1;
            if !complete && !cursor.is_empty() && pages < u64::from(api::MAX_PAGES) {
                let mut state = keep(rq.state.clone(), "pages", json!(pages));
                state = keep(
                    state,
                    "items",
                    Value::Array(items.iter().map(api::entry_to_value).collect()),
                );
                return ask_page(id, &target_from_state(&rq.state), state, &cursor);
            }
            finish_list(id, rq, &items)
        }
    }
}

/// 宿主问过「确定吗」，回的是 `{"ok": true/false}`。
///
/// 不把字段名写死成一种形状：缺字段当没确认——宁可少删一次，也别悄悄删掉。
fn confirmed(reply: &Value) -> bool {
    if let Some(b) = reply.as_bool() {
        return b;
    }
    for key in ["ok", "confirmed", "yes"] {
        if let Some(b) = reply.get(key).and_then(Value::as_bool) {
            return b;
        }
    }
    false
}

/// 删一个文件：先问一句，再按 id 删。
fn on_remove(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "kv" => {
            let target = match target_of(rq) {
                Ok(t) => t,
                Err(msg) => return fail(id, &msg),
            };
            let want = fstr(&rq.form, "id");
            if want.is_empty() {
                return fail(id, &t("files.need_id"));
            }
            let mut state = keep(rq.state.clone(), "kv", kv_pick(rq));
            state = target_state(state, &target);
            state = keep(state, "id", json!(want));
            state = keep(state, "step", json!("confirm"));
            need(
                id,
                state,
                vec![need_item(
                    "ui.confirm",
                    "yes",
                    json!({
                        "title": t("files.confirm_title"),
                        "message": f(&t("files.confirm"), &[&want]),
                    }),
                )],
            )
        }
        "confirm" => {
            if let Some(e) = got_err(&rq.provided, "yes") {
                return fail(id, &f(&t("files.confirm_fail"), &[&e]));
            }
            if !confirmed(&got(&rq.provided, "yes")) {
                let site = site_in_play(rq);
                return done(id, panel(&site, t("files.cancelled"), "warn"));
            }
            let target = target_from_state(&rq.state);
            let want = state_s(&rq.state, "id");
            let state = keep(rq.state.clone(), "step", json!("done"));
            need(
                id,
                state,
                vec![need_item(
                    "http.request",
                    "del",
                    api::delete_request(&target.base, &target.token, &want),
                )],
            )
        }
        _ => {
            if let Some(e) = got_err(&rq.provided, "del") {
                return fail(id, &f(&t("files.remove_fail"), &[&e]));
            }
            let res = got(&rq.provided, "del");
            let status = res.get("status").and_then(Value::as_u64).unwrap_or(0);
            if status >= 400 {
                return fail(id, &site::api_error(status as u16, &s(res.get("body"))));
            }
            let site = site_in_play(rq);
            let want = state_s(&rq.state, "id");
            let status_text = f(&t("files.removed"), &[&want]);
            let mut body = panel(&site, status_text.clone(), "ok");
            body["values"]["id"] = json!("");
            body["values"]["listing"] = json!("");
            body["toast"] = json!({ "level": "ok", "message": truncate(&status_text, 200) });
            done(id, body)
        }
    }
}

/// 从用户挑的路径里取文件名。
///
/// 宿主把「用户这次挑了哪个文件」记在自己那里，给到这里的只是一段文字。插件
/// 只用它决定上传后的名字——发给宿主的说明里没有路径这一项，所以即便这段文字
/// 是手打的，也读不到那个文件。
fn file_name_of(picked: &str) -> String {
    picked
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 把用户挑的那一个文件传到站点。
///
/// 字节由宿主流出去：插件给的是「面板里 `file` 那一栏」，不是路径。宿主要确认
/// 那一栏确实是用户自己挑的才肯发，手打出来的路径会被它挡掉。
fn on_upload(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "kv" => {
            let target = match target_of(rq) {
                Ok(t) => t,
                Err(msg) => return fail(id, &msg),
            };
            let picked = fstr(&rq.form, "file");
            let name = file_name_of(&picked);
            if name.is_empty() {
                return fail(id, &t("files.need_file"));
            }
            let mut state = keep(rq.state.clone(), "kv", kv_pick(rq));
            state = target_state(state, &target);
            state = keep(state, "name", json!(name));
            state = keep(state, "step", json!("up"));
            need(
                id,
                state,
                vec![need_item(
                    "http.upload",
                    "up",
                    api::upload_request(
                        &target.base,
                        &target.token,
                        &name,
                        &target.folder,
                        &target.storage,
                        target.dedup,
                    ),
                )],
            )
        }
        _ => {
            if let Some(e) = got_err(&rq.provided, "up") {
                // 宿主还没有这个能力时会走到这里。这不是网络问题，说清楚缺什么，
                // 别让人去查网络。
                let msg = if e.contains("http.upload") {
                    t("files.need_cap")
                } else {
                    f(&t("files.upload_fail"), &[&e])
                };
                return fail(id, &msg);
            }
            let res = got(&rq.provided, "up");
            let status = res.get("status").and_then(Value::as_u64).unwrap_or(0);
            let raw = s(res.get("body"));
            if status >= 400 {
                return fail(id, &site::api_error(status as u16, &raw));
            }
            let value = match body_json(&raw) {
                Ok(v) => v,
                Err(msg) => return fail(id, &msg),
            };
            let up = match api::parse_upload(&value) {
                Some(u) => u,
                None => return fail(id, &f(&t("files.bad_body"), &[&truncate(&raw, 120)])),
            };
            let size = res.get("size").and_then(Value::as_u64).unwrap_or(0);
            let mut status_text = f(&t("files.uploaded"), &[&up.name, &api::human_size(size)]);
            if up.deduped {
                status_text = format!("{}；{}", status_text, t("files.deduped"));
            }
            let site = site_in_play(rq);
            let target = target_from_state(&rq.state);
            // 站点有时只回一个相对路径（`/f/1`）。插件知道站点在哪，所以由插件拼成完整地址，
            // 宿主不需要理解任何站点细节。
            let link = api::absolutize(&target.base, &up.link);
            let mut body = panel(&site, status_text.clone(), "ok");
            if !link.is_empty() {
                body["values"]["link"] = json!(link);
            }
            body["toast"] = json!({ "level": "ok", "message": truncate(&status_text, 200) });
            done(id, body)
        }
    }
}

/// 把这次要发的请求整段列出来，一个字都不发出去。
///
/// 用户点了「看请求」就能核对：地址对不对、认证头怎么带、表单里有哪些字段。
/// 这也是宿主那边的实现者拿来对齐的清单。
fn on_plan(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "kv" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        _ => {
            let site = site_in_play(rq);
            let text = match target_of(rq) {
                Ok(target) => api::plan(
                    &target.base,
                    &target.token,
                    &target.folder,
                    &target.storage,
                    target.dedup,
                    &fstr(&rq.form, "id"),
                ),
                Err(msg) => format!("{}：{}", t("files.plan_partial"), msg),
            };
            let mut body = panel(&site, t("files.plan_ok"), "ok");
            body["values"]["trace"] = json!(text);
            done(id, body)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// 假宿主：只回答这个插件真的要的能力，别的一律报错——漏掉一个 need 会立刻暴露，
    /// 不会像真宿主那样把错误混进结果里让测试继续往下跑。
    struct Host {
        kv: Value,
        create_fails: Option<String>,
        calls: Vec<String>,
        /// `rclone.remotes` 的原始答复，两种形状都要能塞进来。
        remotes: Value,
        /// 远程名 → `config/get` 的答复（平铺参数，没有 params 包装）。
        configs: Map<String, Value>,
        /// 混淆串 → `config/reveal` 换回来的明文；查不到就原样返回，和真宿主一致。
        revealed: Map<String, Value>,
        reveal_fails: bool,
        copy_fails: Option<String>,
        /// `http.request` 的答复队列，按调用顺序发放；用完之后一律回 `{}` 200。
        replies: VecDeque<Value>,
        /// 插件到底要求宿主发了什么：`(need 的 key, 参数)`。测试靠它检查请求的形状，
        /// 而不是靠插件自己的说明文字——说明写错了也得被抓住。
        sent: Vec<(String, Value)>,
        /// `http.upload` 的答复。默认成功。
        upload: Value,
        /// 让 `http.upload` 回 `_error`，模拟宿主没实现或拒绝授权。
        upload_fails: Option<String>,
        /// `ui.confirm` 的回答。
        says_yes: bool,
    }

    impl Host {
        fn new() -> Host {
            Host {
                kv: json!({}),
                create_fails: None,
                calls: Vec::new(),
                remotes: json!({ "remotes": [] }),
                configs: Map::new(),
                revealed: Map::new(),
                reveal_fails: false,
                copy_fails: None,
                replies: VecDeque::new(),
                sent: Vec::new(),
                upload: json!({
                    "status": 200,
                    "size": 7,
                    "body": r#"{"success":true,"file":{"name":"a b.png"},"links":{"download":"/f/1"}}"#,
                }),
                upload_fails: None,
                says_yes: true,
            }
        }

        /// 造一个「旧版主程序建的」K-Vault 远程：webdav + kvault 标记 + 混淆过的 bearer。
        fn legacy_remote(&mut self, name: &str, base: &str, bearer: &str) {
            self.configs.insert(
                name.to_string(),
                json!({
                    "type": "webdav",
                    "url": format!("{base}/dav"),
                    "vendor": "other",
                    "kvault": "1",
                    "kvault_folder": "album",
                    "kvault_storage": "r2",
                    "kvault_dedup": "0",
                    "bearer_token": bearer,
                }),
            );
        }

        fn answer(&mut self, cap: &str, key: &str, params: &Value) -> Value {
            self.calls.push(format!("{cap}:{key}"));
            match cap {
                "kv.get_all" => self.kv.clone(),
                "rclone.remotes" => self.remotes.clone(),
                "ui.copy" => match &self.copy_fails {
                    Some(e) => json!({ "_error": e }),
                    None => json!({}),
                },
                "rclone.call" => {
                    let method = s(params.get("method"));
                    // 真的 `rclone.call` 把方法参数放在 `params` 里：`{"method":…,"params":{…}}`。
                    // 早先这里少读一层，`config/get` 永远查不到名字，测试就会凭空少一条记录。
                    let inner = params.get("params").cloned().unwrap_or(Value::Null);
                    self.calls.push(method.clone());
                    match method.as_str() {
                        "config/providers" => json!({
                            "providers": [ { "Name": "webdav" }, { "Name": "s3" } ]
                        }),
                        "config/get" => {
                            let name = s(inner.get("name"));
                            match self.configs.get(&name) {
                                Some(v) => v.clone(),
                                None => json!({ "_error": format!("没有这个远程: {name}") }),
                            }
                        }
                        "config/reveal" => {
                            if self.reveal_fails {
                                json!({ "_error": "rclone 不肯解密" })
                            } else {
                                let raw = s(inner.get("value"));
                                let plain = self.revealed.get(&raw).cloned().unwrap_or(json!(raw));
                                json!({ "value": plain })
                            }
                        }
                        "config/listremotes" => json!({ "remotes": [] }),
                        "config/create" => match &self.create_fails {
                            Some(e) => json!({ "_error": e }),
                            None => json!({}),
                        },
                        other => json!({ "_error": format!("测试里没有这个方法: {other}") }),
                    }
                }
                // 通用 HTTP：插件给什么就发什么，宿主自己不知道这是哪个站点。
                // 这里只是把"要发的东西"原样记下来，测试据此核对请求形状。
                "http.request" => {
                    self.sent.push((key.to_string(), params.clone()));
                    self.replies
                        .pop_front()
                        .unwrap_or_else(|| json!({ "status": 200, "body": "{}" }))
                }
                "http.upload" => {
                    self.sent.push((key.to_string(), params.clone()));
                    match &self.upload_fails {
                        Some(e) => json!({ "_error": e }),
                        None => self.upload.clone(),
                    }
                }
                "ui.confirm" => json!({ "ok": self.says_yes }),
                other => json!({ "_error": format!("测试里没有这个能力: {other}") }),
            }
        }
    }

    /// 一次动作写下的东西。`done_ops` 一律带 `"ops"` 字段（可能是空数组），
    /// 所以"没写任何东西"要判空数组，不能判 `keys` 里有没有 `ops`。
    fn ops_of(out: &Value) -> Vec<Value> {
        out.get("ops")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
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

    /// 扫描只读：把带 kvault 标记的远程记成清单，一个字节都不写回 rclone。
    /// 这样旧版主程序随时能退回，用户不会因为「装了个插件」丢掉能用的远程。
    #[test]
    fn migration_records_marked_remotes_and_never_writes_rclone() {
        let mut host = Host::new();
        host.kv = json!({
            "site": { "base": "https://mine.example.com", "token": "tok-123" },
            "old-vault": { "token": "kv-old" },
        });
        host.remotes = json!({ "remotes": ["old-vault", "gdrive"] });
        host.legacy_remote("old-vault", "https://old.example.com", "obscured-1");
        host.configs
            .insert("gdrive".into(), json!({ "type": "drive" }));

        let (out, rounds) = drive(&mut host, "migrate", &json!({}));

        let ops = out
            .get("ops")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(
            !ops.iter().any(|op| s(op.get("key")) == KEY),
            "站点已经填好了，迁移不该再写它：{ops:?}"
        );
        assert!(
            !host.calls.iter().any(|c| c.starts_with("config/update")
                || c == "config/create"
                || c == "config/delete"),
            "迁移不许改 rclone 配置：{:?}",
            host.calls
        );

        let imported = ops
            .iter()
            .find(|op| s(op.get("key")) == migrate::IMPORTED)
            .expect("缺清单");
        let record = imported
            .get("value")
            .and_then(|v| v.get("old-vault"))
            .expect("清单里没有 old-vault");
        assert_eq!(s(record.get("base")), "https://old.example.com");
        assert_eq!(s(record.get("folder")), "album");
        assert_eq!(s(record.get("storage")), "r2");
        assert_eq!(record.get("dedup"), Some(&json!(false)));
        assert_eq!(
            s(record.get("token_source")),
            "kv",
            "kv 里有明文副本时就记成 kv，不用去解混淆串"
        );
        assert!(
            record.get("token").is_none() && record.get("bearer").is_none(),
            "清单里不许带令牌：{record:?}"
        );
        assert!(
            imported
                .get("value")
                .and_then(|v| v.get("gdrive"))
                .is_none(),
            "没有 kvault 标记的远程不算 K-Vault"
        );

        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        assert_eq!(
            body["values"]["pick"],
            json!("old-vault"),
            "扫描完该把第一条填进记录名"
        );
        assert!(
            rounds.len() <= 5,
            "远程再多也不该多跑几轮：{}",
            rounds.len()
        );
    }

    /// 站点设置是空的、旧版又只留了混淆串时，才去解一次混淆串把它填上。
    #[test]
    fn migration_seeds_an_empty_site_from_the_revealed_token() {
        let mut host = Host::new();
        host.remotes = json!(["v1"]);
        host.legacy_remote("v1", "https://old.example.com", "obscured-1");
        host.revealed.insert("obscured-1".into(), json!("kv-live"));

        let (out, _) = drive(&mut host, "migrate", &json!({}));

        let ops = out.get("ops").and_then(Value::as_array).unwrap();
        let site = ops
            .iter()
            .find(|op| s(op.get("key")) == KEY)
            .expect("站点是空的，该被填上");
        assert_eq!(site["value"]["base"], json!("https://old.example.com"));
        assert_eq!(
            site["value"]["token"],
            json!("kv-live"),
            "用的是 reveal 解出来的明文，不是混淆串"
        );
        let imported = ops
            .iter()
            .find(|op| s(op.get("key")) == migrate::IMPORTED)
            .unwrap();
        assert_eq!(s(imported["value"]["v1"].get("token_source")), "rclone");
    }

    /// 一个 K-Vault 远程都没有时什么都不写，并说清楚为什么。
    #[test]
    fn a_migration_that_finds_nothing_writes_nothing() {
        let mut host = Host::new();
        host.remotes = json!({ "remotes": ["gdrive"] });
        host.configs
            .insert("gdrive".into(), json!({ "type": "drive" }));

        let (out, _) = drive(&mut host, "migrate", &json!({}));

        let ops = out
            .get("ops")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(ops.is_empty(), "没找到东西就不该写 kv：{ops:?}");
        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        assert_eq!(body.get("lamp").and_then(Value::as_str), Some("warn"));
    }

    /// 单条远程读不到时只记一条备注，不许把整次扫描变成报错。
    #[test]
    fn one_unreadable_remote_becomes_a_note() {
        let mut host = Host::new();
        host.remotes = json!({ "remotes": ["broken", "old-vault"] });
        host.legacy_remote("old-vault", "https://old.example.com", "obscured-1");
        host.kv = json!({ "site": { "base": "https://mine.example.com", "token": "tok-123" } });

        let (out, _) = drive(&mut host, "migrate", &json!({}));

        let ops = out.get("ops").and_then(Value::as_array).unwrap();
        let imported = ops
            .iter()
            .find(|op| s(op.get("key")) == migrate::IMPORTED)
            .expect("好的那个还是要进清单");
        assert!(imported["value"].get("old-vault").is_some());
        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        assert!(
            s(body.get("status")).contains("broken"),
            "备注里要点名是哪个远程：{}",
            s(body.get("status"))
        );
    }

    /// 用清单里的一条填表单：只改表单，不落盘。
    #[test]
    fn adopt_fills_the_form_and_writes_nothing() {
        let mut host = Host::new();
        host.kv = json!({
            "imported": { "old-vault": {
                "name": "old-vault",
                "base": "https://old.example.com",
                "folder": "album",
                "storage": "r2",
                "dedup": false,
                "token_source": "kv"
            }},
            "old-vault": { "token": "kv-old" },
        });

        let (out, _) = drive(&mut host, "adopt", &json!({ "pick": "old-vault" }));

        assert!(ops_of(&out).is_empty(), "填表单不该写任何东西");
        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        assert_eq!(body["values"]["base"], json!("https://old.example.com"));
        assert_eq!(
            body["values"]["token"],
            json!("kv-old"),
            "令牌要一起带进表单"
        );
        assert_eq!(body["values"]["folder"], json!("album"));
        assert_eq!(body["values"]["storage"], json!("r2"));
        assert_eq!(body["values"]["dedup"], json!("false"));
        assert_eq!(
            body.get("lamp").and_then(Value::as_str),
            Some("warn"),
            "还没测过，不该报绿灯"
        );
    }

    #[test]
    fn adopt_refuses_a_name_that_is_not_in_the_inventory() {
        let mut host = Host::new();
        let (out, _) = drive(&mut host, "adopt", &json!({ "pick": "nope" }));
        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        assert_eq!(body.get("lamp").and_then(Value::as_str), Some("err"));
        assert!(ops_of(&out).is_empty());
    }

    /// 默认导出不带令牌，但把同一份文件恢复回去仍能还原清单与站点。
    #[test]
    fn export_hides_tokens_by_default_and_restore_merges_it_back() {
        let mut host = Host::new();
        host.kv = json!({
            "site": {
                "base": "https://mine.example.com",
                "token": "tok-123",
                "folder": "album",
                "name": "my-vault",
                "storage": "r2",
                "dedup": true
            },
            "imported": { "old-vault": {
                "name": "old-vault",
                "base": "https://old.example.com",
                "folder": "album",
                "storage": "r2",
                "dedup": false,
                "token_source": "kv"
            }},
            "old-vault": { "token": "kv-old" },
        });

        let (out, _) = drive(&mut host, "export", &json!({}));
        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        let text = s(body["values"].get("dump"));
        assert!(
            text.contains("\"plugin\": \"kvault\""),
            "导出的该是一份能读的 JSON：{text}"
        );
        assert!(
            !text.contains("tok-123") && !text.contains("kv-old"),
            "默认不该带令牌：{text}"
        );

        let mut fresh = Host::new();
        let (out2, _) = drive(&mut fresh, "restore", &json!({ "dump": text }));
        let ops = out2.get("ops").and_then(Value::as_array).unwrap();
        let imported = ops
            .iter()
            .find(|op| s(op.get("key")) == migrate::IMPORTED)
            .expect("缺清单");
        assert!(imported["value"].get("old-vault").is_some());
        let site = ops
            .iter()
            .find(|op| s(op.get("key")) == KEY)
            .expect("空插件的站点该被填上");
        assert_eq!(s(site["value"].get("base")), "https://mine.example.com");
        assert_eq!(
            s(site["value"].get("token")),
            "",
            "导出的令牌是空的，恢复出来也只能是空的"
        );
    }

    /// 带令牌导出后恢复：令牌写回按远程名的那条，正在用的站点绝不被覆盖。
    #[test]
    fn restore_keeps_the_live_site_and_writes_tokens_back() {
        let mut host = Host::new();
        host.kv = json!({
            "site": { "base": "https://mine.example.com", "token": "tok-123" },
            "old-vault": { "token": "kv-old" },
        });
        let (out, _) = drive(&mut host, "export", &json!({ "with_token": true }));
        let text = s(out["result"]["body"]["values"].get("dump"));
        assert!(text.contains("tok-123"), "勾了带令牌就该带上：{text}");

        let mut live = Host::new();
        live.kv = json!({ "site": { "base": "https://live.example.com", "token": "tok-live" } });
        let (out2, _) = drive(&mut live, "restore", &json!({ "dump": text }));

        let ops = out2.get("ops").and_then(Value::as_array).unwrap();
        assert!(
            !ops.iter().any(|op| s(op.get("key")) == KEY),
            "站点正在用，恢复不该覆盖它：{ops:?}"
        );
        let token = ops
            .iter()
            .find(|op| s(op.get("key")) == "old-vault")
            .expect("该把令牌写回去");
        assert_eq!(token["value"]["token"], json!("kv-old"));
        let body = out2.get("result").and_then(|r| r.get("body")).unwrap();
        assert!(
            s(body.get("status")).contains("保留"),
            "状态里要说明站点没动：{}",
            s(body.get("status"))
        );
        assert_eq!(body["values"]["base"], json!("https://live.example.com"));
    }

    /// 别人家的 JSON、或者半截文件，都得当场拒掉，不能写坏现有设置。
    #[test]
    fn restore_refuses_something_that_is_not_an_export() {
        let mut host = Host::new();
        for dump in [
            "{\"plugin\":\"other\",\"format\":1}",
            "{\"plugin\":\"kvault\",\"format\":99}",
            "not json at all",
            "",
        ] {
            let (out, _) = drive(&mut host, "restore", &json!({ "dump": dump }));
            let body = out.get("result").and_then(|r| r.get("body")).unwrap();
            assert_eq!(
                body.get("lamp").and_then(Value::as_str),
                Some("err"),
                "这份该被拒：{dump}"
            );
            assert!(
                out.get("ops")
                    .and_then(Value::as_array)
                    .map_or(true, |o| o.is_empty()),
                "拒绝时不该写任何东西：{dump}"
            );
        }
    }

    /// 导出时复制失败要看得见，而不是悄悄成功。
    #[test]
    fn a_failed_copy_is_reported() {
        let mut host = Host::new();
        host.kv = json!({ "site": { "base": "https://mine.example.com", "token": "tok-123" } });
        host.copy_fails = Some("剪贴板被占用".to_string());

        let (out, _) = drive(&mut host, "export", &json!({}));

        let body = out.get("result").and_then(|r| r.get("body")).unwrap();
        assert!(
            s(body.get("status")).contains("剪贴板被占用"),
            "复制失败要在状态里写清楚：{}",
            s(body.get("status"))
        );
        assert!(
            !s(body["values"].get("dump")).is_empty(),
            "复制失败也要把内容留在框里，用户能自己抄"
        );
    }

    // ---- 站点上的文件：列表、删除、上传、请求说明 ----

    fn body_of(out: &Value) -> Value {
        out.get("result")
            .and_then(|r| r.get("body"))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn reply_pages(host: &mut Host, pages: &[&str]) {
        for page in pages {
            host.replies
                .push_back(json!({ "status": 200, "body": page }));
        }
    }

    /// 列表要按站点说的分页走完，并且把每一条的 id 留下来——删除只认 id。
    #[test]
    fn listing_walks_pages_until_the_site_says_it_is_done() {
        let mut host = Host::new();
        reply_pages(
            &mut host,
            &[
                r#"{"success":true,"data":{"files":[
                    {"id":"r2:album/a.txt","name":"a.txt","folderPath":"album/","size":10,"uploadedAt":"2026-01-01T00:00:00Z"},
                    {"id":"tg:AgACAg","name":"b.png","folderPath":"","size":2048}],
                    "pagination":{"listComplete":false,"cursor":"c2"}}}"#,
                r#"{"files":[{"id":"r2:album/sub/c.zip","name":"c.zip","folderPath":"album/sub","size":3145728}],
                    "pagination":{"listComplete":true}}"#,
            ],
        );

        let (out, _) = drive(&mut host, "list", &filled());

        let body = body_of(&out);
        let listing = s(body["values"].get("listing"));
        for id in ["r2:album/a.txt", "tg:AgACAg", "r2:album/sub/c.zip"] {
            assert!(listing.contains(id), "列表里少了 {id}：\n{listing}");
        }
        assert!(
            listing.contains("album/sub/c.zip"),
            "要按路径显示：\n{listing}"
        );
        assert_eq!(host.sent.len(), 2, "两页就得发两次请求");
        assert_eq!(s(host.sent[0].1.get("method")), "GET");
        let second = s(host.sent[1].1.get("url"));
        assert!(second.contains("cursor=c2"), "第二页要带上游标：{second}");
        assert!(second.contains("limit=200"), "分页大小由插件定：{second}");
        assert!(ops_of(&out).is_empty(), "看列表不写任何东西");
    }

    /// 站点报错时，用户要看到站点给的原话，而不是"列表失败"四个字。
    #[test]
    fn a_site_error_keeps_the_sites_own_words() {
        let mut host = Host::new();
        // 第一页就是 401：站点把错误码和原话放在 body 里。
        host.replies.push_back(json!({
            "status": 401,
            "body": r#"{"error":{"code":"TOKEN_INVALID","message":"令牌已失效"}}"#,
        }));

        let (out, _) = drive(&mut host, "list", &filled());

        let status = s(body_of(&out).get("status"));
        assert!(status.contains("401"), "要带上状态码：{status}");
        assert!(
            status.contains("TOKEN_INVALID"),
            "要带上站点错误码：{status}"
        );
        assert!(status.contains("令牌已失效"), "要带上站点原话：{status}");
    }

    /// 删除是两步：先问人，再发 DELETE；id 里的空格要编码，`:` 要留着。
    #[test]
    fn deleting_asks_first_then_sends_the_encoded_id() {
        let mut host = Host::new();
        let mut form = filled();
        form["id"] = json!("tg:abc def");

        let (out, _) = drive(&mut host, "remove", &form);

        assert_eq!(host.sent.len(), 1, "只该发一次 DELETE");
        let (key, params) = &host.sent[0];
        assert_eq!(key, "del");
        assert_eq!(s(params.get("method")), "DELETE");
        let url = s(params.get("url"));
        assert!(
            url.contains("tg:abc%20def"),
            "空格要编码、冒号要留着：{url}"
        );
        assert!(
            !url.contains("tg:abc def"),
            "原样发出去会被站点当两个参数：{url}"
        );
        let auth = s(params["headers"].get("Authorization"));
        assert!(auth.starts_with("Bearer "), "认证头由插件给：{auth}");
        assert!(ops_of(&out).is_empty(), "删站点上的文件不动本地 kv");
    }

    /// 人说不删，就一个字节都不发。宁可少删一次，不可误删。
    #[test]
    fn deleting_nothing_when_the_user_says_no() {
        let mut host = Host::new();
        host.says_yes = false;
        let mut form = filled();
        form["id"] = json!("r2:album/a.txt");

        let (out, _) = drive(&mut host, "remove", &form);

        assert!(host.sent.is_empty(), "用户没同意就不该发请求");
        let body = body_of(&out);
        assert_eq!(s(body.get("lamp")), "warn");
        let status = s(body.get("status"));
        assert!(status.contains("没有删"), "取消要明说没删：{status}");
    }

    /// 上传这一步只说"用户选了哪个框里的文件"，绝不给宿主一个路径。
    #[test]
    fn upload_names_the_picked_field_and_never_a_path() {
        let mut host = Host::new();
        let mut form = filled();
        form["file"] = json!("C:\\pics\\a b.png");

        let (out, _) = drive(&mut host, "upload", &form);

        assert_eq!(host.sent.len(), 1);
        let (key, params) = &host.sent[0];
        assert_eq!(key, "up");
        assert_eq!(
            s(params["source"].get("form_key")),
            "file",
            "只给表单里的字段名"
        );
        assert!(
            params.get("source").and_then(|v| v.get("path")).is_none(),
            "插件不该知道也不必给出本地路径：{params}"
        );
        assert_eq!(s(params["file"].get("name")), "a b.png", "文件名要去掉目录");
        assert_eq!(
            s(params["file"].get("field")),
            "file",
            "multipart 的字段名由插件定"
        );
        let fields = params["fields"].as_array().cloned().unwrap_or_default();
        assert!(
            fields.iter().any(|f| s(f.get("name")) == "storage"),
            "存储位置是必须带的字段：{params}"
        );
        assert!(
            params.get("idem_seed").is_some(),
            "重试去重靠这个种子，宿主再拼上字节摘要：{params}"
        );
        let body = body_of(&out);
        assert_eq!(s(body.get("lamp")), "ok");
        assert_eq!(
            s(body["values"].get("link")),
            "https://vault.example.com/f/1",
            "站点给的链接要显示出来"
        );
    }

    /// 宿主拒绝授权时说清楚是谁拒绝的，不能假装上传成功。
    #[test]
    fn a_refused_upload_is_reported_as_refused() {
        let mut host = Host::new();
        host.upload_fails = Some("SOURCE_NOT_GRANTED: 这个字段没有被授权".to_string());
        let mut form = filled();
        form["file"] = json!("C:\\pics\\a.png");

        let (out, _) = drive(&mut host, "upload", &form);

        let body = body_of(&out);
        assert_eq!(s(body.get("lamp")), "err");
        assert!(
            s(body.get("status")).contains("SOURCE_NOT_GRANTED"),
            "宿主原话要传出来：{}",
            s(body.get("status"))
        );
        assert!(
            s(body["values"].get("link")).is_empty(),
            "失败时不能给出链接"
        );
    }

    /// 宿主还没有那个能力时，要指名缺的是哪一个，而不是笼统报错。
    #[test]
    fn a_host_without_the_upload_capability_names_it() {
        let mut host = Host::new();
        host.upload_fails = Some("未知能力: http.upload".to_string());
        let mut form = filled();
        form["file"] = json!("C:\\pics\\a.png");

        let (out, _) = drive(&mut host, "upload", &form);

        let body = body_of(&out);
        let status = s(body.get("status"));
        assert!(
            status.contains("http.upload"),
            "缺哪个能力就说哪个：{status}"
        );
        assert!(
            status.contains("HOST-CONTRACT.md"),
            "要告诉人补在哪份文档的哪一节：{status}"
        );
        assert_eq!(s(body.get("lamp")), "err");
        assert!(
            s(body["values"].get("link")).is_empty(),
            "没发出去就不能给出链接"
        );
        assert_eq!(host.sent.len(), 1, "只该有那一次尝试，不该重试");
    }

    /// 请求说明只是给人看的，一个请求都不发，令牌也不露全文。
    #[test]
    fn the_request_plan_sends_nothing_and_hides_the_token() {
        let mut host = Host::new();
        let mut form = filled();
        form["id"] = json!("r2:album/a.txt");

        let (out, _) = drive(&mut host, "plan", &form);

        assert!(host.sent.is_empty(), "说明阶段不发网络请求");
        let trace = s(body_of(&out)["values"].get("trace"));
        assert!(trace.contains("/api/v1/files"), "要写出列表地址：\n{trace}");
        assert!(trace.contains("DELETE"), "要写出删除方法：\n{trace}");
        assert!(
            trace.contains("/api/v1/upload"),
            "要写出上传地址：\n{trace}"
        );
        assert!(!trace.contains("tok-123"), "令牌只能露前几位：\n{trace}");
        assert!(trace.contains("tok-"), "还得让人能对上号：\n{trace}");
    }
}
