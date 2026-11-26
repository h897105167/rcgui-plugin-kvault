// 宿主交给插件做的三件事：列目录、上传、删除，外加「接管旧设置」。
//
// 宿主只知道「这条远程由哪个插件负责、它声明了哪些操作」，不知道站点长什么样：
// 地址怎么拼、令牌放哪个头、目录怎么从索引里切出来、同名文件怎么区分，全在这一层。
// 因此这里收到的都是抽象说法（远程名、目录、文件名、要删的名字），返回的也是抽象说法。
//
// 每一次 `remote.*` 调用都是一小段轮次：先取插件的 kv（面板站点与令牌都在里面），
// 不够再问 rclone 要这条远程的参数，参数里只有密文时才求一次明文。
// 拿到「站点 + 令牌 + 目录」之后才发真正的站点请求。

use serde_json::{json, Map, Value};

use crate::api;
use crate::i18n::{f, t};
use crate::migrate;
use crate::proto::*;
use crate::site;

/// 宿主要的是这条远程的「站点、令牌、目录、存储、去重」。
#[derive(Clone, Debug, Default)]
struct Plan {
    base: String,
    token: String,
    folder: String,
    storage: String,
    dedup: bool,
}

impl Plan {
    fn to_value(&self) -> Value {
        json!({
            "base": self.base,
            "token": self.token,
            "folder": self.folder,
            "storage": self.storage,
            "dedup": self.dedup,
        })
    }

    fn of_state(state: &Value) -> Plan {
        let plan = state.get("plan").cloned().unwrap_or(Value::Null);
        Plan {
            base: s(plan.get("base")).trim().to_string(),
            token: s(plan.get("token")).trim().to_string(),
            folder: s(plan.get("folder")).trim().trim_matches('/').to_string(),
            storage: s(plan.get("storage")).trim().to_string(),
            dedup: migrate::truthy(plan.get("dedup")),
        }
    }

    /// 界面要的目录：给了路径就用路径，没给就用这条远程自己的上传目录。
    fn dir(&self, path: &str) -> String {
        let path = path.trim().trim_matches('/');
        if path.is_empty() {
            self.folder.clone()
        } else {
            path.to_string()
        }
    }
}

/// 一轮的产出：要么「再问一次宿主」，要么「站点信息齐了，可以发请求」。
enum Step {
    Reply(Value),
    Ready(Plan),
}

/// 一轮最多同时发多少个删除请求。
///
/// 一次删几百条要和站点来回几百趟，而宿主一轮只等两分钟；分批既不超时，
/// 也能在某一批失败时把已经删掉的名报回去。
const DEL_BATCH: usize = 40;

/// 一次操作最多删多少项。超过就明说没做完——静默截断会让用户以为都删了。
const DEL_LIMIT: usize = 200;

const STEP_KV: &str = "kv";
const STEP_CFG: &str = "cfg";
const STEP_REVEAL: &str = "rvl";
const STEP_PAGE: &str = "page";
const STEP_UP: &str = "up";
const STEP_DEL: &str = "del";

/// 插件自己的整站设置里，令牌存在哪一格（和面板看到的是同一个字段）。
const SITE_KEY: &str = crate::KEY;

// ---------------------------------------------------------------- 站点信息

