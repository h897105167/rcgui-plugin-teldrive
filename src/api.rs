// teldrive 的 HTTP 接口调用。插件自己不能发请求，只能让宿主代发：
// 每个 call_need 返回一条 need，下一轮从 provided[key] 里拿 {status,content_type,body,size}。
// body 是文本，JSON 要自己解析。
//
// 认证有两种：登录前所有 /v1/auth/* 都不要认证；登录后拿 accessToken 当 Bearer，
// 或者拿 API key 当 X-Api-Key（建 rclone remote 时用的就是后者，因为它长期有效）。

use crate::proto::need_item;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};

pub fn base(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

static SEQ: AtomicU64 = AtomicU64::new(1);

/// 每次调用取一个不同的数交给 `idem`。
///
/// WASI 里拿不到随机源，用「当前纳秒时间 xor 进程内自增序号」：
/// 同一个进程内不会重复，重启后时间戳也不会回到旧值。
pub fn nonce() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    now ^ SEQ
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// teldrive 认的三种凭证。
///
/// 建 API key 和加机器人这两个接口只收 Bearer（OpenAPI 里的 security 没有
/// ExternalApiKeyAuth），所以登录后那几步必须用 `Bearer`；建 remote 时 rclone
/// 侧要的是长期有效的 key，才用 `ApiKey`。
#[derive(Clone, Debug)]
pub enum Auth {
    None,
    Bearer(String),
    ApiKey(String),
}

impl Auth {
    fn header(&self) -> Option<(&'static str, String)> {
        let (name, value) = match self {
            Auth::None => return None,
            Auth::Bearer(t) if t.trim().is_empty() => return None,
            Auth::Bearer(t) => ("Authorization", format!("Bearer {}", t.trim())),
            Auth::ApiKey(t) if t.trim().is_empty() => return None,
            Auth::ApiKey(t) => ("X-Api-Key", t.trim().to_string()),
        };
        Some((name, value))
    }
}

/// 组装一条 http.request need。
///
/// 空凭证表示匿名请求（`/v1/auth/telegram/*` 那几个就是这么发的）。
/// idempotent 为真时带一个 Idempotency-Key，便于宿主侧重试同一次创建。
pub fn call_need_auth(
    key: &str,
    port: u16,
    method: &str,
    path: &str,
    auth: &Auth,
    body: Option<Value>,
    idempotent: bool,
    nonce: u64,
) -> Value {
    let mut headers = Map::new();
    if body.is_some() {
        headers.insert("Content-Type".into(), json!("application/json"));
    }
    if let Some((name, value)) = auth.header() {
        headers.insert(name.into(), json!(value));
    }
    if idempotent {
        headers.insert("Idempotency-Key".into(), json!(idem(nonce)));
    }

    let mut params = Map::new();
    params.insert("url".into(), json!(format!("{}{}", base(port), path)));
    params.insert("method".into(), json!(method));
    params.insert("headers".into(), Value::Object(headers));
    params.insert("timeout_ms".into(), json!(60_000));
    if let Some(b) = body {
        params.insert("body".into(), json!(b.to_string()));
    }
    need_item("http.request", key, Value::Object(params))
}

pub struct Reply {
    pub status: u16,
    pub json: Value,
    pub raw: String,
}

/// 把 http.request 的结果统一成「状态码 + 解析后的 JSON + 原文」。
pub fn reply(v: &Value) -> Reply {
    let status = v.get("status").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
    let raw = v
        .get("body")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let json = if raw.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&raw).unwrap_or(Value::Null)
    };
    Reply { status, json, raw }
}

/// teldrive 的错误体是 {"error":{"code","message",...}}；不是它就把原文截断交出去。
pub fn err_text(r: &Reply) -> String {
    if let Some(msg) = r
        .json
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return format!("{} {}", r.status, msg);
    }
    let raw = r.raw.trim();
    if raw.is_empty() {
        format!("HTTP {}", r.status)
    } else {
        format!("HTTP {} {}", r.status, crate::proto::truncate(raw, 200))
    }
}

