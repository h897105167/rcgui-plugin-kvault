// 插件与宿主之间的四类回复和通用取值辅助。
//
// 一轮调用只能是下面两种之一：
//   {id, need:[...], state}                     —— 申请能力，下一轮在 provided 里拿结果
//   {id, ops:[...], result:{status, body}}      —— 写 kv、弹 toast，并把面板状态写回
// 所以「先查再决定」必须分轮：查到的东西塞进 state 带到下一轮。

use serde_json::{json, Map, Value};

pub struct Rq {
    pub form: Value,
    /// 宿主整份参数。面板动作用 `form`，宿主按操作协议发来的参数
    /// （`remote`/`path`/`items`/`file_name`/`file_key`/`legacy`…）落在这一层的顶上，
    /// 因为它们不属于面板表单，混进 `form` 会让面板看起来像用户填了这些东西。
    pub args: Value,
    pub provided: Value,
    pub state: Value,
}

pub fn s(v: Option<&Value>) -> String {
    v.and_then(|x| x.as_str()).unwrap_or_default().to_string()
}

/// 能力失败时宿主把该 key 的结果替换成 {"_error": "..."}，而不是抛异常。
pub fn got(provided: &Value, key: &str) -> Value {
    provided.get(key).cloned().unwrap_or(Value::Null)
}

pub fn got_err(provided: &Value, key: &str) -> Option<String> {
    provided
        .get(key)
        .and_then(|v| v.get("_error"))
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
}

pub fn need_item(cap: &str, key: &str, params: Value) -> Value {
    json!({ "cap": cap, "key": key, "params": params })
}

pub fn need(id: u64, state: Value, list: Vec<Value>) -> Value {
    json!({ "id": id, "need": list, "state": state })
}

pub fn done_ops(id: u64, ops: Vec<Value>, body: Value) -> Value {
    json!({ "id": id, "ops": ops, "result": { "status": 200, "body": body } })
}

pub fn done(id: u64, body: Value) -> Value {
    done_ops(id, Vec::new(), body)
}

/// 给用户看的失败：写进面板状态并弹一条红色提示，不用 error 通道（error 只表示插件自己坏了）。
pub fn fail(id: u64, msg: &str) -> Value {
    done(
        id,
        json!({
            "status": msg,
            "lamp": "err",
            "toast": { "level": "err", "message": msg },
        }),
    )
}

/// 往跨轮 state 里记一个键。
///
/// 第一轮宿主给的 state 是 null（还没有任何跨轮数据），所以这里必须自己开一个空对象：
/// 原来只对已有对象插入，null 时插入被静默丢掉，下一轮又从头开始，动作就永久卡在第一步
/// （表现出来是同一个能力被反复索取，面板永远转圈）。
pub fn keep(state: Value, key: &str, value: Value) -> Value {
    let mut map = match state {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    map.insert(key.to_string(), value);
    Value::Object(map)
}

pub fn state_s(state: &Value, key: &str) -> String {
    s(state.get(key))
}

pub fn fstr(form: &Value, key: &str) -> String {
    s(form.get(key)).trim().to_string()
}

/// 宿主这次操作带来的参数（顶层的那些，不在 `form` 里）。
pub fn arg(rq: &Rq, key: &str) -> String {
    s(rq.args.get(key)).trim().to_string()
}

/// 同 [`arg`]，按真假值读。宿主发来的是 JSON 的 bool，正文里写 `"true"` 也要认。
pub fn arg_flag(rq: &Rq, key: &str) -> bool {
    crate::migrate::truthy(rq.args.get(key))
}

/// 本次要用的 kv。
///
/// 宿主只在 `provided` 里放**本轮**问到的答案，所以刚问到的 kv 要从那里取；
/// 前几轮问到的只能靠 state 一路带下来。两者都没有（例如面板刷新时 state 是 null）
/// 就返回 Null，调用方按空表处理。
pub fn kv_pick(rq: &Rq) -> Value {
    let fresh = got(&rq.provided, "kv");
    if fresh.is_object() {
        fresh
    } else {
        rq.state.get("kv").cloned().unwrap_or(Value::Null)
    }
}

pub fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max).collect();
    out.push('…');
    out
}