/// 把「远程名 → 站点 + 令牌」这件事走完。
///
/// 顺利时第一轮就问宿主，最后一轮返回 `Ready`；任何一步缺东西都直接失败，
/// 让用户看见缺的是什么，而不是拿空地址去请求、然后报一个看不懂的 404。
fn resolve(id: u64, rq: &Rq) -> Step {
    match state_s(&rq.state, "step").as_str() {
        "" => Step::Reply(need(
            id,
            keep(rq.state.clone(), "step", json!(STEP_KV)),
            vec![need_item("kv.get_all", "kv", json!({}))],
        )),
        STEP_KV => {
            let kv = got(&rq.provided, "kv");
            let name = arg(rq, "remote");
            if name.is_empty() {
                return match plan_of_site(&kv) {
                    Ok(plan) => Step::Ready(plan),
                    Err(message) => Step::Reply(fail(id, &message)),
                };
            }
            let state = keep(rq.state.clone(), "kv", kv);
            Step::Reply(need(
                id,
                state,
                vec![need_item(
                    "rclone.call",
                    "cfg",
                    json!({ "method": "config/get", "params": { "name": name } }),
                )],
            ))
        }
        STEP_CFG => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let name = arg(rq, "remote");
            let live = got(&rq.provided, "cfg");
            match plan_of_remote(&name, &kv, &live) {
                Ok((plan, None)) => Step::Ready(plan),
                Ok((plan, Some(secret))) => {
                    let state = keep(rq.state.clone(), "plan", plan.to_value());
                    Step::Reply(need(
                        id,
                        state,
                        vec![need_item(
                            "rclone.call",
                            "rvl",
                            json!({ "method": "config/reveal", "params": { "value": secret } }),
                        )],
                    ))
                }
                Err(message) => Step::Reply(fail(id, &message)),
            }
        }
        STEP_REVEAL => {
            let mut plan = Plan::of_state(&rq.state);
            let shown = s(got(&rq.provided, "rvl").get("value")).trim().to_string();
            // 解不出来就用原文：一个错令牌会得到 401，比悄悄不带令牌好查。
            if !shown.is_empty() {
                plan.token = shown;
            }
            Step::Ready(plan)
        }
        _ => Step::Ready(Plan::of_state(&rq.state)),
    }
}

/// 面板上填的那套站点（用户没指定远程时用）。
fn plan_of_site(kv: &Value) -> Result<Plan, String> {
    let site_cfg = kv.get(SITE_KEY).cloned().unwrap_or(Value::Null);
    let base = site::base_of(&s(site_cfg.get("base")));
    if base.is_empty() {
        return Err(t("site.missing_base").to_string());
    }
    let token = s(site_cfg.get("token")).trim().to_string();
    if token.is_empty() {
        return Err(t("site.missing_token").to_string());
    }
    Ok(Plan {
        base,
        token,
        folder: s(site_cfg.get("folder"))
            .trim()
            .trim_matches('/')
            .to_string(),
        storage: storage_of(&s(site_cfg.get("storage"))),
        dedup: match site_cfg.get("dedup") {
            Some(value) => migrate::truthy(Some(value)),
            None => true,
        },
    })
}

/// 指定了远程名时用：优先 rclone 里的实时参数，其次插件记着的清单记录。
///
/// 令牌不在 rclone 参数里明文出现（只有一个加密值），所以要把它还给宿主去解一次；
/// 插件自己拿不到密钥，也不该拿到。
fn plan_of_remote(name: &str, kv: &Value, live: &Value) -> Result<(Plan, Option<String>), String> {
    // `config/get` 的答复就是参数本身；万一哪天套了一层 `params` 也认。
    let live = live
        .get("params")
        .filter(|inner| inner.is_object())
        .unwrap_or(live)
        .clone();
    let live = if live.is_object() { Some(live) } else { None };
    let live_found = live.as_ref().and_then(|p| migrate::found_of(name, p));
    let record = migrate::imported_of(kv)
        .get(name)
        .and_then(migrate::from_value);

    let base = live_found
        .as_ref()
        .map(|f| f.base.clone())
        .or_else(|| record.as_ref().map(|f| f.base.clone()))
        .unwrap_or_default();
    if base.is_empty() {
        return Err(f(&t("remote.no_base"), &[name]));
    }
    let folder = live_found
        .as_ref()
        .map(|f| f.folder.clone())
        .or_else(|| record.as_ref().map(|f| f.folder.clone()))
        .unwrap_or_default();
    let storage = storage_of(
        &live_found
            .as_ref()
            .map(|f| f.storage.clone())
            .or_else(|| record.as_ref().map(|f| f.storage.clone()))
            .unwrap_or_default(),
    );
    let dedup = live_found
        .as_ref()
        .or(record.as_ref())
        .map(|f| f.dedup)
        .unwrap_or(true);
    let plan = |token: String| Plan {
        base: base.clone(),
        token,
        folder: folder.trim().trim_matches('/').to_string(),
        storage: storage.clone(),
        dedup,
    };

    // 令牌顺序：这条远程自己的 → 整站设置里的（用户通常只填一个）→ rclone 里的密文。
    let own = s(kv.get(name).and_then(|v| v.get("token")))
        .trim()
        .to_string();
    if !own.is_empty() {
        return Ok((plan(own), None));
    }
    let shared = s(kv.get(SITE_KEY).and_then(|v| v.get("token")))
        .trim()
        .to_string();
    if !shared.is_empty() {
        return Ok((plan(shared), None));
    }
    let sealed = live_found
        .as_ref()
        .map(|f| f.bearer.trim().to_string())
        .unwrap_or_default();
    if !sealed.is_empty() {
        return Ok((plan(String::new()), Some(sealed)));
    }
    Err(f(&t("remote.no_token"), &[name]))
}