/// v4 形状的 UUID 字符串。np 里没有随机数，用调用方给的计数器混一下——
/// 只要求每次不同，不需要密码学强度（它只是防重复提交）。
pub fn idem(nonce: u64) -> String {
    let mut x = nonce
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0x1234_5678_9ABC_DEF0);
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let a = next();
    let b = next();
    let hex = format!("{:016x}{:016x}", a, b);
    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

/// 二维码登录的第一步：让服务端导出一个扫码登录 token。
///
/// 成功是 201，体里是 `{flowId, expiresAt, state, qrUrl, qrExpiresAt}`，`qrUrl`
/// 形如 `tg://login?token=…`，就是二维码的内容。`state` 只可能是 `pending` 或
/// `password_required`；后者表示这个账号先要过两步密码，不会给二维码。
pub fn qr_start_need(key: &str, port: u16, nonce: u64) -> Value {
    call_need_auth(
        key,
        port,
        "POST",
        "/v1/auth/telegram/qr/start",
        &Auth::None,
        Some(json!({})),
        true,
        nonce,
    )
}

/// 二维码登录的轮询：手机确认前是 202（体里带一个新 `qrUrl`），确认后是 200，
/// 体是 `{accessToken, refreshToken, tokenType, expiresIn}`。
///
/// 服务端把 `Idempotency-Key` 当作必填头（缺了就回 400 "request could not be decoded"），
/// 而且按这个 key 缓存响应：同一个 key 再问只会拿回第一次的 202，二维码就再也换不了、
/// 登录成功也发现不了。调用方每轮取一次 `nonce()`，所以这里天然每次都不同。
pub fn qr_poll_need(key: &str, port: u16, flow_id: &str, nonce: u64) -> Value {
    let body = json!({ "flowId": flow_id });
    call_need_auth(
        key,
        port,
        "POST",
        "/v1/auth/telegram/qr/poll",
        &Auth::None,
        Some(body),
        true,
        nonce,
    )
}

/// 从响应里按几个候选键依次取值，取到第一个非空字符串为止。
///
/// 用它是为了同时兼容 `accessToken` 与 `access_token` 两种写法。
pub fn pick(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// 从登录返回的 TokenPair 里取 accessToken。
pub fn access_token(v: &Value) -> String {
    pick(v, &["accessToken", "access_token"])
}

/// 列频道：`GET /v1/channels`。成功 200，体是 `{items:[{id,name,selected,health,…}]}`。
///
/// 频道 id 是 Telegram 的 int64，可能是负数；调用方按字符串保存，别当数字解析后再
/// 序列化回 JSON，`f64` 会把 19 位的 id 抹掉末尾几位。
pub fn channels_need(key: &str, port: u16, auth: &Auth) -> Value {
    call_need_auth(key, port, "GET", "/v1/channels", auth, None, false, 0)
}

/// 切换当前频道：`POST /v1/channels/{id}/select`，200。
///
/// 这个接口不改频道本身，只把 `selected` 挪过去；服务端对它没有副作用，
/// 但同一路径的 POST 一律带幂等键更省事。
pub fn channel_select_need(key: &str, port: u16, id: &str, auth: &Auth, nonce: u64) -> Value {
    let path = format!("/v1/channels/{}/select", id.trim());
    call_need_auth(key, port, "POST", &path, auth, Some(json!({})), true, nonce)
}

/// 列机器人：`GET /v1/bots`。成功 200，体是 `{items:[{id,username,enabled,…}]}`。
///
/// 列表是账号级的：teldrive 的 `bots` 表只按 `user_id` 存，没有频道维度，
/// 所以一个账号下所有机器人都会出现在这里，与当前选中哪个频道无关。
pub fn bots_need(key: &str, port: u16, auth: &Auth) -> Value {
    call_need_auth(key, port, "GET", "/v1/bots", auth, None, false, 0)
}

/// 删机器人。只从 teldrive 的机器人池里移除，Telegram 上的机器人本身不动。
/// 成功返回 204、没有 body，调用方只看 status。这一系列接口同样只认 Bearer。
pub fn bot_delete_need(key: &str, port: u16, bot_id: &str, auth: &Auth) -> Value {
    let path = format!("/v1/bots/{}", bot_id.trim());
    call_need_auth(key, port, "DELETE", &path, auth, None, false, 0)
}
