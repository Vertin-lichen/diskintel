//! 检索历史：最近提交过的查询，供检索框下拉复用。
//!
//! 存储位置 %APPDATA%\DiskIntel\search-history.json，
//! 内容就是一个 JSON 字符串数组（例如 ["报告","ext:pdf size:>10m"]）。
//! 刻意不引入 serde：格式只有一层数组，手写三十行解析比多一个依赖更划算；
//! 任何解析失败都退化成「没有历史」，绝不 panic。

use std::path::PathBuf;

/// 最多保留的条数（最近在前）。
pub const MAX_ITEMS: usize = 20;

/// 历史文件路径：%APPDATA%\DiskIntel\search-history.json。
///
/// 取不到 APPDATA 时退到临时目录，保证「读不到就空历史」而不是报错退出。
pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("DiskIntel").join("search-history.json")
}

/// 读取历史；文件不存在或内容损坏都返回空列表。
pub fn load() -> Vec<String> {
    load_from(&path())
}

/// 从指定文件读取（测试与未来导入功能用）。
fn load_from(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse(&text)
}

/// 记录一次真正提交过的检索：去重、最近在前、最多 MAX_ITEMS 条。
///
/// 空白查询不记录（清空搜索框不算一次检索）。返回写入后的最新列表。
pub fn push(query: &str) -> Vec<String> {
    let mut items = load();
    insert(&mut items, query);
    save(&items);
    items
}

/// 删除一条历史（下拉面板每行的 ×）。返回最新列表。
pub fn remove(query: &str) -> Vec<String> {
    let mut items = load();
    items.retain(|item| item != query);
    save(&items);
    items
}

/// 清空历史（下拉面板底部的「清空历史」）。
pub fn clear() -> Vec<String> {
    save(&[]);
    Vec::new()
}

fn save(items: &[String]) {
    save_to(&path(), items);
}

/// 写到指定文件（父目录不存在会先建）。
fn save_to(path: &std::path::Path, items: &[String]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(error) = std::fs::write(path, render(items)) {
        eprintln!("[history] 写入失败：{error}");
    }
}

/// 纯内存版的记录逻辑（可单测）：去重 → 置顶 → 截断。
fn insert(items: &mut Vec<String>, query: &str) {
    let query = query.trim();
    if query.is_empty() {
        return;
    }
    items.retain(|item| item != query);
    items.insert(0, query.to_owned());
    items.truncate(MAX_ITEMS);
}

/// 序列化成紧凑 JSON 数组。
fn render(items: &[String]) -> String {
    let mut out = String::with_capacity(items.len() * 16 + 2);
    out.push('[');
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('"');
        for ch in item.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}

/// 解析 JSON 字符串数组；遇到任何意外都返回「已解析出的部分」或空列表。
fn parse(text: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    let mut chars = text.chars().peekable();
    // 跳到第一个 '['
    loop {
        match chars.next() {
            Some('[') => break,
            Some(_) => {}
            None => return items,
        }
    }
    loop {
        // 跳过分隔与空白，找下一个字符串或数组结束
        loop {
            match chars.next() {
                Some(c) if c.is_whitespace() || c == ',' => {}
                Some('"') => break,
                Some(']') | None => return items,
                Some(_) => return items, // 非法内容：后面的都丢掉
            }
        }
        let mut item = String::new();
        loop {
            match chars.next() {
                Some('"') => break,
                Some('\\') => match chars.next() {
                    Some('"') => item.push('"'),
                    Some('\\') => item.push('\\'),
                    Some('/') => item.push('/'),
                    Some('b') => item.push('\u{0008}'),
                    Some('f') => item.push('\u{000c}'),
                    Some('n') => item.push('\n'),
                    Some('r') => item.push('\r'),
                    Some('t') => item.push('\t'),
                    Some('u') => item.push(hex4(&mut chars)),
                    Some(other) => item.push(other),
                    None => return items,
                },
                Some(c) => item.push(c),
                None => return items,
            }
        }
        let item = item.trim().to_owned();
        if !item.is_empty() && !items.contains(&item) {
            items.push(item);
            items.truncate(MAX_ITEMS);
        }
    }
}

/// 读 \uXXXX 的四个十六进制位；非法就退成替换字符，绝不 panic。
fn hex4(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> char {
    let mut code = 0u32;
    for _ in 0..4 {
        let Some(ch) = chars.next() else {
            return '\u{fffd}';
        };
        let Some(digit) = ch.to_digit(16) else {
            return '\u{fffd}';
        };
        code = code * 16 + digit;
    }
    char::from_u32(code).unwrap_or('\u{fffd}')
}

#[cfg(test)]
mod tests {
    use super::{MAX_ITEMS, insert, load_from, parse, render, save_to};

    #[test]
    fn roundtrip_keeps_special_chars() {
        let items = vec![
            "报告".to_owned(),
            "ext:pdf size:>10m".to_owned(),
            "带\"引号\"与\\反斜杠".to_owned(),
            "带\n换行".to_owned(),
        ];
        let text = render(&items);
        assert_eq!(parse(&text), items);
    }

    #[test]
    fn parse_tolerates_garbage() {
        assert!(parse("").is_empty());
        assert!(parse("not json").is_empty());
        assert!(parse("[]").is_empty());
        assert_eq!(parse("[\"a\",\"a\",\"b\"]"), vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(parse("[ \"a\" , \"b\" ]"), vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(parse("[\"\\u62a5\\u544a\"]"), vec!["报告".to_owned()]);
    }

    #[test]
    fn file_roundtrip_through_disk() {
        // 真实落盘一次：验证父目录自动创建、UTF-8 中文、特殊字符都能原样读回。
        let path = std::env::temp_dir()
            .join(format!("diskintel-history-test-{}.json", std::process::id()));
        save_to(&path, &[]);
        assert!(load_from(&path).is_empty());
        let items = vec![
            "报告".to_owned(),
            "ext:pdf size:>10m".to_owned(),
            "D:\\资料\\报价单".to_owned(),
            "带\"引号\"".to_owned(),
        ];
        save_to(&path, &items);
        assert_eq!(load_from(&path), items);
    }

    #[test]
    fn insert_dedups_and_caps() {
        let mut items = Vec::new();
        insert(&mut items, "  ");
        assert!(items.is_empty());
        insert(&mut items, "a");
        insert(&mut items, "b");
        insert(&mut items, "a");
        assert_eq!(items, vec!["a".to_owned(), "b".to_owned()]);
        for index in 0..40 {
            insert(&mut items, &format!("q{index}"));
        }
        assert_eq!(items.len(), MAX_ITEMS);
        assert_eq!(items[0], "q39");
    }
}
