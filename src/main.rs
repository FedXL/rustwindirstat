mod model;
mod scanner;
mod treemap;

use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Stroke, Vec2};
use model::Node;
use scanner::{ScanEvent, ScanResult, scan_async};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

fn main() -> eframe::Result {
    let start_path = std::env::args()
        .skip_while(|arg| arg != "--scan")
        .nth(1)
        .unwrap_or_else(|| "C:\\".to_owned());
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 850.0])
            .with_maximized(true)
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Windirstat RS",
        options,
        Box::new(move |cc| Ok(Box::new(DiskApp::new(cc, start_path)))),
    )
}

struct DiskApp {
    root: String,
    search: String,
    last_search: String,
    search_results: Vec<usize>,
    scan_rx: Option<Receiver<ScanEvent>>,
    cancel: Option<Arc<AtomicBool>>,
    result: Option<ScanResult>,
    selected: usize,
    status: String,
    progress_files: u64,
    progress_total: Option<u64>,
    progress_bytes: u64,
    scan_phase: String,
    error: Option<String>,
    confirm_delete: bool,
    delete_target: Option<PathBuf>,
    admin_offer: bool,
    expanded: HashSet<usize>,
    scroll_to_selected: bool,
    sort_column: SortColumn,
    sort_descending: bool,
    render_generation: Arc<std::sync::atomic::AtomicU64>,
    render_rx: Option<Receiver<(u64, [usize; 2], treemap::RenderedMap)>>,
    render_tx: Sender<(u64, [usize; 2], treemap::RenderedMap)>,
    render_running: Arc<AtomicBool>,
    render_progress: Option<Arc<treemap::RenderProgress>>,
    render_size: [usize; 2],
    render_requested_size: [usize; 2],
    render_finished_size: [usize; 2],
    render_requested_at: std::time::Instant,
    map_texture: Option<egui::TextureHandle>,
    map_hits: Vec<u32>,
    map_rects: Vec<treemap::PackedRect>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Name,
    Percent,
    Size,
    Items,
    Files,
    Folders,
    Modified,
    Attributes,
}

const TREE_COLUMN_WIDTHS: [f32; 8] = [420.0, 125.0, 105.0, 85.0, 80.0, 90.0, 160.0, 65.0];
const TREE_ROW_HEIGHT: f32 = 28.0;
const TREE_CONTENT_WIDTH: f32 = 1130.0;

