#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod doc;
mod encoding;
mod index;
mod source;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontId, Key, Modifiers, Rect, Sense, Stroke, pos2, vec2};

use doc::{Action, Document};
use source::Repaint;

fn main() -> eframe::Result {
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RBigViewer")
            .with_inner_size([1100.0, 700.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "RBigViewer",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let repaint: Repaint = Arc::new(move || ctx.request_repaint());
            let mut app = App { docs: Vec::new(), active: 0, repaint, goto: String::new(), error: None };
            for f in files {
                app.open(f);
            }
            Ok(Box::new(app))
        }),
    )
}

/// На Windows используем Consolas — она есть в системе и поддерживает кириллицу.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in ["C:\\Windows\\Fonts\\consola.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("mono".into(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts.families.entry(egui::FontFamily::Monospace).or_default().insert(0, "mono".into());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

struct App {
    docs: Vec<Document>,
    active: usize,
    repaint: Repaint,
    goto: String,
    error: Option<String>,
}

impl App {
    fn open(&mut self, path: PathBuf) {
        if let Some(i) = self.docs.iter().position(|d| d.path == path) {
            self.active = i;
            return;
        }
        match Document::open(&path, self.repaint.clone()) {
            Ok(d) => {
                self.docs.push(d);
                self.active = self.docs.len() - 1;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{}: {e}", path.display())),
        }
    }

    fn open_dialog(&mut self) {
        if let Some(files) = rfd::FileDialog::new().pick_files() {
            for f in files {
                self.open(f);
            }
        }
    }

    fn close(&mut self, i: usize) {
        self.docs.remove(i);
        if self.active >= self.docs.len() {
            self.active = self.docs.len().saturating_sub(1);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Перетаскивание файлов в окно.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for p in dropped {
            self.open(p);
        }

        let mut goto_focus = false;
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::O)) {
            self.open_dialog();
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::W)) && !self.docs.is_empty() {
            self.close(self.active);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::G)) {
            goto_focus = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Tab)) && !self.docs.is_empty() {
            self.active = (self.active + 1) % self.docs.len();
        }

        egui::Panel::top("tabs").show(ui, |ui| self.tabs_bar(ui, goto_focus));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            if let Some(doc) = self.docs.get_mut(self.active) {
                text_view(ui, doc);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Перетащите файл в окно или нажмите Ctrl+O");
                });
            }
        });
    }
}

