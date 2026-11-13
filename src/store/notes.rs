//! 快照备注的 sidecar：<data_root>\snapshots\notes.json。
//!
//! 为什么不用快照库的 meta 表：快照库是只读打开的（OpenFlags::SQLITE_OPEN_READ_ONLY），
//! 而且写进去会改动用户的历史观测数据。备注只属于界面层，用「快照名 → 备注」这样一个
//! 扁平 JSON 文件跟着快照目录走，备份快照目录时备注一起备份，删掉快照也不会牵连别的库。
//!
//! 格式只支持扁平对象，手写读写、不引入 serde：
//!
//!     {"20261112-100008": "扫描前状态", "20261112-094921": "加了 3 个游戏"}
//!
//! 文件缺失返回空表；内容损坏（非法 JSON、值不是字符串）时同样返回空表，绝不 panic，
//! 也绝不因为一次读失败就让快照页打不开。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 备注文件名（放在快照目录里，和 .sqlite 同处一层）。
pub const NOTES_FILE: &str = "notes.json";

/// 备注文件路径：<data_root>\snapshots\notes.json。
pub fn note_path(data_root: &Path) -> PathBuf {
    data_root.join("snapshots").join(NOTES_FILE)
}

/// 读全部备注。文件不存在 / 读不了 / 内容损坏都返回空表（不 panic、不报错）。
pub fn load(data_root: &Path) -> HashMap<String, String> {
    let path = note_path(data_root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    parse_object(&text).unwrap_or_default()
}

/// 写全部备注。先写同目录的临时文件再改名，避免写一半断电把 notes.json 写废。
pub fn save(data_root: &Path, notes: &HashMap<String, String>) -> Result<(), String> {
    let path = note_path(data_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建备注目录 {} 失败：{error}", parent.display()))?;
    }
    let mut keys: Vec<&String> = notes.keys().collect();
    keys.sort();
    let text = to_json(keys.into_iter().map(|key| (key.as_str(), notes[key].as_str())));
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)
        .map_err(|error| format!("写备注临时文件 {} 失败：{error}", temporary.display()))?;
    std::fs::rename(&temporary, &path).map_err(|error| {
        format!(
            "备注写入 {} 失败（临时文件 {} 仍在）：{error}",
            path.display(),
            temporary.display()
        )
    })
}

/// 序列化成扁平 JSON 对象（键按字典序，人眼和 diff 都稳定）。
fn to_json<'a>(entries: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut out = String::from("{\n");
    let mut first = true;
    for (key, value) in entries {
        if !first {
            out.push_str(",\n");
        }
        first = false;
        out.push_str("  ");
        push_string(&mut out, key);
        out.push_str(": ");
        push_string(&mut out, value);
    }
    if !first {
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

/// 写一个 JSON 字符串字面量：只转义 "、\、换行与制表符，其它控制字符压成空格。
fn push_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push(' '),
            ch => out.push(ch),
        }
    }
    out.push('"');
}

/// 解析扁平 JSON 对象；任何不认识的形状返回 None（调用方按空表处理）。
fn parse_object(text: &str) -> Option<HashMap<String, String>> {
    let chars: Vec<char> = text.trim_start_matches('\u{feff}').chars().collect();
    let mut cursor = Cursor { chars, at: 0 };
    cursor.skip_space();
    cursor.expect('{')?;
    let mut out = HashMap::new();
    cursor.skip_space();
    if cursor.peek() == Some('}') {
        cursor.at += 1;
        cursor.skip_space();
        return if cursor.at >= cursor.chars.len() { Some(out) } else { None };
    }
    loop {
        cursor.skip_space();
        let key = cursor.string()?;
        cursor.skip_space();
        cursor.expect(':')?;
        cursor.skip_space();
        let value = cursor.string()?;
        out.insert(key, value);
        cursor.skip_space();
        match cursor.peek()? {
            ',' => cursor.at += 1,
            '}' => {
                cursor.at += 1;
                break;
            }
            _ => return None,
        }
    }
    cursor.skip_space();
    if cursor.at >= cursor.chars.len() { Some(out) } else { None }
}

/// 极简游标：只认识空白、花括号、冒号、逗号和字符串。
struct Cursor {
    chars: Vec<char>,
    at: usize,
}