impl DiskApp {
    fn new(cc: &eframe::CreationContext<'_>, root: String) -> Self {
        let (render_tx, render_rx) = std::sync::mpsc::channel();
        cc.egui_ctx.set_theme(egui::ThemePreference::Light);
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "segoe_ui".to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
                r"C:\Windows\Fonts\segoeui.ttf"
            ))),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe_ui".to_owned());
        cc.egui_ctx.set_fonts(fonts);
        let mut style = (*cc.egui_ctx.style_of(egui::Theme::Light)).clone();
        style.spacing.item_spacing = Vec2::new(5.0, 3.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, FontId::proportional(16.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(12.0));
        style.visuals.panel_fill = Color32::from_rgb(240, 240, 240);
        style.visuals.window_fill = Color32::from_rgb(245, 245, 245);
        style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(240, 240, 240);
        cc.egui_ctx.set_style_of(egui::Theme::Light, style);
        Self {
            root,
            search: String::new(),
            last_search: String::new(),
            search_results: Vec::new(),
            scan_rx: None,
            cancel: None,
            result: None,
            selected: 0,
            status: "Укажите диск или папку и нажмите «Сканировать». Для быстрого сканирования NTFS запустите программу от имени администратора.".to_owned(),
            progress_files: 0,
            progress_total: None,
            progress_bytes: 0,
            scan_phase: String::new(),
            error: None,
            confirm_delete: false,
            delete_target: None,
            admin_offer: false,
            expanded: HashSet::from([0]),
            scroll_to_selected: false,
            sort_column: SortColumn::Size,
            sort_descending: true,
            render_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            render_rx: Some(render_rx), render_tx, render_size: [0, 0], render_requested_size: [0, 0],
            render_running: Arc::new(AtomicBool::new(false)), render_progress: None, render_finished_size: [0,0],
            render_requested_at: std::time::Instant::now(), map_texture: None, map_hits: Vec::new(), map_rects: Vec::new(),
        }
    }

    fn start_scan(&mut self) {
        let root = PathBuf::from(self.root.trim());
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        scan_async(root, tx, cancel.clone());
        self.scan_rx = Some(rx);
        self.cancel = Some(cancel);
        self.result = None;
        self.invalidate_map();
        self.selected = 0;
        self.expanded.clear();
        self.expanded.insert(0);
        self.scroll_to_selected = false;
        self.progress_files = 0;
        self.progress_total = None;
        self.progress_bytes = 0;
        self.error = None;
        self.admin_offer = false;
        self.scan_phase = "Подготовка сканирования".to_owned();
        self.status = self.scan_phase.clone();
    }

    fn invalidate_map(&mut self) {
        self.render_generation.fetch_add(1, Ordering::Relaxed);
        self.render_size = [0, 0];
        self.render_requested_size = [0, 0];
        self.render_finished_size = [0, 0];
        self.map_texture = None;
        self.map_hits.clear();
        self.map_rects.clear();
        self.render_progress = None;
    }

    fn poll_map(&mut self, ctx: &egui::Context, size: [usize; 2]) {
        if let Some(rx) = &self.render_rx {
            while let Ok((generation, requested_size, map)) = rx.try_recv() {
                if generation == self.render_generation.load(Ordering::Relaxed) {
                    let actual_size = map.image.size;
                    if let Some(texture) = &mut self.map_texture {
                        texture.set(map.image, egui::TextureOptions::LINEAR);
                    } else {
                        self.map_texture = Some(ctx.load_texture(
                            "treemap",
                            map.image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                    self.render_size = actual_size;
                    self.render_finished_size = requested_size;
                    self.map_hits = map.hits;
                    self.map_rects = map.rects;
                }
            }
        }
        if self.result.is_none() || size[0] == 0 || size[1] == 0 {
            return;
        }
        if size != self.render_requested_size {
            self.render_generation.fetch_add(1, Ordering::Relaxed);
            self.render_progress = None;
            self.render_requested_size = size;
            self.render_requested_at = std::time::Instant::now();
        }
        let limit_reached = self
            .render_progress
            .as_ref()
            .is_some_and(|p| p.snapshot().0 == treemap::PHASE_LIMIT);
        if self.render_finished_size != size
            && self.render_requested_size == size
            && !limit_reached
            && self.render_requested_at.elapsed() > Duration::from_millis(140)
            && self
                .render_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            let generation = self.render_generation.fetch_add(1, Ordering::Relaxed) + 1;
            let nodes = self.result.as_ref().unwrap().nodes.clone();
            let tx = self.render_tx.clone();
            let cancel = self.render_generation.clone();
            let running = self.render_running.clone();
            let progress = Arc::new(treemap::RenderProgress::default());
            self.render_progress = Some(progress.clone());
            let requested_size = size;
            let (w, h) = (size[0], size[1]);
            std::thread::spawn(move || {
                struct RunningGuard(Arc<AtomicBool>);
                impl Drop for RunningGuard {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _guard = RunningGuard(running);
                if let Some(map) =
                    treemap::render(nodes, w, h, cancel.clone(), generation, progress)
                {
                    if cancel.load(Ordering::Relaxed) == generation {
                        let _ = tx.send((generation, requested_size, map));
                    }
                }
            });
        }
        let terminal_limit = self
            .render_progress
            .as_ref()
            .is_some_and(|p| p.snapshot().0 == treemap::PHASE_LIMIT);
        if self.render_running.load(Ordering::Relaxed)
            || (self.render_finished_size != size && !terminal_limit)
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn poll_scan(&mut self) {
        let mut finished = false;
        if let Some(rx) = &self.scan_rx {
            loop {
                match rx.try_recv() {
                    Ok(ScanEvent::Progress {
                        files,
                        total,
                        bytes,
                        phase,
                    }) => {
                        self.progress_files = files;
                        self.progress_total = total;
                        self.progress_bytes = bytes;
                        self.scan_phase = phase;
                    }
                    Ok(ScanEvent::MftFallback {
                        reason,
                        elevation_required,
                    }) => {
                        self.admin_offer = elevation_required;
                        self.scan_phase = "Обход папок".to_owned();
                        self.status = format!("MFT недоступна: {reason}. Перехожу к обходу папок.");
                    }
                    Ok(ScanEvent::Finished(result)) => {
                        finished = true;
                        match result {
                            Ok(result) => {
                                let elapsed = result.elapsed.as_secs_f64();
                                self.status = format!(
                                    "{}  •  {} файлов  •  {}  •  {:.2} с{}",
                                    result.mode.label(),
                                    result.files,
                                    human_size(result.nodes.first().map_or(0, |node| node.size)),
                                    elapsed,
                                    if self.admin_offer {
                                        "  •  Запустите от администратора для ускорения NTFS"
                                    } else {
                                        ""
                                    }
                                );
                                self.expanded.clear();
                                self.expanded.insert(0);
                                self.result = Some(result);
                            }
                            Err(error) => {
                                self.admin_offer = error.to_ascii_lowercase().contains("access")
                                    || error.to_ascii_lowercase().contains("folder")
                                    || error.to_ascii_lowercase().contains("папк");
                                self.status = error.clone();
                                self.error = Some(error);
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            self.scan_rx = None;
            self.cancel = None;
        }
    }

    fn refresh_search(&mut self) {
        if self.search == self.last_search {
            return;
        }
        self.last_search.clone_from(&self.search);
        self.search_results.clear();
        let query = self.search.trim().to_lowercase();
        if query.is_empty() {
            return;
        }
        if let Some(result) = &self.result {
            self.search_results = result
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| node.path.to_string_lossy().to_lowercase().contains(&query))
                .map(|(index, _)| index)
                .collect();
            self.search_results
                .sort_by(|a, b| result.nodes[*b].size.cmp(&result.nodes[*a].size));
            self.search_results.truncate(500);
        }
    }

    fn selected_path(&self) -> Option<&std::path::Path> {
        self.result
            .as_ref()
            .and_then(|r| r.nodes.get(self.selected))
            .map(|node| node.path.as_path())
    }

    fn select_treemap_node(&mut self, index: usize) {
        let Some(nodes) = self.result.as_ref().map(|result| result.nodes.clone()) else {
            return;
        };
        if index >= nodes.len() {
            return;
        }
        self.selected = index;
        self.search.clear();
        let mut ancestor = nodes[index].parent;
        while let Some(parent) = ancestor {
            self.expanded.insert(parent);
            ancestor = nodes[parent].parent;
        }
        self.scroll_to_selected = true;
    }

    fn open_selected(&self) {
        if let Some(path) = self.selected_path() {
            #[cfg(windows)]
            {
                let mut command = std::process::Command::new("explorer.exe");
                if path.is_file() {
                    command.arg(format!("/select,{}", path.display()));
                } else {
                    command.arg(path);
                }
                let _ = command.spawn();
            }
        }
    }
}

impl eframe::App for DiskApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_scan();
        self.refresh_search();
        let is_scanning = self.scan_rx.is_some();
        let map_progress = self.render_progress.as_ref().map(|p| p.snapshot());
        let mut start = false;
        let mut cancel = false;
        let mut open_selected = false;
        let mut delete_selected = false;

        egui::Panel::top("menubar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("Файл", |ui| {
                    if ui.button("Сканировать").clicked() {
                        start = true;
                        ui.close();
                    }
                    if ui.button("Выход").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Редактировать", |ui| {
                    if ui.button("Копировать путь выбранного объекта").clicked()
                    {
                        if let Some(path) = self.selected_path() {
                            ui.ctx().copy_text(path.display().to_string());
                        }
                        ui.close();
                    }
                });
                ui.menu_button("Очистка", |ui| {
                    if ui
                        .add_enabled(
                            self.selected != 0 && self.result.is_some(),
                            egui::Button::new("Удалить выбранный объект…"),
                        )
                        .clicked()
                    {
                        if let Some(path) = self.selected_path() {
                            self.delete_target = Some(path.to_path_buf());
                            self.confirm_delete = true;
                        }
                        ui.close();
                    }
                });
                ui.menu_button("Структура каталогов", |ui| {
                    if ui.button("Развернуть корень").clicked() {
                        self.expanded.insert(0);
                        ui.close();
                    }
                    if ui.button("Свернуть всё").clicked() {
                        self.expanded.clear();
                        self.expanded.insert(0);
                        ui.close();
                    }
                });
                ui.menu_button("Отчёт", |ui| {
                    ui.label("Сводка сканирования находится в строке состояния.");
                });
                ui.menu_button("Настройки", |ui| {
                    ui.label("Классическая светлая тема");
                });
                ui.menu_button("Помощь", |ui| {
                    ui.label("WindirStat RS");
                });
            });
        });

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Путь:");
                ui.add(egui::TextEdit::singleline(&mut self.root).desired_width(340.0));
                if ui
                    .add_enabled(!is_scanning, egui::Button::new("Сканировать"))
                    .clicked()
                {
                    start = true;
                }
                if ui
                    .add_enabled(is_scanning, egui::Button::new("Остановить"))
                    .clicked()
                {
                    cancel = true;
                }
                if ui.button("Открыть").clicked() {
                    open_selected = true;
                }
                if ui
                    .add_enabled(
                        self.selected != 0 && !is_scanning,
                        egui::Button::new("Удалить…"),
                    )
                    .clicked()
                {
                    delete_selected = true;
                }
                if self.admin_offer {
                    #[cfg(windows)]
                    if ui.button("Перезапустить от администратора").clicked()
                    {
                        match scanner::relaunch_elevated(PathBuf::from(self.root.trim()).as_path())
                        {
                            Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                            Err(error) => self.status = error,
                        }
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Поиск:");
                ui.add(egui::TextEdit::singleline(&mut self.search).desired_width(340.0));
                ui.label("по имени или пути");
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                if is_scanning {
                    ui.separator();
                    ui.label(format!(
                        "{} · {}",
                        self.scan_phase,
                        human_size(self.progress_bytes)
                    ));
                } else if let Some((phase, _, _)) = map_progress.filter(|(phase, _, _)| {
                    *phase == treemap::PHASE_PREPARE || *phase == treemap::PHASE_LAYOUT
                }) {
                    ui.separator();
                    ui.label(if phase == treemap::PHASE_PREPARE {
                        "Подготовка treemap"
                    } else {
                        "Раскладка плиток"
                    });
                } else if map_progress.is_some_and(|(phase, _, _)| phase == treemap::PHASE_LIMIT) {
                    ui.separator();
                    ui.label("Treemap: превышен лимит временной памяти");
                }
            });
            if is_scanning {
                let fraction = self.progress_total.map(|total| {
                    if total == 0 {
                        0.0
                    } else {
                        (self.progress_files as f32 / total as f32).clamp(0.0, 1.0)
                    }
                });
                let bar = match fraction {
                    Some(value) => egui::ProgressBar::new(value)
                        .text(format!("{} записей", self.progress_files)),
                    None => egui::ProgressBar::new(0.25)
                        .animate(true)
                        .text(format!("{} файлов", self.progress_files)),
                };
                ui.add(bar);
            } else if let Some((phase, done, total)) = map_progress.filter(|(phase, _, _)| {
                *phase == treemap::PHASE_PREPARE || *phase == treemap::PHASE_LAYOUT
            }) {
                let fraction = if total == 0 {
                    0.0
                } else {
                    (done as f32 / total as f32).clamp(0.0, 1.0)
                };
                let label = if phase == treemap::PHASE_PREPARE {
                    "Подготовка treemap"
                } else {
                    "Раскладка плиток"
                };
                ui.add(egui::ProgressBar::new(fraction).text(format!("{label}: {done}/{total}")));
            } else if self.result.is_some() {
                // Keep the status panel's height fixed while the map worker starts,
                // finishes, or is restarted for a resized viewport.
                ui.add(egui::ProgressBar::new(1.0).text("Treemap ready"));
            }
        });

        egui::Panel::top("directory_table_v2")
            .resizable(true)
            .default_size(ui.available_height() * 0.46)
            .min_size(170.0)
            .show(ui, |ui| {
                if let Some(result) = &self.result {
                    let rows = if self.search.trim().is_empty() {
                        collect_visible_rows(
                            &result.nodes,
                            &self.expanded,
                            self.sort_column,
                            self.sort_descending,
                        )
                    } else {
                        self.search_results
                            .iter()
                            .map(|&index| (index, 0))
                            .collect()
                    };
                    let scroll_row = if self.scroll_to_selected {
                        rows.iter().position(|(index, _)| *index == self.selected)
                    } else {
                        None
                    };
                    let mut clicked_sort = None;
                    egui::ScrollArea::horizontal().show(ui, |ui| {
                        ui.set_min_width(TREE_CONTENT_WIDTH);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            sort_header(
                                ui,
                                "Имя",
                                420.0,
                                SortColumn::Name,
                                &self.sort_column,
                                self.sort_descending,
                                &mut clicked_sort,
                            );
                            sort_header(
                                ui,
                                "Доля",
                                125.0,
                                SortColumn::Percent,
                                &self.sort_column,
                                self.sort_descending,
                                &mut clicked_sort,
                            );
                            sort_header(
                                ui,
                                "Размер",
                                105.0,
                                SortColumn::Size,
                                &self.sort_column,
                                self.sort_descending,
                                &mut clicked_sort,
                            );
                            sort_header(
                                ui,
                                "Элементы",
                                85.0,
                                SortColumn::Items,
                                &self.sort_column,
                                self.sort_descending,
                                &mut clicked_sort,
                            );
                            sort_header(
                                ui,
                                "Файлы",
                                80.0,
                                SortColumn::Files,
                                &self.sort_column,
                                self.sort_descending,
                                &mut clicked_sort,
                            );
                            sort_header(
                                ui,
                                "Папки",
                                90.0,
                                SortColumn::Folders,
                                &self.sort_column,
                                self.sort_descending,
                                &mut clicked_sort,
                            );
                            sort_header(
                                ui,
                                "Изменён",
                                160.0,
                                SortColumn::Modified,
                                &self.sort_column,
                                self.sort_descending,
                                &mut clicked_sort,
                            );
                            sort_header(
                                ui,
                                "Атрибуты",
                                65.0,
                                SortColumn::Attributes,
                                &self.sort_column,
                                self.sort_descending,
                                &mut clicked_sort,
                            );
                        });
                        if rows.is_empty() {
                            ui.label(if self.search.trim().is_empty() {
                                "Результаты сканирования появятся здесь."
                            } else {
                                "Совпадений нет."
                            });
                        } else {
                            let mut row_scroll = egui::ScrollArea::vertical();
                            if let Some(row) = scroll_row {
                                let visible_rows =
                                    (ui.available_height() / TREE_ROW_HEIGHT).max(1.0);
                                let centered_row = (row as f32 - visible_rows * 0.5).max(0.0);
                                row_scroll = row_scroll
                                    .vertical_scroll_offset(centered_row * TREE_ROW_HEIGHT);
                            }
                            row_scroll.show_rows(ui, TREE_ROW_HEIGHT, rows.len(), |ui, range| {
                                for row in range {
                                    let (index, depth) = rows[row];
                                    let node = &result.nodes[index];
                                    let (clicked, toggled) = draw_directory_row(
                                        ui,
                                        node,
                                        index,
                                        depth,
                                        result.nodes[0].size,
                                        self.selected == index,
                                        self.expanded.contains(&index),
                                    );
                                    if clicked {
                                        self.selected = index;
                                    }
                                    if toggled && !self.expanded.remove(&index) {
                                        self.expanded.insert(index);
                                    }
                                }
                            });
                            if scroll_row.is_some() {
                                self.scroll_to_selected = false;
                            }
                        }
                    });
                    if let Some(column) = clicked_sort {
                        if self.sort_column == column {
                            self.sort_descending = !self.sort_descending;
                        } else {
                            self.sort_column = column;
                            self.sort_descending = column != SortColumn::Name;
                        }
                    }
                } else {
                    ui.label("Просканируйте диск или папку, чтобы увидеть содержимое.");
                }
            });
        egui::CentralPanel::default().show(ui, |ui| {
            if self.result.is_some() {
                let nodes = self.result.as_ref().unwrap().nodes.clone();
                let available = ui.available_size().max(Vec2::splat(1.0));
                let (rect, response) = ui.allocate_exact_size(available, Sense::click());
                let ppp = ui.ctx().pixels_per_point();
                let physical = [
                    (rect.width() * ppp).round().max(1.0) as usize,
                    (rect.height() * ppp).round().max(1.0) as usize,
                ];
                self.poll_map(ui.ctx(), physical);
                if let Some(texture) = &self.map_texture {
                    ui.painter().image(
                        texture.id(),
                        rect,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    if response.hovered() {
                        if let Some(pointer) = response.hover_pos() {
                            let x = ((pointer.x - rect.left()) / rect.width()
                                * self.render_size[0] as f32)
                                .floor()
                                .max(0.0) as usize;
                            let y = ((pointer.y - rect.top()) / rect.height()
                                * self.render_size[1] as f32)
                                .floor()
                                .max(0.0) as usize;
                            if let Some(&hit) = self.map_hits.get(y * self.render_size[0] + x) {
                                if hit != u32::MAX {
                                    let node = &nodes[hit as usize];
                                    response.clone().on_hover_text(format!(
                                        "{}\n{}",
                                        node.path.display(),
                                        human_size(node.size)
                                    ));
                                    if response.clicked() {
                                        self.select_treemap_node(hit as usize);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(r) = self.map_rects.get(self.selected) {
                        let sx = rect.width() / self.render_size[0] as f32;
                        let sy = rect.height() / self.render_size[1] as f32;
                        let selected_rect = Rect::from_min_size(
                            egui::pos2(rect.left() + r.x as f32 * sx, rect.top() + r.y as f32 * sy),
                            Vec2::new(r.w as f32 * sx, r.h as f32 * sy),
                        );
                        ui.painter().rect_stroke(
                            selected_rect,
                            0.0,
                            Stroke::new(2.0, Color32::WHITE),
                            egui::StrokeKind::Inside,
                        );
                    }
                } else {
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "Rendering treemap…",
                        FontId::proportional(15.0),
                        Color32::DARK_GRAY,
                    );
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        "Выберите диск или папку, чтобы увидеть распределение занятого места.",
                    );
                });
            }
        });

        if start {
            self.start_scan();
        }
        if cancel {
            if let Some(cancel) = &self.cancel {
                cancel.store(true, Ordering::Relaxed);
            }
            self.status = "Останавливаю сканирование…".to_owned();
        }
        if open_selected {
            self.open_selected();
        }
        if delete_selected {
            if let Some(path) = self.selected_path() {
                self.delete_target = Some(path.to_path_buf());
                self.confirm_delete = true;
            }
        }

        if self.confirm_delete {
            egui::Window::new("Подтверждение удаления")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    let path = self.delete_target.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                    ui.label("Удалить выбранный объект с диска?");
                    ui.label(path);
                    ui.horizontal(|ui| {
                        if ui.button("Удалить навсегда").clicked() {
                            if let Some(path) = self.delete_target.take() {
                                let deleted = if path.is_dir() {
                                    std::fs::remove_dir_all(&path)
                                } else {
                                    std::fs::remove_file(&path)
                                };
                                match deleted {
                                    Ok(()) => {
                                        self.status = "Объект удалён. Просканируйте путь ещё раз, чтобы обновить результаты.".to_owned();
                                        self.result = None;
                                    }
                                    Err(error) => self.status = format!("Ошибка удаления: {error}"),
                                }
                            }
                            self.confirm_delete = false;
                        }
                        if ui.button("Отмена").clicked() {
                            self.confirm_delete = false;
                            self.delete_target = None;
                        }
                    });
                });
        }

        if let Some(error) = self.error.clone() {
            egui::Window::new("Ошибка сканирования")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(error);
                    if self.admin_offer {
                        ui.label("Быстрое чтение MFT требует прав администратора.");
                        #[cfg(windows)]
                        if ui.button("Перезапустить от администратора").clicked()
                        {
                            match scanner::relaunch_elevated(
                                PathBuf::from(self.root.trim()).as_path(),
                            ) {
                                Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                                Err(error) => self.status = error,
                            }
                        }
                    }
                    if ui.button("Закрыть").clicked() {
                        self.error = None;
                    }
                });
        }

        if is_scanning {
            ui.ctx().request_repaint_after(Duration::from_millis(80));
        }
    }
}