impl App {
    fn tabs_bar(&mut self, ui: &mut egui::Ui, goto_focus: bool) {
        ui.horizontal(|ui| {
            if ui.button("Открыть…").on_hover_text("Ctrl+O").clicked() {
                self.open_dialog();
            }
            ui.separator();
            let mut close = None;
            for (i, d) in self.docs.iter().enumerate() {
                let tab = ui.selectable_label(i == self.active, &d.name).on_hover_text(d.path.display().to_string());
                if tab.clicked() {
                    self.active = i;
                }
                if tab.middle_clicked() || ui.small_button("×").on_hover_text("Закрыть (Ctrl+W)").clicked() {
                    close = Some(i);
                }
            }
            if let Some(i) = close {
                self.close(i);
            }

            if let Some(doc) = self.docs.get_mut(self.active) {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let edit = ui.add(egui::TextEdit::singleline(&mut self.goto).desired_width(90.0).hint_text("номер"));
                    if goto_focus {
                        edit.request_focus();
                    }
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        let digits: String = self.goto.chars().filter(char::is_ascii_digit).collect();
                        if let Ok(n) = digits.parse::<u64>() {
                            doc.message = None;
                            doc.push(Action::GotoLine(n.max(1)));
                        }
                    }
                    ui.label("Строка (Ctrl+G):");
                });
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let Some(doc) = self.docs.get_mut(self.active) else {
                if let Some(e) = &self.error {
                    ui.colored_label(Color32::LIGHT_RED, e);
                }
                return;
            };
            let (lines, progress, complete) = doc.index_info();
            let cur = doc.line_number(doc.top).map(fmt_num).unwrap_or_else(|| "?".into());
            if complete {
                ui.label(format!("Стр. {cur} / {}", fmt_num(lines)));
            } else {
                ui.label(format!("Стр. {cur} / ≥{}", fmt_num(lines)));
                ui.add(egui::ProgressBar::new(progress).desired_width(120.0).text(format!("индекс {:.0}%", progress * 100.0)));
            }
            ui.separator();
            let size = doc.size();
            ui.label(format!("{}  ·  {:.1}%", fmt_size(size), if size == 0 { 0.0 } else { doc.top as f64 * 100.0 / size as f64 }));
            ui.separator();
            ui.checkbox(&mut doc.follow, "Следить за концом").on_hover_text("Как tail -f: при дописывании файла прокручивать вниз");
            if doc.is_busy() {
                ui.spinner();
            }
            if let Some(m) = &doc.message {
                ui.colored_label(Color32::from_rgb(230, 160, 60), m);
            }
            if let Some(e) = &self.error {
                ui.colored_label(Color32::LIGHT_RED, e);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut chosen = doc.encoding;
                egui::ComboBox::from_id_salt("encoding").selected_text(chosen.name()).show_ui(ui, |ui| {
                    for &enc in encoding::CHOICES.iter().chain(std::iter::once(&doc.detected)) {
                        let label = if enc == doc.detected { format!("{} (авто)", enc.name()) } else { enc.name().to_string() };
                        ui.selectable_value(&mut chosen, enc, label);
                    }
                });
                if chosen != doc.encoding {
                    doc.set_encoding(chosen);
                }
                ui.label("Кодировка:");
            });
        });
    }
}

