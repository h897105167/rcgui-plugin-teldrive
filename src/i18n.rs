// 面板文案：中英各一份，key 是点分小写串。
// 语言由宿主每轮通过 params.lang 传入（"zh" | "en"），缺省用中文；
// 找不到的 key 原样返回，方便在面板上直接看出漏翻。

use serde_json::Value;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

const TABLE: &str = include_str!("i18n.json");

static LANG: AtomicU8 = AtomicU8::new(0); // 0 = zh, 1 = en
static PARSED: OnceLock<Value> = OnceLock::new();

fn table() -> &'static Value {
    PARSED.get_or_init(|| serde_json::from_str(TABLE).unwrap_or(Value::Null))
}

pub fn set_lang(lang: Option<&str>) {
    let want_en = matches!(lang, Some(l) if l.eq_ignore_ascii_case("en"));
    LANG.store(if want_en { 1 } else { 0 }, Ordering::Relaxed);
}

fn lookup(key: &str) -> Option<String> {
    let root = table();
    let primary = if LANG.load(Ordering::Relaxed) == 1 {
        "en"
    } else {
        "zh"
    };
    let fallback = if primary == "en" { "zh" } else { "en" };
    for name in [primary, fallback] {
        if let Some(v) = root
            .get(name)
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
        {
            return Some(v.to_string());
        }
    }
    None
}

pub fn t(key: &str) -> String {
    lookup(key).unwrap_or_else(|| key.to_string())
}

/// 按顺序填占位符。文案里两种写法都认：第 i 个参数先填 `{}`，没有就填 `{i}`。
/// 参数比占位符多时多出来的忽略，占位符比参数多时剩下的原样留着。
pub fn f(key: &str, args: &[&str]) -> String {
    let mut out = t(key);
    for (i, a) in args.iter().enumerate() {
        let pos = out.find("{}").or_else(|| out.find(&format!("{{{}}}", i)));
        match pos {
            Some(p) => {
                let len = if out[p..].starts_with("{}") {
                    2
                } else {
                    i.to_string().len() + 2
                };
                out.replace_range(p..p + len, a);
            }
            None => break,
        }
    }
    out
}