fn sort_header(
    ui: &mut egui::Ui,
    title: &str,
    width: f32,
    column: SortColumn,
    current: &SortColumn,
    descending: bool,
    clicked: &mut Option<SortColumn>,
) {
    let marker = if column == *current {
        if descending {
            "  \u{25bc}"
        } else {
            "  \u{25b2}"
        }
    } else {
        ""
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, Color32::from_rgb(242, 242, 242));
    painter.text(
        rect.left_center() + Vec2::new(6.0, 0.0),
        Align2::LEFT_CENTER,
        format!("{title}{marker}"),
        FontId::proportional(14.0),
        Color32::from_gray(35),
    );
    if response.clicked() {
        *clicked = Some(column);
    }
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, Color32::from_gray(185)),
    );
    painter.line_segment(
        [rect.right_top(), rect.right_bottom()],
        Stroke::new(1.0, Color32::from_gray(215)),
    );
}

fn draw_directory_row(
    ui: &mut egui::Ui,
    node: &Node,
    index: usize,
    depth: usize,
    total_size: u64,
    selected: bool,
    expanded: bool,
) -> (bool, bool) {
    let width = ui.available_width().max(TREE_CONTENT_WIDTH);
    let (row, response) = ui.allocate_exact_size(Vec2::new(width, TREE_ROW_HEIGHT), Sense::click());
    let painter = ui.painter();
    let row_fill = if selected {
        Color32::from_rgb(210, 228, 247)
    } else if response.hovered() {
        Color32::from_rgb(244, 248, 252)
    } else {
        Color32::WHITE
    };
    painter.rect_filled(row, 0.0, row_fill);

    let mut x = row.left();
    let name_cell = Rect::from_min_size(row.min, Vec2::new(TREE_COLUMN_WIDTHS[0], row.height()));
    let indent = (depth as f32 * 15.0).min(225.0);
    let toggle = if node.is_dir && !node.children.is_empty() {
        let toggle_rect = Rect::from_center_size(
            egui::pos2(x + 10.0 + indent, row.center().y),
            Vec2::splat(14.0),
        );
        let toggle_response = ui.interact(
            toggle_rect,
            ui.make_persistent_id(("tree-toggle", index)),
            Sense::click(),
        );
        painter.rect_filled(toggle_rect, 0.0, Color32::WHITE);
        painter.rect_stroke(
            toggle_rect,
            0.0,
            Stroke::new(1.0, Color32::from_gray(145)),
            egui::StrokeKind::Inside,
        );
        painter.line_segment(
            [
                toggle_rect.left_center() + Vec2::new(3.0, 0.0),
                toggle_rect.right_center() - Vec2::new(3.0, 0.0),
            ],
            Stroke::new(1.0, Color32::from_gray(70)),
        );
        if !expanded {
            painter.line_segment(
                [
                    toggle_rect.center_top() + Vec2::new(0.0, 3.0),
                    toggle_rect.center_bottom() - Vec2::new(0.0, 3.0),
                ],
                Stroke::new(1.0, Color32::from_gray(70)),
            );
        }
        toggle_response.clicked()
    } else {
        false
    };

    let icon_x = x + indent + 21.0;
    let icon_rect = Rect::from_min_size(
        egui::pos2(icon_x, row.center().y - 8.0),
        Vec2::new(18.0, 16.0),
    );
    if node.is_dir {
        draw_folder_icon(painter, icon_rect);
    } else {
        draw_file_icon(painter, icon_rect);
    }
    let name_clip = Rect::from_min_max(
        egui::pos2(icon_rect.right() + 5.0, row.top()),
        egui::pos2(name_cell.right() - 4.0, row.bottom()),
    );
    painter.with_clip_rect(name_clip).text(
        egui::pos2(name_clip.left(), row.center().y),
        Align2::LEFT_CENTER,
        &node.name,
        FontId::proportional(14.0),
        Color32::from_gray(32),
    );
    response
        .clone()
        .on_hover_text(node.path.display().to_string());
    x += TREE_COLUMN_WIDTHS[0];

    let percent = if total_size == 0 {
        0.0
    } else {
        node.size as f32 / total_size as f32
    };
    let progress_cell = Rect::from_min_size(
        egui::pos2(x, row.top()),
        Vec2::new(TREE_COLUMN_WIDTHS[1], row.height()),
    );
    let track = Rect::from_center_size(
        progress_cell.center(),
        Vec2::new(progress_cell.width() - 12.0, 16.0),
    );
    painter.rect_filled(track, 0.0, Color32::from_rgb(231, 231, 231));
    let fill = Rect::from_min_max(
        track.min + Vec2::splat(1.0),
        egui::pos2(
            track.left() + 1.0 + (track.width() - 2.0) * percent.clamp(0.0, 1.0),
            track.bottom() - 1.0,
        ),
    );
    if fill.width() > 0.0 {
        painter.rect_filled(fill, 0.0, Color32::from_rgb(157, 82, 76));
    }
    painter.rect_stroke(
        track,
        0.0,
        Stroke::new(1.0, Color32::from_gray(190)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        track.center(),
        Align2::CENTER_CENTER,
        format!("{:.0}%", percent * 100.0),
        FontId::proportional(12.0),
        Color32::from_gray(35),
    );
    x += TREE_COLUMN_WIDTHS[1];

    let cells = [
        (human_size(node.size), TREE_COLUMN_WIDTHS[2], true),
        (
            (node.files + node.folders).to_string(),
            TREE_COLUMN_WIDTHS[3],
            true,
        ),
        (node.files.to_string(), TREE_COLUMN_WIDTHS[4], true),
        (node.folders.to_string(), TREE_COLUMN_WIDTHS[5], true),
        (format_modified(node.modified), TREE_COLUMN_WIDTHS[6], false),
        (
            format_attributes(node.attributes),
            TREE_COLUMN_WIDTHS[7],
            false,
        ),
    ];
    for (text, cell_width, right_aligned) in cells {
        let align = if right_aligned {
            Align2::RIGHT_CENTER
        } else {
            Align2::LEFT_CENTER
        };
        let text_x = if right_aligned {
            x + cell_width - 7.0
        } else {
            x + 6.0
        };
        painter
            .with_clip_rect(Rect::from_min_max(
                egui::pos2(x + 2.0, row.top()),
                egui::pos2(x + cell_width - 2.0, row.bottom()),
            ))
            .text(
                egui::pos2(text_x, row.center().y),
                align,
                text,
                FontId::proportional(14.0),
                Color32::from_gray(38),
            );
        x += cell_width;
    }

    let mut separator_x = row.left();
    for cell_width in TREE_COLUMN_WIDTHS {
        separator_x += cell_width;
        painter.line_segment(
            [
                egui::pos2(separator_x, row.top() + 3.0),
                egui::pos2(separator_x, row.bottom() - 3.0),
            ],
            Stroke::new(1.0, Color32::from_gray(226)),
        );
    }
    painter.line_segment(
        [row.left_bottom(), row.right_bottom()],
        Stroke::new(
            1.0,
            if selected {
                Color32::from_rgb(142, 177, 211)
            } else {
                Color32::from_gray(232)
            },
        ),
    );
    (response.clicked(), toggle)
}

fn collect_visible_rows(
    nodes: &[Node],
    expanded: &HashSet<usize>,
    column: SortColumn,
    descending: bool,
) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    let mut stack = vec![(0usize, 0usize)];
    while let Some((index, depth)) = stack.pop() {
        rows.push((index, depth));
        if expanded.contains(&index) {
            let mut children = nodes[index].children.clone();
            children.sort_by(|&a, &b| compare_nodes(&nodes[a], &nodes[b], column, descending));
            stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
        }
    }
    rows
}