fn text_view(ui: &mut egui::Ui, doc: &mut Document) {
    let font = FontId::monospace(14.0);
    let (row_h, char_w) = ui.ctx().fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, 'M')));
    let full = ui.available_rect_before_wrap();
    let rows = ((full.height() / row_h).floor() as usize).max(1);

    // Ввод: колесо, клавиатура.
    let view = ui.interact(full, ui.scope_id().with("view"), Sense::click_and_drag());
    if view.hovered() {
        let (delta, shift) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
        let (dx, dy) = if shift && delta.x == 0.0 { (delta.y, 0.0) } else { (delta.x, delta.y) };
        doc.scroll_acc -= dy / row_h;
        let whole = doc.scroll_acc.trunc();
        if whole != 0.0 {
            doc.scroll_acc -= whole;
            doc.push(Action::Scroll(whole as i64));
        }
        if dx != 0.0 {
            doc.hscroll = (doc.hscroll as f32 - dx / char_w).max(0.0) as usize;
        }
    }
    if !ui.ctx().egui_wants_keyboard_input() {
        let page = rows.saturating_sub(1).max(1) as i64;
        ui.input(|i| {
            if i.key_pressed(Key::ArrowDown) { doc.push(Action::Scroll(1)) }
            if i.key_pressed(Key::ArrowUp) { doc.push(Action::Scroll(-1)) }
            if i.key_pressed(Key::PageDown) { doc.push(Action::Scroll(page)) }
            if i.key_pressed(Key::PageUp) { doc.push(Action::Scroll(-page)) }
            if i.key_pressed(Key::ArrowRight) { doc.hscroll += 8 }
            if i.key_pressed(Key::ArrowLeft) { doc.hscroll = doc.hscroll.saturating_sub(8) }
            if i.modifiers.command && i.key_pressed(Key::Home) { doc.push(Action::Jump(0)) }
            else if i.key_pressed(Key::Home) { doc.hscroll = 0 }
            if i.modifiers.command && i.key_pressed(Key::End) { doc.push(Action::End) }
        });
    }

    doc.tick(rows);
    let (lines, loaded) = doc.visible_lines(rows);

    // Геометрия: номера строк | текст | полоса прокрутки.
    let first_no = lines.first().and_then(|l| doc.line_number(l.start));
    let digits = first_no.map(|n| fmt_num(n + rows as u64).chars().count()).unwrap_or(1).max(4);
    let gutter_w = char_w * digits as f32 + 12.0;
    let sb_w = 14.0;
    let gutter = Rect::from_min_size(full.min, vec2(gutter_w, full.height()));
    let text_rect = Rect::from_min_max(pos2(gutter.max.x + 6.0, full.min.y), pos2(full.max.x - sb_w, full.max.y));
    let sb = Rect::from_min_max(pos2(full.max.x - sb_w, full.min.y), full.max);

    let painter = ui.painter_at(full);
    let vis = ui.visuals();
    painter.rect_filled(full, 0.0, vis.extreme_bg_color);
    painter.rect_filled(gutter, 0.0, vis.faint_bg_color);
    let text_color = vis.text_color();
    let dim = vis.weak_text_color();

    let cols = (text_rect.width() / char_w).ceil() as usize + 1;
    let text_painter = ui.painter_at(text_rect);
    let mut no = first_no;
    let mut continuation = false;
    for (r, l) in lines.iter().enumerate() {
        let y = full.min.y + r as f32 * row_h;
        if continuation {
            // Продолжение строки, порезанной по 16 КБ.
            painter.text(pos2(gutter.max.x - 6.0, y), Align2::RIGHT_TOP, "↪", font.clone(), dim);
        } else if let Some(n) = no {
            painter.text(pos2(gutter.max.x - 6.0, y), Align2::RIGHT_TOP, fmt_num(n), font.clone(), dim);
        }
        let shown: String = l.text.chars().skip(doc.hscroll).take(cols).collect();
        text_painter.text(pos2(text_rect.min.x, y), Align2::LEFT_TOP, shown, font.clone(), text_color);
        if !l.split {
            no = no.map(|n| n + 1);
        }
        // Продолжение длинной строки номера не получает.
        continuation = l.split;
    }
    if !loaded {
        let y = full.min.y + lines.len() as f32 * row_h;
        painter.text(pos2(text_rect.min.x, y), Align2::LEFT_TOP, "… загрузка", font.clone(), dim);
    }

    // Полоса прокрутки по байтам.
    let size = doc.size().max(1);
    let sb_resp = ui.interact(sb, ui.scope_id().with("scrollbar"), Sense::click_and_drag());
    painter.rect_filled(sb, 0.0, vis.faint_bg_color);
    let shown_bytes = lines.last().map(|l| l.next - doc.top).unwrap_or(0);
    let thumb_h = (sb.height() * shown_bytes as f32 / size as f32).clamp(24.0, sb.height());
    let frac = doc.top as f32 / size as f32;
    let thumb_y = sb.min.y + frac * (sb.height() - thumb_h);
    let thumb = Rect::from_min_size(pos2(sb.min.x + 2.0, thumb_y), vec2(sb_w - 4.0, thumb_h));
    let thumb_color = if sb_resp.dragged() || sb_resp.hovered() { vis.widgets.active.bg_fill } else { vis.widgets.inactive.bg_fill };
    painter.rect_filled(thumb, 3.0, thumb_color);
    painter.line_segment([sb.left_top(), sb.left_bottom()], Stroke::new(1.0, vis.widgets.noninteractive.bg_stroke.color));
    if sb_resp.dragged() || sb_resp.clicked() {
        if let Some(p) = sb_resp.interact_pointer_pos() {
            let f = ((p.y - sb.min.y - thumb_h / 2.0) / (sb.height() - thumb_h)).clamp(0.0, 1.0);
            doc.push(Action::Jump((f as f64 * size as f64) as u64));
        }
    }
    ui.allocate_rect(full, Sense::hover());
}

fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{202F}');
        }
        out.push(c);
    }
    out
}

fn fmt_size(n: u64) -> String {
    let units = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} Б") } else { format!("{v:.2} {}", units[u]) }
}
