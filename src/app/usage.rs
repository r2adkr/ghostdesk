//! Usage tab: what agents used, by day and by agent (data from `crate::usage`).

use std::collections::BTreeMap;

use super::*;
use crate::usage::{self, Counts};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Range {
    #[default]
    Week,
    Month,
    Quarter,
}

impl Range {
    fn days(self) -> usize {
        match self {
            Range::Week => 7,
            Range::Month => 30,
            Range::Quarter => 90,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Metric {
    #[default]
    Tokens,
    Work,
    Screens,
}

impl Metric {
    fn of(self, c: &Counts) -> u64 {
        match self {
            Metric::Tokens => c.tokens(),
            Metric::Work => c.work(),
            Metric::Screens => c.screens,
        }
    }

    fn color(self) -> Color32 {
        match self {
            Metric::Tokens => theme::info(),
            Metric::Work => theme::success(),
            Metric::Screens => theme::warn(),
        }
    }
}

#[derive(Default)]
pub(super) struct UsageUi {
    range: Range,
    metric: Metric,
    /// The user picked a chart themselves (until then it follows what there is data for)
    chosen: bool,
}

/// The last `n` days, oldest first, as "YYYY-MM-DD".
fn last_days(n: usize) -> Vec<String> {
    let now = usage::now_unix();
    (0..n as i64).rev().map(|i| usage::day_of(now - i * 86_400)).collect()
}

/// A round upper bound for the chart axis (1, 2, 5 × 10^k).
fn nice_max(v: u64) -> u64 {
    if v <= 4 {
        return 4;
    }
    let mut step = 1u64;
    while step * 10 <= v {
        step *= 10;
    }
    [1, 2, 5, 10].iter().map(|m| m * step).find(|m| *m >= v).unwrap_or(step * 10)
}

impl GhostApp {
    pub(super) fn usage_view(&mut self, ui: &mut egui::Ui) {
        let range = self.usage_ui.range;
        let days = last_days(range.days());
        let data = usage::global().since(days.first().map(String::as_str).unwrap_or(""));
        let per_day: Vec<(String, Counts)> = days
            .iter()
            .map(|d| {
                let mut c = Counts::default();
                for v in data.get(d).into_iter().flat_map(|m| m.values()) {
                    c.add(v);
                }
                (d.clone(), c)
            })
            .collect();
        let mut total = Counts::default();
        let mut by_source: BTreeMap<String, Counts> = BTreeMap::new();
        for (_, sources) in &data {
            for (name, c) in sources {
                total.add(c);
                by_source.entry(name.clone()).or_default().add(c);
            }
        }

        // with only outside agents' activity there are no tokens to chart, so show their actions until the user chooses
        if !self.usage_ui.chosen {
            self.usage_ui.metric = if total.tokens() == 0 && total.work() > 0 { Metric::Work } else { Metric::Tokens };
        }
        let metric = self.usage_ui.metric;
        let head_w = ui.available_width().min(960.0);
        ui.allocate_ui_with_layout(Vec2::new(head_w, 34.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.label(RichText::new(tr("사용량")).size(20.0).strong().color(theme::text()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (r, label) in [(Range::Quarter, tr("90일")), (Range::Month, tr("30일")), (Range::Week, tr("7일"))] {
                    if theme::tab(ui, self.usage_ui.range == r, label).clicked() {
                        self.usage_ui.range = r;
                    }
                }
            });
        });
        ui.add_space(12.0);

        egui::ScrollArea::vertical().id_salt("usage").auto_shrink([false, false]).show(ui, |ui| {
            let width = ui.available_width().min(960.0);
            ui.set_max_width(width);

            // summary tiles
            let tiles = [
                (tr("토큰"), usage::short_count(total.tokens()), trf("입력 {} · 출력 {}", &[&usage::short_count(total.in_tokens), &usage::short_count(total.out_tokens)])),
                (tr("동작"), usage::short_count(total.work()), trf("앱 안 에이전트 {} · 외부 {}", &[&usage::short_count(total.steps), &usage::short_count(total.actions)])),
                (tr("연 화면"), usage::short_count(total.screens), tr("외부 에이전트가 연 화면").to_string()),
                (
                    tr("아낀 토큰"),
                    format!("≈{}", usage::short_count(total.saved_tokens())),
                    trf("스크린샷 {}장 생략", &[&usage::short_count(total.shots_saved)]),
                ),
            ];
            let gap = 10.0;
            let w = ((width - gap * 3.0) / 4.0).floor();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for (label, value, sub) in &tiles {
                    theme::card_frame().inner_margin(Margin::symmetric(14, 12)).show(ui, |ui| {
                        ui.set_width(w - 28.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(theme::hint(*label));
                            ui.label(RichText::new(value).size(24.0).color(theme::text()));
                            ui.add(egui::Label::new(theme::hint(sub)).truncate());
                        });
                    });
                }
            });
            ui.add_space(14.0);

            // chart
            theme::card_frame().show(ui, |ui| {
                ui.set_width(width - 32.0);
                ui.horizontal(|ui| {
                    for (m, label) in [(Metric::Tokens, tr("토큰")), (Metric::Work, tr("동작")), (Metric::Screens, tr("연 화면"))] {
                        if theme::tab(ui, self.usage_ui.metric == m, label).clicked() {
                            self.usage_ui.metric = m;
                            self.usage_ui.chosen = true;
                        }
                    }
                    if metric == Metric::Tokens {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(theme::hint(tr("출력")));
                            theme::dot(ui, Metric::Tokens.color().gamma_multiply(0.45), 4.0);
                            ui.add_space(8.0);
                            ui.label(theme::hint(tr("입력")));
                            theme::dot(ui, Metric::Tokens.color(), 4.0);
                        });
                    }
                });
                ui.add_space(8.0);
                if per_day.iter().all(|(_, c)| metric.of(c) == 0) {
                    ui.add_space(46.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(tr("아직 기록이 없어요")).color(theme::text()));
                        ui.label(theme::hint(match metric {
                            Metric::Tokens => tr("앱 안의 에이전트가 일하면 쓴 토큰이 쌓여요"),
                            _ => tr("에이전트가 일하면 여기에 쌓여요"),
                        }));
                    });
                    ui.add_space(46.0);
                } else {
                    bar_chart(ui, &per_day, metric);
                }
            });
            ui.add_space(14.0);

            // by agent
            theme::card_frame().show(ui, |ui| {
                ui.set_width(width - 32.0);
                ui.label(theme::hint(tr("에이전트별")));
                ui.add_space(6.0);
                let mut rows: Vec<(&String, u64)> = by_source.iter().map(|(n, c)| (n, metric.of(c))).filter(|(_, v)| *v > 0).collect();
                rows.sort_by(|a, b| b.1.cmp(&a.1));
                if rows.is_empty() {
                    ui.label(theme::hint(tr("아직 기록이 없어요")));
                }
                let max = rows.first().map_or(1, |r| r.1).max(1);
                for (name, v) in rows.into_iter().take(10) {
                    ui.horizontal(|ui| {
                        let label_w = (ui.available_width() * 0.34).clamp(120.0, 260.0);
                        ui.allocate_ui_with_layout(Vec2::new(label_w, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(label_w);
                            ui.add(egui::Label::new(RichText::new(name).color(theme::text())).truncate()).on_hover_text(name);
                        });
                        let bar_w = (ui.available_width() - 64.0).max(20.0);
                        let (r, _) = ui.allocate_exact_size(Vec2::new(bar_w, 20.0), egui::Sense::hover());
                        let fill = egui::Rect::from_min_size(
                            egui::pos2(r.left(), r.center().y - 4.0),
                            Vec2::new((bar_w * v as f32 / max as f32).max(2.0), 8.0),
                        );
                        ui.painter().rect_filled(fill, 4, metric.color());
                        ui.label(RichText::new(usage::short_count(v)).monospace().color(theme::body()));
                    });
                }
            });
            ui.add_space(10.0);
            ui.label(theme::hint(tr("외부 에이전트(Claude Code, Codex 등)가 쓰는 토큰은 그 AI 서비스에서 확인하세요. GhostDesk 는 화면·동작·스크린샷만 세요.")));
            ui.add_space(8.0);
        });
    }
}