fn compare_nodes(a: &Node, b: &Node, column: SortColumn, descending: bool) -> std::cmp::Ordering {
    let order = match column {
        SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        SortColumn::Percent | SortColumn::Size => a.size.cmp(&b.size),
        SortColumn::Items => (a.files + a.folders).cmp(&(b.files + b.folders)),
        SortColumn::Files => a.files.cmp(&b.files),
        SortColumn::Folders => a.folders.cmp(&b.folders),
        SortColumn::Modified => a.modified.cmp(&b.modified),
        SortColumn::Attributes => a.attributes.cmp(&b.attributes),
    };
    if descending { order.reverse() } else { order }
}

fn format_modified(value: Option<std::time::SystemTime>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let Ok(duration) = value.duration_since(std::time::UNIX_EPOCH) else {
        return String::new();
    };
    let Ok(utc) = time::OffsetDateTime::from_unix_timestamp(duration.as_secs() as i64) else {
        return String::new();
    };
    let local = time::UtcOffset::current_local_offset().map_or(utc, |offset| utc.to_offset(offset));
    format!(
        "{:02}.{:02}.{:04} {:02}:{:02}",
        local.day(),
        u8::from(local.month()),
        local.year(),
        local.hour(),
        local.minute()
    )
}

fn format_attributes(value: u32) -> String {
    let mut result = String::new();
    for (flag, label) in [(1, "R"), (2, "H"), (4, "S"), (32, "A")] {
        if value & flag != 0 {
            result.push_str(label);
        }
    }
    result
}