fn storage_of(raw: &str) -> String {
    let raw = raw.trim().to_ascii_lowercase();
    if raw.is_empty() {
        site::DEFAULT_STORAGE.to_string()
    } else {
        raw
    }
}

// ---------------------------------------------------------------- 列目录

pub fn on_remote_list(id: u64, rq: &Rq) -> Value {
    let plan = match resolve(id, rq) {
        Step::Reply(reply) => return reply,
        Step::Ready(plan) => plan,
    };
    if state_s(&rq.state, "step") != STEP_PAGE {
        let state = page_state(rq.state.clone(), &plan, 0, &[]);
        return ask_page(id, state, &plan, "");
    }
    let page = match read_page(&rq.provided, "page") {
        Ok(page) => page,
        Err(message) => return fail(id, &message),
    };
    let mut entries = entries_of(&rq.state);
    entries.extend(page.entries);
    let pages = rq.state.get("pages").and_then(Value::as_u64).unwrap_or(0) + 1;
    let next = page.next.unwrap_or_default();
    if !page.complete && !next.is_empty() && pages < u64::from(api::MAX_PAGES) {
        let state = page_state(rq.state.clone(), &plan, pages, &entries);
        return ask_page(id, state, &plan, &next);
    }
    let dir = plan.dir(&arg(rq, "path"));
    let sort_key = sort_of(rq);
    let rows = list_folder(&entries, dir.as_str(), &sort_key, arg_flag(rq, "desc"));
    done(id, json!({ "items": rows }))
}

/// 拼「再要一页」的请求。游标原样带回去，站点靠它接着往下给。
fn ask_page(id: u64, state: Value, plan: &Plan, cursor: &str) -> Value {
    need(
        id,
        state,
        vec![need_item(
            "http.request",
            "page",
            api::list_request(&plan.base, &plan.token, cursor),
        )],
    )
}

fn page_state(state: Value, plan: &Plan, pages: u64, entries: &[api::Entry]) -> Value {
    let items: Vec<Value> = entries.iter().map(api::entry_to_value).collect();
    let state = keep(state, "plan", plan.to_value());
    let state = keep(state, "step", json!(STEP_PAGE));
    let state = keep(state, "pages", json!(pages));
    keep(state, "items", Value::Array(items))
}

fn entries_of(state: &Value) -> Vec<api::Entry> {
    state
        .get("items")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(api::entry_of).collect())
        .unwrap_or_default()
}

/// 一份站点回执 → 一页记录。所有「站点怎么说的」判断都收在这里。
fn read_page(provided: &Value, key: &str) -> Result<api::Page, String> {
    if let Some(err) = got_err(provided, key) {
        return Err(f(&t("files.list_fail"), &[&err]));
    }
    let reply = got(provided, key);
    let status = reply.get("status").and_then(Value::as_u64).unwrap_or(0);
    let body = s(reply.get("body"));
    if status >= 400 {
        return Err(site::api_error(status as u16, &body));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| f(&t("files.bad_body"), &[&e.to_string()]))?;
    Ok(api::parse_files(&value))
}

fn sort_of(rq: &Rq) -> String {
    let sort = arg(rq, "sort");
    if sort.is_empty() {
        "name".to_string()
    } else {
        sort
    }
}

