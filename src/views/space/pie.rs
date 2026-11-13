//! 卷卡片上的分区饼图（环形）：已用 + 灰色空闲（规格 5.1）。
//!
//! 两处必须防的坑（Python 版实测）：
//! 1. 极小扇区（弧长 < 2 px）用多边形近似会退化成长条，改画一道径向刻线；
//! 2. 单个扇区超过 45° 时是凹多边形，凸多边形填充会错，按 45° 分块拼。

use eframe::egui::{Color32, Painter, Pos2, Shape, Stroke, Vec2};

/// 圆弧采样点：从 a0 到 a1（弧度）。
fn arc_points(center: Pos2, radius: f32, a0: f32, a1: f32) -> Vec<Pos2> {
    let steps = (((a1 - a0).abs() / 0.03).ceil() as usize).max(2);
    (0..=steps)
        .map(|i| {
            let t = a0 + (a1 - a0) * (i as f32 / steps as f32);
            center + Vec2::new(t.cos() * radius, t.sin() * radius)
        })
        .collect()
}

/// 一个扇区（角度制已转弧度），从 12 点方向顺时针。
fn sector(painter: &Painter, center: Pos2, radius: f32, a0: f32, a1: f32, color: Color32) {
    let sweep = a1 - a0;
    if sweep <= 0.0 || radius <= 0.0 {
        return;
    }
    if sweep * radius < 2.0 {
        let dir = Vec2::new(a0.cos(), a0.sin());
        painter.line_segment(
            [center + dir * (radius * 0.55), center + dir * radius],
            Stroke::new(1.5, color),
        );
        return;
    }
    let chunks = ((sweep / std::f32::consts::FRAC_PI_4).ceil() as usize).max(1);
    for slot in 0..chunks {
        let from = a0 + sweep * (slot as f32 / chunks as f32);
        let to = a0 + sweep * ((slot + 1) as f32 / chunks as f32);
        let mut points = vec![center];
        points.extend(arc_points(center, radius, from, to));
        painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
    }
}

/// 画环形饼图：slices 为（占比 0..1，颜色），不足 1 的部分用 free_color 补齐。
pub fn donut(
    painter: &Painter,
    center: Pos2,
    radius: f32,
    slices: &[(f32, Color32)],
    free_color: Option<Color32>,
    seam: Color32,
) {
    let mut start = -std::f32::consts::FRAC_PI_2;
    let mut used = 0.0f32;
    for (share, color) in slices {
        let share = share.clamp(0.0, 1.0);
        if share <= 0.0 {
            continue;
        }
        let end = start + share * std::f32::consts::TAU;
        sector(painter, center, radius, start, end, *color);
        start = end;
        used += share;
    }
    if used < 0.9995
        && let Some(color) = free_color
    {
        let end = start + (1.0 - used) * std::f32::consts::TAU;
        sector(painter, center, radius, start, end, color);
    }
    // 白缝：外圈 + 每个扇区的分界由绘制顺序自然形成，这里只描外圈。
    painter.circle_stroke(center, radius, Stroke::new(1.0, seam));
}

/// 环形中心的白/面板色圆（挖孔）。
pub fn punch(painter: &Painter, center: Pos2, inner: f32, fill: Color32) {
    painter.circle_filled(center, inner, fill);
}