/// Bars per day with a round axis. Hover a bar for its numbers.
fn bar_chart(ui: &mut egui::Ui, days: &[(String, Counts)], metric: Metric) {
    let height = 200.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), egui::Sense::hover());
    let left = 44.0;
    let bottom = 22.0;
    let plot = egui::Rect::from_min_max(egui::pos2(rect.left() + left, rect.top() + 6.0), egui::pos2(rect.right() - 4.0, rect.bottom() - bottom));
    let max = nice_max(days.iter().map(|(_, c)| metric.of(c)).max().unwrap_or(0));
    let p = ui.painter_at(rect);
    let grid = theme::border();
    for i in 0..=2 {
        let y = plot.bottom() - plot.height() * i as f32 / 2.0;
        p.line_segment([egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)], Stroke::new(1.0, grid));
        p.text(
            egui::pos2(plot.left() - 8.0, y),
            egui::Align2::RIGHT_CENTER,
            usage::short_count(max * i / 2),
            egui::FontId::proportional(11.0),
            theme::muted(),
        );
    }
    let n = days.len().max(1);
    let slot = plot.width() / n as f32;
    let bar_w = (slot * 0.62).clamp(3.0, 28.0);
    let hover = resp.hover_pos().filter(|pos| plot.expand2(Vec2::new(0.0, 6.0)).contains(*pos)).map(|pos| (((pos.x - plot.left()) / slot) as usize).min(n - 1));
    let every = n.div_ceil(8).max(1);
    for (i, (day, c)) in days.iter().enumerate() {
        let cx = plot.left() + slot * (i as f32 + 0.5);
        if hover == Some(i) {
            p.rect_filled(egui::Rect::from_min_max(egui::pos2(cx - slot / 2.0, plot.top()), egui::pos2(cx + slot / 2.0, plot.bottom())), 4, theme::overlay_hover());
        }
        let h_of = |v: u64| plot.height() * v as f32 / max as f32;
        match metric {
            Metric::Tokens => {
                let (hi, ho) = (h_of(c.in_tokens), h_of(c.out_tokens));
                if c.tokens() > 0 {
                    let base = egui::Rect::from_min_max(egui::pos2(cx - bar_w / 2.0, plot.bottom() - hi), egui::pos2(cx + bar_w / 2.0, plot.bottom()));
                    p.rect_filled(base, 2, metric.color());
                    let top = egui::Rect::from_min_max(egui::pos2(cx - bar_w / 2.0, plot.bottom() - hi - ho), egui::pos2(cx + bar_w / 2.0, plot.bottom() - hi));
                    p.rect_filled(top, 2, metric.color().gamma_multiply(0.45));
                }
            }
            _ => {
                let v = metric.of(c);
                if v > 0 {
                    let r = egui::Rect::from_min_max(egui::pos2(cx - bar_w / 2.0, plot.bottom() - h_of(v).max(2.0)), egui::pos2(cx + bar_w / 2.0, plot.bottom()));
                    p.rect_filled(r, 2, metric.color());
                }
            }
        }
        if i % every == 0 || i + 1 == n {
            // "11-19"
            let label = day.get(5..).unwrap_or(day);
            p.text(egui::pos2(cx, plot.bottom() + 12.0), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(11.0), theme::muted());
        }
    }
    if let Some(i) = hover {
        let (day, c) = &days[i];
        let tip = match metric {
            Metric::Tokens => trf("{}\n입력 {} · 출력 {}", &[day, &usage::short_count(c.in_tokens), &usage::short_count(c.out_tokens)]),
            Metric::Work => trf("{}\n앱 안 에이전트 {} · 외부 {}", &[day, &usage::short_count(c.steps), &usage::short_count(c.actions)]),
            Metric::Screens => trf("{}\n연 화면 {}", &[day, &usage::short_count(c.screens)]),
        };
        resp.on_hover_text(tip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_maximum_is_round() {
        assert_eq!(nice_max(0), 4);
        assert_eq!(nice_max(7), 10);
        assert_eq!(nice_max(11), 20);
        assert_eq!(nice_max(34), 50);
        assert_eq!(nice_max(120_000), 200_000);
    }

    #[test]
    fn days_run_oldest_first() {
        let d = last_days(7);
        assert_eq!(d.len(), 7);
        assert!(d.windows(2).all(|w| w[0] < w[1]), "{d:?}");
    }
}