/// 把索引切成「`folder` 这一层」的条目，按界面给的键排好。
///
/// 目录永远排在文件前；目录没有大小（站点没有目录对象），时间取该目录下最新的一条，
/// 这样按时间倒序时「最近传过东西的目录」会冒到前面。
fn list_folder(files: &[api::Entry], folder: &str, sort_key: &str, desc: bool) -> Vec<Value> {
    let dir = folder.trim().trim_matches('/');
    let mut dirs: Vec<api::Entry> = Vec::new();
    let mut rows: Vec<api::Entry> = Vec::new();
    for entry in files {
        if entry.folder == dir {
            rows.push(entry.clone());
            continue;
        }
        // 只认当前目录下面一层：更深的路径只贡献它的第一个路径段。
        let rest = if dir.is_empty() {
            entry.folder.as_str()
        } else if let Some(rest) = entry.folder.strip_prefix(&format!("{dir}/")) {
            rest
        } else {
            continue;
        };
        let seg = rest.split('/').next().unwrap_or(rest);
        if seg.is_empty() {
            continue;
        }
        match dirs.iter_mut().find(|row| row.name == seg) {
            Some(row) => {
                if entry.uploaded > row.uploaded {
                    row.uploaded = entry.uploaded.clone();
                }
            }
            None => dirs.push(api::Entry {
                id: String::new(),
                name: seg.to_string(),
                folder: String::new(),
                size: 0,
                uploaded: entry.uploaded.clone(),
            }),
        }
    }
    let by = |a: &api::Entry, b: &api::Entry| -> std::cmp::Ordering {
        let ord = if sort_key == "size" {
            a.size.cmp(&b.size).then_with(|| a.name.cmp(&b.name))
        } else if sort_key == "mtime" {
            a.uploaded
                .cmp(&b.uploaded)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        } else {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        };
        if desc {
            ord.reverse()
        } else {
            ord
        }
    };
    dirs.sort_by(by);
    rows.sort_by(by);
    dirs.extend(rows);
    dirs.iter()
        .map(|row| {
            json!({
                "name": row.name,
                "is_dir": row.id.is_empty(),
                "size": row.size,
                "uploaded": row.uploaded,
            })
        })
        .collect()
}

// ---------------------------------------------------------------- 上传

pub fn on_remote_upload(id: u64, rq: &Rq) -> Value {
    let plan = match resolve(id, rq) {
        Step::Reply(reply) => return reply,
        Step::Ready(plan) => plan,
    };
    if state_s(&rq.state, "step") != STEP_UP {
        let name = arg(rq, "file_name");
        if name.is_empty() {
            return fail(id, &t("files.need_file").to_string());
        }
        // 宿主按「remote-<文件名>」记下这次挑选；令牌不同步过去，宿主取不到授权，
        // 字节一个也发不出去。所以这里必须原样写回宿主给的那个键。
        let key = {
            let key = arg(rq, "file_key");
            if key.is_empty() {
                "file".to_string()
            } else {
                key
            }
        };
        let folder = plan.dir(&arg(rq, "path"));
        let mut request = api::upload_request(
            &plan.base,
            &plan.token,
            &name,
            &folder,
            &plan.storage,
            plan.dedup,
        );
        request["source"]["form_key"] = json!(key);
        let state = keep(rq.state.clone(), "plan", plan.to_value());
        let state = keep(state, "step", json!(STEP_UP));
        return need(id, state, vec![need_item("http.upload", "up", request)]);
    }
    if let Some(err) = got_err(&rq.provided, "up") {
        let message = if err.contains("http.upload") {
            t("files.need_cap").to_string()
        } else {
            f(&t("files.upload_fail"), &[&err])
        };
        return fail(id, &message);
    }
    let reply = got(&rq.provided, "up");
    let status = reply.get("status").and_then(Value::as_u64).unwrap_or(0);
    let body = s(reply.get("body"));
    if status >= 400 {
        return fail(id, &site::api_error(status as u16, &body));
    }
    let uploaded = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| api::parse_upload(&value));
    let Some(uploaded) = uploaded else {
        return fail(id, &f(&t("files.bad_body"), &[&truncate(&body, 120)]));
    };
    // 站点可能只回 `/f/1` 这样的相对地址；插件知道站点在哪，补全后再交出去。
    let link = api::absolutize(&plan.base, &uploaded.link);
    done(
        id,
        json!({
            "name": uploaded.name,
            "link": link,
            "deduped": uploaded.deduped,
        }),
    )
}

// ---------------------------------------------------------------- 删除

pub fn on_remote_delete(id: u64, rq: &Rq) -> Value {
    let plan = match resolve(id, rq) {
        Step::Reply(reply) => return reply,
        Step::Ready(plan) => plan,
    };
    match state_s(&rq.state, "step").as_str() {
        STEP_DEL => after_batch(id, rq),
        STEP_PAGE => match read_page(&rq.provided, "page") {
            Ok(page) => more_or_plan(id, rq, &plan, page),
            Err(message) => fail(id, &message),
        },
        _ => {
            let state = page_state(rq.state.clone(), &plan, 0, &[]);
            ask_page(id, state, &plan, "")
        }
    }
}