fn draw_treemap(ui: &mut egui::Ui, nodes: &[Node], selected: &mut usize) {
    let size = ui.available_size().max(Vec2::splat(100.0));
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if nodes.first().is_none_or(|root| root.children.is_empty()) {
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "No items to display",
            FontId::proportional(15.0),
            Color32::GRAY,
        );
        return;
    }
    let painter = ui.painter().with_clip_rect(rect);
    let mut selected_rect = None;
    draw_treemap_level(
        ui,
        &painter,
        nodes,
        0,
        rect.shrink(2.0),
        selected,
        &mut selected_rect,
    );
    if let Some(selected_rect) = selected_rect {
        painter.rect_stroke(
            selected_rect.shrink(1.0),
            0.0,
            Stroke::new(2.0, Color32::WHITE),
            egui::StrokeKind::Inside,
        );
    }
}

fn squarify(nodes: &[Node], children: &[usize], bounds: Rect) -> Vec<(usize, Rect)> {
    let mut remaining: Vec<usize> = children
        .iter()
        .copied()
        .filter(|&i| nodes[i].size > 0)
        .collect();
    remaining.sort_by(|&a, &b| nodes[b].size.cmp(&nodes[a].size));
    let mut output = Vec::with_capacity(remaining.len());
    let mut area = bounds;
    let mut total: f64 = remaining.iter().map(|&i| nodes[i].size as f64).sum();
    while !remaining.is_empty() && area.width() >= 1.0 && area.height() >= 1.0 && total > 0.0 {
        let side = area.width().min(area.height()) as f64;
        let mut row = vec![remaining.remove(0)];
        while !remaining.is_empty() {
            let candidate = remaining[0];
            let old = worst_row(nodes, &row, side, total, area.area() as f64);
            let mut trial = row.clone();
            trial.push(candidate);
            if worst_row(nodes, &trial, side, total, area.area() as f64) <= old {
                row.push(remaining.remove(0));
            } else {
                break;
            }
        }
        let row_weight: f64 = row.iter().map(|&i| nodes[i].size as f64).sum();
        let strip_area = area.area() as f64 * row_weight / total;
        if area.width() >= area.height() {
            let strip_height =
                (strip_area / area.width() as f64).clamp(0.0, area.height() as f64) as f32;
            let mut x = area.left();
            for &index in &row {
                let w = (area.width() as f64 * nodes[index].size as f64 / row_weight) as f32;
                output.push((
                    index,
                    Rect::from_min_max(
                        egui::pos2(x, area.top()),
                        egui::pos2((x + w).min(area.right()), area.top() + strip_height),
                    ),
                ));
                x += w;
            }
            area.min.y += strip_height;
        } else {
            let strip_width =
                (strip_area / area.height() as f64).clamp(0.0, area.width() as f64) as f32;
            let mut y = area.top();
            for &index in &row {
                let h = (area.height() as f64 * nodes[index].size as f64 / row_weight) as f32;
                output.push((
                    index,
                    Rect::from_min_max(
                        egui::pos2(area.left(), y),
                        egui::pos2(area.left() + strip_width, (y + h).min(area.bottom())),
                    ),
                ));
                y += h;
            }
            area.min.x += strip_width;
        }
        total -= row_weight;
    }
    output
}

