//! 矩形树图布局：长边优先的二分切分（严格照 Web 版 app.js:775-810，不是 squarified）。
//!
//! 规则（规格 5.2）：
//! 1. 输入已按 size 降序；权重 w_i = max(1, size_i)，0 字节项仍占 1，避免除零；
//! 2. 终止：空、w/h ≤ 0；仅 1 项 → 占满；depth ≥ 8 → 剩余项聚合为一块（面积守恒，绝不丢弃）；
//! 3. total = Σw，累加到第一个 ≥ total/2 的索引作为切点（两侧都必须非空）；
//! 4. 较长边切分；递归深度 +1。
//!
//! 复杂度 O(n log n)，同一输入必得同一矩形（无随机）。

use eframe::egui::{pos2, Rect};

/// 递归深度上限：到 8 层把剩余项并成「其他 N 项」，防止极深目录爆炸。
pub const MAX_DEPTH: usize = 8;

/// 权重：0 字节项仍占 1。
pub fn weight(size: i64) -> f64 {
    size.max(1) as f64
}

/// 一次聚合：自 start 起的 len 项共用一个矩形（画成「其他 N 项」）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aggregate {
    pub start: usize,
    pub len: usize,
    pub rect: Rect,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// 与输入等长；被聚合成一块的项（除第一项）为 Rect::NOTHING。
    pub rects: Vec<Rect>,
    pub aggregates: Vec<Aggregate>,
}

impl Layout {
    /// 所有矩形面积之和（用于守恒断言）。
    pub fn area(&self) -> f32 {
        self.rects.iter().map(rect_area).sum()
    }
}

pub fn rect_area(rect: &Rect) -> f32 {
    if !rect.is_positive() {
        return 0.0;
    }
    rect.width() * rect.height()
}

/// 对 weights（已按大小降序）在 rect 内布局。
pub fn layout(weights: &[f64], rect: Rect) -> Layout {
    let mut out = Layout {
        rects: vec![Rect::NOTHING; weights.len()],
        aggregates: Vec::new(),
    };
    if weights.is_empty() || rect.width() <= 0.5 || rect.height() <= 0.5 {
        return out;
    }
    if weights.iter().map(|w| w.max(0.0)).sum::<f64>() <= 0.0 {
        return out;
    }
    split(weights, 0..weights.len(), rect, 0, &mut out);
    out
}

fn split(
    weights: &[f64],
    range: std::ops::Range<usize>,
    rect: Rect,
    depth: usize,
    out: &mut Layout,
) {
    let n = range.len();
    if n == 0 || rect.width() <= 0.5 || rect.height() <= 0.5 {
        return;
    }
    if n == 1 {
        out.rects[range.start] = rect;
        return;
    }
    if depth >= MAX_DEPTH {
        // 聚合块：整段共用这一个矩形（面积守恒，绝不静默丢弃）。
        out.rects[range.start] = rect;
        out.aggregates.push(Aggregate {
            start: range.start,
            len: n,
            rect,
        });
        return;
    }

    let total: f64 = weights[range.clone()].iter().map(|w| w.max(0.0)).sum();
    if total <= 0.0 {
        return;
    }

    // 累加到第一个 ≥ total/2 的位置；至少 1 项、至多 n-1 项。
    let mut acc = 0.0;
    let mut half = 0usize;
    for (offset, w) in weights[range.clone()].iter().enumerate() {
        acc += w.max(0.0);
        if acc >= total / 2.0 {
            half = offset + 1;
            break;
        }
    }
    let half = half.clamp(1, n - 1);
    let left = range.start..range.start + half;
    let right = range.start + half..range.end;
    let left_total: f64 = weights[left.clone()].iter().map(|w| w.max(0.0)).sum();
    let ratio = (left_total / total) as f32;

    if rect.width() >= rect.height() {
        let split_x = (rect.left() + rect.width() * ratio)
            .clamp(rect.left() + f32::EPSILON, rect.right() - f32::EPSILON);
        split(
            weights,
            left,
            Rect::from_min_max(rect.min, pos2(split_x, rect.max.y)),
            depth + 1,
            out,
        );
        split(
            weights,
            right,
            Rect::from_min_max(pos2(split_x, rect.min.y), rect.max),
            depth + 1,
            out,
        );
    } else {
        let split_y = (rect.top() + rect.height() * ratio)
            .clamp(rect.top() + f32::EPSILON, rect.bottom() - f32::EPSILON);
        split(
            weights,
            left,
            Rect::from_min_max(rect.min, pos2(rect.max.x, split_y)),
            depth + 1,
            out,
        );
        split(
            weights,
            right,
            Rect::from_min_max(pos2(rect.min.x, split_y), rect.max),
            depth + 1,
            out,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() <= b.abs() * 0.005 + 0.5
    }

    #[test]
    fn area_is_conserved() {
        let cases: Vec<Vec<f64>> = vec![
            vec![100.0, 50.0, 25.0, 12.5, 6.0],
            vec![1e12, 3.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            vec![5.0],
            vec![9.0, 9.0],
            (0..200).map(|i| (200 - i) as f64).collect(),
            vec![0.0, 0.0, 3.0],
        ];
        for weights in cases {
            let rect = Rect::from_min_size(pos2(10.0, 20.0), eframe::egui::vec2(640.0, 360.0));
            let out = layout(&weights, rect);
            let area = out.area();
            assert!(approx(area, rect.width() * rect.height()), "面积不守恒: {area}");
            for r in &out.rects {
                assert!(r.min.x >= rect.min.x - 0.01 && r.max.x <= rect.max.x + 0.01);
                assert!(r.min.y >= rect.min.y - 0.01 && r.max.y <= rect.max.y + 0.01);
            }
        }
    }

    #[test]
    fn long_side_splits_first() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), eframe::egui::vec2(400.0, 200.0));
        let out = layout(&[60.0, 40.0], rect);
        let a = out.rects[0];
        let b = out.rects[1];
        assert!((a.top() - rect.top()).abs() < 0.01);
        assert!((b.top() - rect.top()).abs() < 0.01);
        assert!((a.width() / rect.width() - 0.6).abs() < 0.01);
    }

    #[test]
    fn single_tile_and_depth_aggregate() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), eframe::egui::vec2(100.0, 100.0));
        let out = layout(&[7.0], rect);
        assert_eq!(out.rects[0], rect);

        let weights: Vec<f64> = (0..64).map(|i| (64 - i) as f64).collect();
        let mut out = layout(&weights, rect);
        let mut covered = 0.0;
        for r in &out.rects {
            covered += rect_area(r);
        }
        assert!(approx(covered, 10_000.0));

        // 深度聚合：每层只有两项，递归深度会超过 8 层，剩余项必须被聚合成一块。
        let deep: Vec<f64> = vec![1e9, 5e8, 2e8, 1e8, 5e7, 2e7, 1e7, 5e6, 2e6, 1e6];
        out = layout(&deep, rect);
        assert!(!out.aggregates.is_empty(), "深度 8 必须产生聚合块");
        assert!(approx(out.area(), 10_000.0));
    }
}