fn more_or_plan(id: u64, rq: &Rq, plan: &Plan, page: api::Page) -> Value {
    let mut entries = entries_of(&rq.state);
    entries.extend(page.entries);
    let pages = rq.state.get("pages").and_then(Value::as_u64).unwrap_or(0) + 1;
    let next = page.next.unwrap_or_default();
    if !page.complete && !next.is_empty() && pages < u64::from(api::MAX_PAGES) {
        let state = page_state(rq.state.clone(), &plan, pages, &entries);
        return ask_page(id, state, plan, &next);
    }
    plan_deletes(id, rq, plan, &entries)
}

/// 「目录 + 名字」→ 要到站点上删的东西，然后逐批发请求。
fn plan_deletes(id: u64, rq: &Rq, plan: &Plan, entries: &[api::Entry]) -> Value {
    let dir = plan.dir(&arg(rq, "path"));
    let items = rq
        .args
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut pending: Vec<Value> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in &items {
        let name = s(item.get("name")).trim().to_string();
        if name.is_empty() || seen.iter().any(|n| n == &name) {
            continue;
        }
        seen.push(name.clone());
        let is_dir = migrate::truthy(item.get("is_dir"));
        let ids = pick_targets(entries, &dir, &name, is_dir);
        if ids.is_empty() {
            missing.push(name);
            continue;
        }
        if !is_dir && ids.len() > 1 {
            // 同名文件不止一条：删哪一条是用户的事，插件不能替他猜。
            failed.push(f(
                &t("files.name_ambiguous"),
                &[&name, &ids.len().to_string()],
            ));
            continue;
        }
        for one in ids {
            pending.push(json!({ "name": name, "id": one }));
        }
    }
    if pending.len() > DEL_LIMIT {
        failed.push(f(&t("files.too_many"), &[&DEL_LIMIT.to_string()]));
        pending.truncate(DEL_LIMIT);
    }
    if pending.is_empty() {
        return done(
            id,
            json!({ "deleted": [], "missing": missing, "failed": failed }),
        );
    }
    let state = keep(rq.state.clone(), "plan", plan.to_value());
    let state = keep(state, "step", json!(STEP_DEL));
    let state = keep(state, "pending", Value::Array(pending));
    let state = keep(state, "ok", json!([]));
    let state = keep(state, "bad", json!([]));
    let state = keep(state, "missing", json!(missing));
    let state = keep(state, "failed", json!(failed));
    issue_batch(id, state)
}

/// 发下一批删除请求；没有剩余项就直接交结果。
fn issue_batch(id: u64, state: Value) -> Value {
    let plan = Plan::of_state(&state);
    let pending = state
        .get("pending")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let take: Vec<Value> = pending.iter().take(DEL_BATCH).cloned().collect();
    let rest: Vec<Value> = pending.iter().skip(DEL_BATCH).cloned().collect();
    let state = keep(state, "pending", Value::Array(rest));
    if take.is_empty() {
        return finish_delete(id, &state);
    }
    let mut batch: Vec<Value> = Vec::new();
    let mut needs: Vec<Value> = Vec::new();
    for (index, item) in take.iter().enumerate() {
        let key = format!("del{index}");
        let name = s(item.get("name")).trim().to_string();
        let one = s(item.get("id")).trim().to_string();
        batch.push(json!({ "key": key, "name": name, "id": one }));
        needs.push(need_item(
            "http.request",
            &key,
            api::delete_request(&plan.base, &plan.token, &one),
        ));
    }
    let state = keep(state, "batch", Value::Array(batch));
    need(id, state, needs)
}

