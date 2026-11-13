//! 检索条件解析：把 `ext:pdf size:>10m after:2026-01-01 kind:file path:D:\`
//! 这类内联条件与普通关键词拆开，产出可参数化的 SQL 片段。
//!
//! 契约：
//! ```ignore
//! let f = filter::parse("报告 ext:pdf size:>10m");
//! let (sql, params) = f.where_clause();   // sql 不含 WHERE 关键字
//! ```
//! 参数一律走 `?` 占位符，禁止把用户输入拼进 SQL 字符串。

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{NaiveDate, NaiveDateTime};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    All,
    File,
    Dir,
}

#[derive(Default, Clone)]
pub struct Filter {
    /// 普通关键词，全部 AND，匹配 path LIKE %term%。
    pub terms: Vec<String>,
    /// 扩展名（小写，无点）。
    pub exts: Vec<String>,
    pub min_size: Option<i64>,
    pub max_size: Option<i64>,
    /// Unix 秒。
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub kind: Option<Kind>,
    /// 路径前缀，限定范围。
    pub scope: Option<String>,
}

impl Filter {
    /// 是否没有任何条件（用于跳过 COUNT）。
    pub fn is_none(&self) -> bool {
        self.terms.is_empty()
            && self.exts.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.after.is_none()
            && self.before.is_none()
            && self.kind.is_none()
            && self.scope.is_none()
    }

    /// 生成 `WHERE` 之后的片段与参数。
    ///
    /// 要求：
    /// - 无任何条件时返回 `("1=1", vec![])`；
    /// - `terms` 逐个 `path LIKE ? ESCAPE '\'` 并 AND；
    /// - `exts` 用 `ext IN (?,?,…)`（小写）；
    /// - `min_size` → `size >= ?`，`max_size` → `size <= ?`；
    /// - `after` → `mtime >= <FILETIME>`（注意 entries.mtime 是 Windows FILETIME，
    ///   100ns since 1601，换算：`unix * 10_000_000 + 116_444_736_000_000_000`）；
    /// - `before` → `mtime <= <FILETIME>`；
    /// - `kind` → `is_dir = 0 / 1`；
    /// - `scope` → `(path = ? OR path LIKE ? ESCAPE '\')`，参数为
    ///   `scope` 与其 `scope + "\\%"`。
    pub fn where_clause(&self) -> (String, Vec<String>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<String> = Vec::new();

        for term in &self.terms {
            clauses.push("path LIKE ? ESCAPE '\\'".to_owned());
            params.push(format!("%{}%", escape_like(term)));
        }

        if !self.exts.is_empty() {
            let holes = vec!["?"; self.exts.len()].join(",");
            clauses.push(format!("ext IN ({holes})"));
            params.extend(self.exts.iter().cloned());
        }

        if let Some(size) = self.min_size {
            clauses.push("size >= ?".to_owned());
            params.push(size.to_string());
        }
        if let Some(size) = self.max_size {
            clauses.push("size <= ?".to_owned());
            params.push(size.to_string());
        }

        // mtime = 0 表示「未读到时间」，任何时间条件都要把它们排除掉。
        if self.after.is_some() || self.before.is_some() {
            let mut time_clauses = vec!["mtime > 0".to_owned()];
            // v3 快照里的 mtime 是 Unix 秒（Python 侧 EFU 导入时已换算），不是 FILETIME。
            // 历史实现按 FILETIME 换算，导致 after:/before: 永远匹配不到任何行（已修正）。
            if let Some(after) = self.after {
                time_clauses.push("mtime >= ?".to_owned());
                params.push(after.to_string());
            }
            if let Some(before) = self.before {
                time_clauses.push("mtime <= ?".to_owned());
                params.push(before.to_string());
            }
            clauses.push(format!("({})", time_clauses.join(" AND ")));
        }

        match self.kind {
            Some(Kind::File) => clauses.push("is_dir = 0".to_owned()),
            Some(Kind::Dir) => clauses.push("is_dir = 1".to_owned()),
            Some(Kind::All) | None => {}
        }

        if let Some(scope) = &self.scope {
            clauses.push("(path = ? OR path LIKE ? ESCAPE '\\')".to_owned());
            params.push(scope.clone());
            // 分隔符本身要转义：`D:\资料\\%` = 字面 `D:\资料\` + 任意后缀
            params.push(format!("{}\\\\%", escape_like(scope)));
        }

        if clauses.is_empty() {
            ("1=1".to_owned(), Vec::new())
        } else {
            (clauses.join(" AND "), params)
        }
    }
}