impl Cursor {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(ch) if ch.is_whitespace()) {
            self.at += 1;
        }
    }

    fn expect(&mut self, want: char) -> Option<()> {
        if self.peek()? == want {
            self.at += 1;
            Some(())
        } else {
            None
        }
    }

    /// 解析一个带转义的 JSON 字符串。
    fn string(&mut self) -> Option<String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            let ch = self.peek()?;
            self.at += 1;
            match ch {
                '"' => return Some(out),
                '\\' => {
                    let escaped = self.peek()?;
                    self.at += 1;
                    match escaped {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' => {
                            let mut code = 0u32;
                            for _ in 0..4 {
                                let digit = self.peek()?.to_digit(16)?;
                                self.at += 1;
                                code = code * 16 + digit;
                            }
                            out.push(char::from_u32(code)?);
                        }
                        _ => return None,
                    }
                }
                ch if (ch as u32) < 0x20 => return None,
                ch => out.push(ch),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一临时目录（与 trash.rs 同口径：只写自己的 %TEMP% 子目录，不做永久删除）。
    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join("diskintel-notes-tests")
            .join(format!("{tag}-{nanos}-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 写读往返：中文、引号、反斜杠、制表符、换行都要原样回来。
    #[test]
    fn save_and_load_round_trip() {
        let root = unique_dir("roundtrip");
        let mut notes = HashMap::new();
        notes.insert("20261112-100008".to_owned(), "扫描前状态".to_owned());
        notes.insert(
            "20261112-094921".to_owned(),
            "带 \"引号\"、反斜杠 \\、制表\t和换行\n的备注".to_owned(),
        );
        notes.insert("20261112-021736".to_owned(), String::new());

        save(&root, &notes).expect("保存应当成功");
        assert_eq!(load(&root), notes);

        // 落盘内容必须是合法可读的扁平 JSON
        let text = std::fs::read_to_string(note_path(&root)).expect("文件应当存在");
        assert!(text.starts_with('{') && text.trim_end().ends_with('}'), "{text}");
        assert!(text.contains("\"20261112-100008\": \"扫描前状态\""), "{text}");
        assert!(!text.contains(".tmp"), "临时文件不应残留：{text}");
        assert!(!root.join("snapshots").join("notes.json.tmp").exists());
    }

    /// 缺文件：返回空表，不报错、不 panic。
    #[test]
    fn missing_file_is_empty() {
        let root = unique_dir("missing");
        assert!(load(&root).is_empty());
        // 目录都不存在也一样
        assert!(load(&root.join("not-created")).is_empty());
    }

    /// 损坏文件：返回空表，不 panic（下次保存会整体覆盖成合法内容）。
    #[test]
    fn corrupt_file_is_empty() {
        let root = unique_dir("corrupt");
        let path = note_path(&root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for broken in [
            "{\"a\": }",
            "[1,2,3]",
            "{\"a\": 1}",
            "{\"a\": \"b\"",
            "not json at all",
            "",
            "\u{feff}{}",
        ] {
            std::fs::write(&path, broken).unwrap();
            let loaded = load(&root);
            assert!(
                loaded.is_empty(),
                "损坏内容 {broken:?} 不该读出任何键：{loaded:?}"
            );
        }
        // 落盘一个合法对象后能恢复
        let mut notes = HashMap::new();
        notes.insert("a".to_owned(), "b".to_owned());
        save(&root, &notes).expect("覆盖保存");
        assert_eq!(load(&root), notes);
    }

    /// 解析器接受别人手写的、键顺序不同的 JSON。
    #[test]
    fn parses_hand_written_json() {
        let parsed = parse_object("  {\"b\" : \"2\", \"a\":\"1\" ,\n \"c\":\"\"}  ").expect("应当解析成功");
        assert_eq!(parsed.get("a").map(String::as_str), Some("1"));
        assert_eq!(parsed.get("b").map(String::as_str), Some("2"));
        assert_eq!(parsed.get("c").map(String::as_str), Some(""));
        assert_eq!(parsed.len(), 3);
        assert!(parse_object("{\"a\":\"1\"} 垃圾").is_none(), "对象后面有多余内容要判损坏");
        assert!(parse_object("{\"a\":\"\\u4e2d\"}").is_some(), "支持 \\u 转义");
    }

    /// 路径口径：<data_root>\snapshots\notes.json。
    #[test]
    fn note_path_lives_in_snapshots_dir() {
        let root = Path::new("D:\\disk-intel-data");
        assert_eq!(
            note_path(root),
            PathBuf::from("D:\\disk-intel-data").join("snapshots").join("notes.json")
        );
    }
}