/// 一批删除的回执：成功的记名字，失败的留原因。
///
/// 宿主按名字把列表里的行摘掉，所以这里报回去的必须是**用户在界面上看见的名字**，
/// 不是站点主键。
fn after_batch(id: u64, rq: &Rq) -> Value {
    let batch = rq
        .state
        .get("batch")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut ok = strings_of(&rq.state, "ok");
    let mut bad: Vec<Value> = rq
        .state
        .get("bad")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for item in &batch {
        let key = s(item.get("key"));
        let name = s(item.get("name")).trim().to_string();
        if let Some(err) = got_err(&rq.provided, &key) {
            bad.push(json!({ "name": name, "reason": err }));
            continue;
        }
        let reply = got(&rq.provided, &key);
        if !reply.is_object() {
            bad.push(json!({
                "name": name,
                "reason": t("files.delete_no_reply").to_string(),
            }));
            continue;
        }
        let status = reply.get("status").and_then(Value::as_u64).unwrap_or(0);
        let body = s(reply.get("body"));
        if status >= 400 || status == 0 {
            let reason = if status == 0 {
                t("files.delete_no_reply").to_string()
            } else {
                site::api_error(status as u16, &body)
            };
            bad.push(json!({ "name": name, "reason": reason }));
            continue;
        }
        if !ok.iter().any(|n| n == &name) {
            ok.push(name);
        }
    }
    let state = keep(rq.state.clone(), "ok", json!(ok));
    let state = keep(state, "bad", Value::Array(bad));
    issue_batch(id, state)
}

fn finish_delete(id: u64, state: &Value) -> Value {
    let ok = strings_of(state, "ok");
    let missing = strings_of(state, "missing");
    let mut failed = strings_of(state, "failed");
    let bad: Vec<Value> = state
        .get("bad")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut deleted: Vec<String> = Vec::new();
    for name in &ok {
        let reason = bad
            .iter()
            .find(|item| s(item.get("name")).trim() == name.as_str())
            .map(|item| s(item.get("reason")));
        match reason {
            // 同名文件里删掉了一部分：报成功会误导人，报失败才让人去站点核对。
            Some(reason) => failed.push(format!("{name}: {reason}")),
            None => deleted.push(name.clone()),
        }
    }
    // 完全没碰到的项也不用再报「失败」，它们在 missing 里已经说清楚了。
    done(
        id,
        json!({
            "deleted": deleted,
            "missing": missing,
            "failed": failed,
        }),
    )
}