/// 解析查询串。要求：
/// - 空白分隔，双引号内视为一个整体；
/// - 识别 `ext:` / `size:` / `after:` / `before:` / `kind:` / `path:` 前缀；
/// - `size:` 支持 `>10m` `<1g` `1m..2g` 与裸数字（字节）；
///   单位 k/m/g/t 大小写不敏感；
/// - `after:` / `before:` 支持 `2026-01-01` 与 `2026-01-01T12:00`，
///   解析失败则忽略该条件（不要报错中断）；
/// - 未识别的 `xxx:yyy` 当作普通关键词。
pub fn parse(query: &str) -> Filter {
    let mut filter = Filter::default();
    for token in tokenize(query) {
        let Some((key, value)) = token.split_once(':') else {
            filter.terms.push(token);
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            filter.terms.push(token);
            continue;
        }
        match key.trim().to_ascii_lowercase().as_str() {
            "ext" => {
                // `ext:pdf,.xlsx` → ["pdf", "xlsx"]
                let before = filter.exts.len();
                for raw in value.split(',') {
                    let ext = raw.trim().trim_start_matches('.').to_ascii_lowercase();
                    if !ext.is_empty() && !filter.exts.contains(&ext) {
                        filter.exts.push(ext);
                    }
                }
                if filter.exts.len() == before {
                    filter.terms.push(token);
                }
            }
            "size" => {
                let (min, max) = parse_size_expr(value);
                if min.is_none() && max.is_none() {
                    filter.terms.push(token);
                    continue;
                }
                if let Some(min) = min {
                    filter.min_size = Some(match filter.min_size {
                        Some(old) => old.max(min),
                        None => min,
                    });
                }
                if let Some(max) = max {
                    filter.max_size = Some(match filter.max_size {
                        Some(old) => old.min(max),
                        None => max,
                    });
                }
            }
            "after" => {
                if let Some(unix) = parse_when(value) {
                    filter.after = Some(match filter.after {
                        Some(old) => old.max(unix),
                        None => unix,
                    });
                }
            }
            "before" => {
                if let Some(unix) = parse_when(value) {
                    filter.before = Some(match filter.before {
                        Some(old) => old.min(unix),
                        None => unix,
                    });
                }
            }
            "kind" => match value.to_ascii_lowercase().as_str() {
                "file" | "files" | "文件" => filter.kind = Some(Kind::File),
                "dir" | "dirs" | "folder" | "目录" | "夹" => filter.kind = Some(Kind::Dir),
                "all" | "全部" => filter.kind = Some(Kind::All),
                _ => filter.terms.push(token),
            },
            "path" => {
                let scope = normalize_scope(value);
                if scope.is_empty() {
                    filter.terms.push(token);
                } else {
                    filter.scope = Some(scope);
                }
            }
            _ => filter.terms.push(token),
        }
    }
    filter
}

/// 按空白切分，双引号内的空白不算分隔符，引号本身被去掉。
fn tokenize(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in query.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            _ if ch.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// 把 `2026-01-01` 之类转成 Unix 秒；失败返回 None。
pub fn parse_date(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(dt) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M") {
        return Some(dt.and_utc().timestamp());
    }
    let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

/// `after:` / `before:` 的取值：先试 `7days` 这类相对时间，再试日期。
fn parse_when(text: &str) -> Option<i64> {
    if let Some(seconds) = parse_relative(text) {
        return Some(now_unix() - seconds);
    }
    parse_date(text)
}

/// `7days` / `3d` / `12h` / `30m`（相对当前时间往回）。
fn parse_relative(text: &str) -> Option<i64> {
    let lower = text.trim().to_ascii_lowercase();
    let digits: String = lower.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let count: i64 = digits.parse().ok()?;
    let unit = lower[digits.len()..].trim();
    let per_unit = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" | "秒" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" | "分" => 60,
        "h" | "hour" | "hours" | "小时" => 3600,
        "d" | "day" | "days" | "天" => 86_400,
        "w" | "week" | "weeks" | "周" => 604_800,
        _ => return None,
    };
    count.checked_mul(per_unit)
}