fn worst_row(nodes: &[Node], row: &[usize], side: f64, total_weight: f64, total_area: f64) -> f64 {
    let min = row
        .iter()
        .map(|&i| nodes[i].size as f64)
        .fold(f64::INFINITY, f64::min);
    let max = row
        .iter()
        .map(|&i| nodes[i].size as f64)
        .fold(0.0, f64::max);
    if min <= 0.0 || total_weight <= 0.0 || total_area <= 0.0 {
        return f64::INFINITY;
    }
    let side_sq = side * side;
    let worst_thin = side_sq * total_weight / (total_area * min);
    let worst_wide = total_area * max / (total_weight * side_sq);
    worst_thin.max(worst_wide)
}

fn draw_treemap_level(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    nodes: &[Node],
    parent: usize,
    bounds: Rect,
    selected: &mut usize,
    selected_rect: &mut Option<Rect>,
) {
    for (index, rect) in squarify(nodes, &nodes[parent].children, bounds) {
        if rect.width() < 1.0 || rect.height() < 1.0 {
            continue;
        }
        let node = &nodes[index];
        let directory_response = if node.is_dir {
            Some(ui.interact(rect, ui.make_persistent_id(("tile", index)), Sense::click()))
        } else {
            None
        };
        if node.is_dir && !node.children.is_empty() {
            painter.rect_filled(rect, 0.0, Color32::from_rgb(24, 27, 31));
            draw_treemap_level(
                ui,
                painter,
                nodes,
                index,
                rect.shrink(1.0),
                selected,
                selected_rect,
            );
            painter.rect_stroke(
                rect.shrink(0.5),
                0.0,
                Stroke::new(1.0, Color32::from_gray(70)),
                egui::StrokeKind::Inside,
            );
        } else {
            let tile = rect.shrink(1.0);
            let color = extension_color_for_name(&node.name);
            paint_cushion(painter, tile, color);
            let light = shade(color, 1.5);
            let dark = shade(color, 0.48);
            painter.line_segment([tile.left_top(), tile.right_top()], Stroke::new(1.0, light));
            painter.line_segment(
                [tile.left_top(), tile.left_bottom()],
                Stroke::new(1.0, light),
            );
            painter.line_segment(
                [tile.left_bottom(), tile.right_bottom()],
                Stroke::new(1.0, dark),
            );
            painter.line_segment(
                [tile.right_top(), tile.right_bottom()],
                Stroke::new(1.0, dark),
            );
            if tile.width() > 90.0 && tile.height() > 28.0 {
                painter.text(
                    tile.left_top() + Vec2::splat(4.0),
                    Align2::LEFT_TOP,
                    &node.name,
                    FontId::proportional(12.0),
                    Color32::WHITE,
                );
            }
        }
        if index == *selected {
            *selected_rect = Some(rect);
        }
        let response = directory_response.unwrap_or_else(|| {
            ui.interact(rect, ui.make_persistent_id(("tile", index)), Sense::click())
        });
        if response.clicked() {
            *selected = index;
        }
        response.on_hover_text(format!(
            "{}\n{}",
            node.path.display(),
            human_size(node.size)
        ));
    }
}