fn strings_of(state: &Value, key: &str) -> Vec<String> {
    state
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 在站点索引里找出「目录 `folder` 下的 `name`」对应哪些文件。
///
/// 目录返回它自己与它下面所有文件的 id；文件返回匹配项（可能不止一条，调用方决定怎么办）。
/// 带 DAV 后缀的显示名（`a~ff0011.png`）优先按「主键末 6 位」定位——后缀就是这么来的，
/// 能唯一定位时就不会误删同名的另一个文件。
fn pick_targets(files: &[api::Entry], folder: &str, name: &str, is_dir: bool) -> Vec<String> {
    let dir = folder.trim().trim_matches('/');
    if is_dir {
        let self_dir = if dir.is_empty() {
            strip_collision_suffix(name)
        } else {
            format!("{dir}/{}", strip_collision_suffix(name))
        };
        let under = format!("{self_dir}/");
        return files
            .iter()
            .filter(|entry| entry.folder == self_dir || entry.folder.starts_with(&under))
            .map(|entry| entry.id.clone())
            .collect();
    }
    let want = strip_collision_suffix(name);
    let candidates: Vec<&api::Entry> = files
        .iter()
        .filter(|entry| entry.folder == dir)
        .filter(|entry| entry.name == name || entry.name == want)
        .collect();
    if let Some(tail) = collision_tail(name) {
        let exact: Vec<String> = candidates
            .iter()
            .filter(|entry| entry.id.ends_with(tail))
            .map(|entry| entry.id.clone())
            .collect();
        if exact.len() == 1 {
            return exact;
        }
    }
    candidates.iter().map(|entry| entry.id.clone()).collect()
}

/// 站点给重名文件补的 DAV 显示后缀本体（`a~ab12cd.txt` → `ab12cd`）。
///
/// 后缀就是文件主键的末 6 位，所以它同时也是「用户点的到底是哪一条」的唯一线索。
fn collision_tail(name: &str) -> Option<&str> {
    let base = match name.rfind('.') {
        Some(index) if index > 0 => &name[..index],
        _ => name,
    };
    let tilde = base.rfind('~')?;
    let tail = &base[tilde + 1..];
    (tail.len() == 6 && tail.chars().all(|c| c.is_ascii_alphanumeric())).then_some(tail)
}

fn strip_collision_suffix(name: &str) -> String {
    let Some(tail) = collision_tail(name) else {
        return name.to_string();
    };
    let marker = format!("~{tail}");
    match name.find(&marker) {
        Some(index) => format!("{}{}", &name[..index], &name[index + marker.len()..]),
        None => name.to_string(),
    }
}

// ---------------------------------------------------------------- 接管旧设置

/// 宿主把旧命名空间里的东西整份交给插件，插件只补空缺、绝不覆盖。
///
/// 覆盖是最坏的失败方式：用户在新插件里改过的地址、换过的令牌会被旧副本悄悄顶掉，
/// 而且没有提示。所以这里只看「新的一格是不是空的」，不比较、不合并优先级。
pub fn on_adopt_legacy(id: u64, rq: &Rq) -> Value {
    let namespace = arg(rq, "namespace");
    let legacy = rq.args.get("legacy").cloned().unwrap_or(Value::Null);
    let Some(map) = legacy.as_object() else {
        return fail(id, &t("legacy.bad").to_string());
    };
    if state_s(&rq.state, "step") != STEP_KV {
        return need(
            id,
            keep(rq.state.clone(), "step", json!(STEP_KV)),
            vec![need_item("kv.get_all", "kv", json!({}))],
        );
    }
    let kv = got(&rq.provided, "kv");
    let mut ops: Vec<Value> = Vec::new();
    // 这次真的写了哪些键。宿主拿它去自己的库里核对——插件这一轮写完就返回了，
    // 看不到写后状态，只有宿主能确认「确实落盘」。核不上，宿主就不记账，下次再交。
    let mut stored: Vec<String> = Vec::new();
    let mut taken = 0usize;
    let mut kept = 0usize;
    for (name, value) in map {
        if name == SITE_KEY {
            let current = kv
                .get(SITE_KEY)
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            let merged = fill_missing(current.clone(), value);
            if merged == current {
                kept += 1;
            } else {
                ops.push(json!({ "op": "kv.set", "key": SITE_KEY, "value": merged }));
                stored.push(SITE_KEY.to_string());
                taken += 1;
            }
            continue;
        }
        let token = s(value.get("token")).trim().to_string();
        if token.is_empty() {
            continue;
        }
        let present = !s(kv.get(name.as_str()).and_then(|v| v.get("token")))
            .trim()
            .is_empty();
        if present {
            kept += 1;
            continue;
        }
        ops.push(json!({ "op": "kv.set", "key": name, "value": { "token": token } }));
        stored.push(name.clone());
        taken += 1;
    }
    done_ops(
        id,
        ops,
        json!({
            "namespace": namespace,
            "taken": taken,
            "kept": kept,
            "stored_keys": stored,
        }),
    )
}

/// 只把「当前为空、旧的有值」的字段填进去。
fn fill_missing(current: Value, old: &Value) -> Value {
    let Some(old_map) = old.as_object() else {
        return current;
    };
    let mut out = current;
    let Some(target) = out.as_object_mut() else {
        return out;
    };
    for (key, value) in old_map {
        let empty = match target.get(key) {
            None => true,
            Some(existing) => existing.is_null() || s(Some(existing)).trim().is_empty(),
        };
        if empty && !s(Some(value)).trim().is_empty() {
            target.insert(key.clone(), value.clone());
        }
    }
    out
}

/// 让 `main.rs` 的测试和面板都能造一份「站点」形状的值（保持与 `site` 键一致）。
#[allow(dead_code)]
fn site_value(base: &str, token: &str, folder: &str, storage: &str, dedup: bool) -> Value {
    let mut map = Map::new();
    map.insert("base".into(), json!(base));
    map.insert("token".into(), json!(token));
    map.insert("folder".into(), json!(folder));
    map.insert("storage".into(), json!(storage));
    map.insert("dedup".into(), json!(dedup));
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, folder: &str, size: u64, uploaded: &str) -> api::Entry {
        api::Entry {
            id: id.into(),
            name: name.into(),
            folder: folder.into(),
            size,
            uploaded: uploaded.into(),
        }
    }

    #[test]
    fn list_folder_shows_one_level_and_folds_deeper_paths() {
        let files = vec![
            entry("a", "a.png", "", 10, "2026-01-01T00:00:00Z"),
            entry("b", "b.txt", "", 20, "2026-01-03T00:00:00Z"),
            entry("c", "c.png", "pics/2026", 30, "2026-02-01T00:00:00Z"),
            entry("d", "d.png", "pics", 40, "2026-01-05T00:00:00Z"),
        ];
        let rows = list_folder(&files, "", "name", false);
        let names: Vec<String> = rows.iter().map(|r| s(r.get("name"))).collect();
        assert_eq!(names, vec!["pics", "a.png", "b.txt"]);
        assert_eq!(rows[0]["is_dir"], json!(true));
        // 目录的时间取它下面最新的一条，按时间倒序时才会冒到前面。
        assert_eq!(s(rows[0].get("uploaded")), "2026-02-01T00:00:00Z");
    }

    #[test]
    fn list_folder_inside_a_directory_only_lists_that_directory() {
        let files = vec![
            entry("a", "a.png", "pics", 10, "2026-01-01T00:00:00Z"),
            entry("b", "b.png", "pics/2026", 20, "2026-01-02T00:00:00Z"),
            entry("c", "c.png", "", 30, "2026-01-03T00:00:00Z"),
        ];
        let rows = list_folder(&files, "pics", "name", false);
        let names: Vec<String> = rows.iter().map(|r| s(r.get("name"))).collect();
        assert_eq!(names, vec!["2026", "a.png"]);
    }

    #[test]
    fn list_folder_sorts_by_size_and_time() {
        let files = vec![
            entry("a", "a.png", "", 30, "2026-01-01T00:00:00Z"),
            entry("b", "b.png", "", 10, "2026-01-03T00:00:00Z"),
            entry("c", "c.png", "", 20, "2026-01-02T00:00:00Z"),
        ];
        let by_size = list_folder(&files, "", "size", true);
        let names: Vec<String> = by_size.iter().map(|r| s(r.get("name"))).collect();
        assert_eq!(names, vec!["a.png", "c.png", "b.png"]);
        let by_time = list_folder(&files, "", "mtime", false);
        let names: Vec<String> = by_time.iter().map(|r| s(r.get("name"))).collect();
        assert_eq!(names, vec!["a.png", "c.png", "b.png"]);
    }

    #[test]
    fn a_dav_suffix_picks_the_one_file_that_user_clicked() {
        let files = vec![
            entry("tg:ab12cd", "a.png", "", 1, ""),
            entry("tg:ef34ab", "a.png", "", 2, ""),
        ];
        // 显示名带后缀时按主键末 6 位唯一定位，不会把同名的另一条也删掉。
        let ids = pick_targets(&files, "", "a~ab12cd.png", false);
        assert_eq!(ids, vec!["tg:ab12cd".to_string()]);
        // 不带后缀的同名文件返回全部候选，交给调用方决定「报不明确」。
        let ids = pick_targets(&files, "", "a.png", false);
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn deleting_a_directory_takes_everything_under_it() {
        let files = vec![
            entry("1", "a.png", "pics", 1, ""),
            entry("2", "b.png", "pics/2026", 1, ""),
            entry("3", "c.png", "pics2", 1, ""),
        ];
        let mut ids = pick_targets(&files, "", "pics", true);
        ids.sort();
        assert_eq!(ids, vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn legacy_settings_only_fill_empty_slots() {
        let current = json!({ "base": "https://new", "token": "", "folder": "" });
        let old = json!({ "base": "https://old", "token": "t0k", "folder": "pics" });
        let merged = fill_missing(current, &old);
        // 新地址保留，空的令牌与目录补上——旧副本不会把用户改过的东西顶掉。
        assert_eq!(s(merged.get("base")), "https://new");
        assert_eq!(s(merged.get("token")), "t0k");
        assert_eq!(s(merged.get("folder")), "pics");
    }

    #[test]
    fn a_plan_prefers_the_path_it_was_given() {
        let plan = Plan {
            base: "https://x".into(),
            token: "t".into(),
            folder: "upload".into(),
            storage: zone(),
            dedup: true,
        };
        assert_eq!(plan.dir(""), "upload");
        assert_eq!(plan.dir("/pics/2026/"), "pics/2026");
    }

    fn zone() -> String {
        site::DEFAULT_STORAGE.to_string()
    }
}