/// 把 `10m` / `1g` / `512k` 转成字节；失败返回 None。
pub fn parse_size(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut body = text;
    // 允许 `10mb` / `1KB` 这类写法：先摘掉字节单位的 b
    if body.len() > 1 {
        if let Some(last) = body.chars().next_back() {
            if last == 'b' || last == 'B' {
                body = &body[..body.len() - last.len_utf8()];
            }
        }
    }
    if body.is_empty() {
        return None;
    }
    let (number, multiplier) = match body.chars().next_back() {
        Some(unit) if unit.is_ascii_alphabetic() => {
            let multiplier = match unit.to_ascii_lowercase() {
                'k' => 1024,
                'm' => 1024 * 1024,
                'g' => 1024 * 1024 * 1024,
                't' => 1024_i64.pow(4),
                _ => return None,
            };
            (&body[..body.len() - unit.len_utf8()], multiplier)
        }
        _ => (body, 1),
    };
    if number.is_empty() {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let bytes = value * multiplier as f64;
    if bytes > i64::MAX as f64 {
        return None;
    }
    Some(bytes.round() as i64)
}

/// `>10m` / `<1g` / `1m..2g` / `1024` → (min, max)。
fn parse_size_expr(text: &str) -> (Option<i64>, Option<i64>) {
    let text = text.trim();
    if let Some((low, high)) = text.split_once("..") {
        return (parse_size(low), parse_size(high));
    }
    for (prefix, is_min) in [(">=", true), ("<=", false), (">", true), ("<", false)] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let value = parse_size(rest);
            return if is_min { (value, None) } else { (None, value) };
        }
    }
    (parse_size(text), None)
}