fn shade(color: Color32, factor: f32) -> Color32 {
    Color32::from_rgb(
        (color.r() as f32 * factor).clamp(0.0, 255.0) as u8,
        (color.g() as f32 * factor).clamp(0.0, 255.0) as u8,
        (color.b() as f32 * factor).clamp(0.0, 255.0) as u8,
    )
}

fn paint_cushion(painter: &egui::Painter, rect: Rect, base: Color32) {
    let x0 = rect.left();
    let x1 = rect.center().x;
    let x2 = rect.right();
    let y0 = rect.top();
    let y1 = rect.center().y;
    let y2 = rect.bottom();
    let edge = shade(base, 0.82);
    let mid = shade(base, 1.12);
    let shine = shade(base, 1.72);
    let colors = [
        shade(base, 0.55),
        edge,
        shade(base, 0.68),
        edge,
        shine,
        mid,
        shade(base, 0.68),
        mid,
        shade(base, 0.48),
    ];
    let positions = [
        egui::pos2(x0, y0),
        egui::pos2(x1, y0),
        egui::pos2(x2, y0),
        egui::pos2(x0, y1),
        egui::pos2(x1, y1),
        egui::pos2(x2, y1),
        egui::pos2(x0, y2),
        egui::pos2(x1, y2),
        egui::pos2(x2, y2),
    ];
    let mut mesh = egui::Mesh::default();
    for (pos, color) in positions.into_iter().zip(colors) {
        mesh.colored_vertex(pos, color);
    }
    for row in 0..2u32 {
        for col in 0..2u32 {
            let a = row * 3 + col;
            let b = a + 1;
            let c = a + 3;
            let d = c + 1;
            mesh.add_triangle(a, b, d);
            mesh.add_triangle(a, d, c);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

fn draw_folder_icon(painter: &egui::Painter, rect: Rect) {
    let yellow = Color32::from_rgb(245, 190, 54);
    painter.rect_filled(
        Rect::from_min_max(
            rect.left_top() + Vec2::new(1.0, 3.0),
            rect.right_bottom() - Vec2::new(1.0, 1.0),
        ),
        1.0,
        yellow,
    );
    painter.rect_filled(
        Rect::from_min_max(
            rect.left_top() + Vec2::new(2.0, 1.0),
            egui::pos2(rect.left() + 8.0, rect.top() + 6.0),
        ),
        1.0,
        yellow,
    );
    painter.rect_stroke(
        rect.shrink(1.0),
        1.0,
        Stroke::new(0.7, Color32::from_rgb(150, 105, 24)),
        egui::StrokeKind::Inside,
    );
}

fn draw_file_icon(painter: &egui::Painter, rect: Rect) {
    let page = Rect::from_min_max(
        rect.left_top() + Vec2::new(4.0, 1.0),
        rect.right_bottom() - Vec2::new(4.0, 1.0),
    );
    painter.rect_filled(page, 0.5, Color32::WHITE);
    painter.rect_stroke(
        page,
        0.5,
        Stroke::new(0.7, Color32::from_gray(125)),
        egui::StrokeKind::Inside,
    );
}

fn extension_color_for_name(name: &str) -> Color32 {
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_else(|| "folder".to_owned());
    extension_color(&extension)
}

fn extension_color(extension: &str) -> Color32 {
    const PALETTE: [Color32; 12] = [
        Color32::from_rgb(59, 130, 246),
        Color32::from_rgb(16, 185, 129),
        Color32::from_rgb(245, 158, 11),
        Color32::from_rgb(239, 68, 68),
        Color32::from_rgb(168, 85, 247),
        Color32::from_rgb(20, 184, 166),
        Color32::from_rgb(236, 72, 153),
        Color32::from_rgb(132, 204, 22),
        Color32::from_rgb(14, 165, 233),
        Color32::from_rgb(249, 115, 22),
        Color32::from_rgb(99, 102, 241),
        Color32::from_rgb(163, 230, 53),
    ];
    let mut hash = 2_166_136_261_u32;
    for byte in extension.to_ascii_lowercase().bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    PALETTE[hash as usize % PALETTE.len()]
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