/// 去掉路径末尾的分隔符，`D:\资料\` 与 `D:\资料` 等价。
fn normalize_scope(scope: &str) -> String {
    let trimmed = scope.trim().trim_end_matches(['\\', '/']);
    if trimmed.len() == 2 && trimmed.ends_with(':') {
        // 卷根 `D:` 保留为 `D:`，LIKE 参数会成为 `D:%`
        return trimmed.to_owned();
    }
    trimmed.to_owned()
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 当前时间戳，供默认 `after:` 使用。
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_terms_are_and_ed() {
        let f = parse("报告 2026");
        assert_eq!(f.terms, vec!["报告", "2026"]);
        let (sql, params) = f.where_clause();
        assert_eq!(
            sql,
            "path LIKE ? ESCAPE '\\' AND path LIKE ? ESCAPE '\\'"
        );
        assert_eq!(params, vec!["%报告%", "%2026%"]);
    }

    #[test]
    fn quoted_term_keeps_spaces() {
        let f = parse("\"我的 报告\" ext:pdf");
        assert_eq!(f.terms, vec!["我的 报告"]);
        assert_eq!(f.exts, vec!["pdf"]);
    }

    #[test]
    fn ext_multi_values_and_dots() {
        let f = parse("ext:PDF,.xlsx,docx");
        assert_eq!(f.exts, vec!["pdf", "xlsx", "docx"]);
        let (sql, params) = f.where_clause();
        assert_eq!(sql, "ext IN (?,?,?)");
        assert_eq!(params, vec!["pdf", "xlsx", "docx"]);
    }

    #[test]
    fn size_forms() {
        let f = parse("size:>10m");
        assert_eq!(f.min_size, Some(10 * 1024 * 1024));
        assert_eq!(f.max_size, None);

        let f = parse("size:<1g");
        assert_eq!(f.max_size, Some(1024 * 1024 * 1024));
        assert_eq!(f.min_size, None);

        let f = parse("size:1m..2g");
        assert_eq!(f.min_size, Some(1024 * 1024));
        assert_eq!(f.max_size, Some(2 * 1024 * 1024 * 1024));

        let f = parse("size:1024");
        assert_eq!(f.min_size, Some(1024));

        // 大小写不敏感
        assert_eq!(parse_size("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("512K"), Some(512 * 1024));
        assert_eq!(parse_size("1t"), Some(1024_i64.pow(4)));
        assert_eq!(parse_size("10mb"), Some(10 * 1024 * 1024));
        assert_eq!(parse_size("abc"), None);
    }

    #[test]
    fn date_becomes_filetime() {
        let f = parse("after:2026-01-01 before:2026-01-01T12:00");
        let unix = parse_date("2026-01-01").unwrap();
        assert_eq!(unix % 86_400, 0);
        let (sql, params) = f.where_clause();
        assert_eq!(sql, "(mtime > 0 AND mtime >= ? AND mtime <= ?)");
        assert_eq!(params[0], unix.to_string());
        assert_eq!(parse_date("2026-01-01T12:00"), Some(unix + 12 * 3600));
        assert_eq!(parse_date("2026-13-40"), None);
        assert_eq!(parse_date("不是日期"), None);
    }

    #[test]
    fn path_scope_prefix() {
        let f = parse("path:D:\\资料");
        assert_eq!(f.scope.as_deref(), Some("D:\\资料"));
        let (sql, params) = f.where_clause();
        assert_eq!(sql, "(path = ? OR path LIKE ? ESCAPE '\\')");
        // 反斜杠走 ESCAPE '\'，参数里成对出现：字面 `D:\资料\` + 通配 %
        assert_eq!(params, vec!["D:\\资料", "D:\\\\资料\\\\%"]);

        // 末尾分隔符被丢掉，不会出现 `D:\资料\\%`
        let f = parse("path:D:\\资料\\");
        assert_eq!(f.scope.as_deref(), Some("D:\\资料"));
    }

    #[test]
    fn kind_and_empty() {
        assert!(parse("").is_none());
        let (sql, params) = parse("").where_clause();
        assert_eq!((sql.as_str(), params.len()), ("1=1", 0));

        assert_eq!(parse("kind:file").kind, Some(Kind::File));
        assert_eq!(parse("kind:dir").kind, Some(Kind::Dir));
        let (sql, _) = parse("kind:file").where_clause();
        assert_eq!(sql, "is_dir = 0");
        let (sql, _) = parse("kind:dir").where_clause();
        assert_eq!(sql, "is_dir = 1");
    }

    #[test]
    fn illegal_input_does_not_panic() {
        let f = parse("size:abc after:xx before: kind: path: ext: ... .. >");
        // 非法 size/kind/ext 退回普通关键词，非法日期被忽略
        assert_eq!(f.min_size, None);
        assert_eq!(f.max_size, None);
        assert_eq!(f.before, None);
        assert_eq!(f.kind, None);
        assert_eq!(f.scope, None);
        assert!(f.exts.is_empty());
        assert!(f.terms.contains(&"size:abc".to_owned()));
        let (_sql, _params) = f.where_clause();
    }

    #[test]
    fn sql_injection_stays_in_params() {
        // 无空格的注入串仍是单个 token，必须原样进参数、不进 SQL
        let f = parse("ext:pdf')OR('1'='1");
        assert_eq!(f.exts, vec!["pdf')or('1'='1"]);
        let (sql, params) = f.where_clause();
        assert_eq!(sql, "ext IN (?)");
        assert!(!sql.contains("1=1"));
        assert_eq!(params, vec!["pdf')or('1'='1"]);

        // 带空格的注入串被切成多个词，同样只在参数里出现
        let f = parse("ext:pdf' OR 1=1--");
        let (sql, params) = f.where_clause();
        assert!(!sql.contains("1=1"));
        assert!(params.iter().any(|p| p.contains("pdf'")));
        assert!(params.iter().all(|p| !p.contains("OR 1")));

        // LIKE 通配符与转义符必须被转义
        let f = parse("50%_off");
        let (sql, params) = f.where_clause();
        assert_eq!(sql, "path LIKE ? ESCAPE '\\'");
        assert_eq!(params, vec!["%50\\%\\_off%"]);
    }

    #[test]
    fn full_query_snapshot() {
        let f = parse("报告 ext:pdf size:>10m after:2026-01-01 kind:file");
        let (sql, params) = f.where_clause();
        assert_eq!(
            sql,
            "path LIKE ? ESCAPE '\\' AND ext IN (?) AND size >= ? \
             AND (mtime > 0 AND mtime >= ?) AND is_dir = 0"
        );
        assert_eq!(params.len(), 4);
        assert_eq!(params[0], "%报告%");
        assert_eq!(params[1], "pdf");
        assert_eq!(params[2], (10 * 1024 * 1024).to_string());
        assert_eq!(
            params[3],
            parse_date("2026-01-01").unwrap().to_string()
        );
    }
}