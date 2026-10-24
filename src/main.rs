mod actions;
mod benchmark;
mod map_view;
mod metrics;
mod mft_index;
mod model;
mod parallel;
mod paths;
mod scanner;
mod search;
mod tree_view;
mod treemap;

use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Stroke, Vec2};
use model::Node;
use scanner::{ScanError, ScanEvent, ScanResult, ScanTask, scan_async};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;
use tree_view::{SortColumn, TreeView};

fn main() -> eframe::Result {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.first().is_some_and(|arg| arg == "--benchmark") {
        std::process::exit(benchmark::run(&arguments[1..]));
    }
    if arguments
        .first()
        .is_some_and(|arg| arg == "--benchmark-suite")
    {
        std::process::exit(benchmark::suite(&arguments[1..]));
    }
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
    search: search::SearchState,
    scan_task: Option<ScanTask>,
    scan_revision: u64,
    result: Option<ScanResult>,
    selected: usize,
    status: String,
    progress_files: u64,
    progress_processed: u64,
    progress_total: Option<u64>,
    progress_bytes: u64,
    scan_phase: String,
    error: Option<String>,
    confirm_delete: bool,
    delete_target: Option<PathBuf>,
    delete_task: Option<actions::DeleteTask>,
    admin_offer: bool,
    tree: TreeView,
    scroll_to_selected: bool,
    map: map_view::MapView,
}

const TREE_COLUMN_WIDTHS: [f32; 8] = [420.0, 125.0, 105.0, 85.0, 80.0, 90.0, 160.0, 65.0];
const TREE_ROW_HEIGHT: f32 = 28.0;
const TREE_COLUMNS: [(&str, SortColumn); 8] = [
    ("Имя", SortColumn::Name),
    ("Доля", SortColumn::Percent),
    ("Размер", SortColumn::Size),
    ("Элементы", SortColumn::Items),
    ("Файлы", SortColumn::Files),
    ("Папки", SortColumn::Folders),
    ("Изменён", SortColumn::Modified),
    ("Атрибуты", SortColumn::Attributes),
];
const TREE_CONTENT_WIDTH: f32 = {
    let mut total = 0.0;
    let mut index = 0;
    while index < TREE_COLUMN_WIDTHS.len() {
        total += TREE_COLUMN_WIDTHS[index];
        index += 1;
    }
    total
};

impl DiskApp {
    fn new(cc: &eframe::CreationContext<'_>, root: String) -> Self {
        Self::configure(&cc.egui_ctx);
        Self::with_root(root)
    }

    fn configure(ctx: &egui::Context) {
        ctx.set_theme(egui::ThemePreference::Light);
        let mut fonts = egui::FontDefinitions::default();
        if let Some(windows) = std::env::var_os("WINDIR")
            && let Ok(bytes) = std::fs::read(PathBuf::from(windows).join("Fonts/segoeui.ttf"))
        {
            fonts.font_data.insert(
                "segoe_ui".to_owned(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "segoe_ui".to_owned());
        }
        ctx.set_fonts(fonts);
        let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
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
        ctx.set_style_of(egui::Theme::Light, style);
    }

    fn with_root(root: String) -> Self {
        Self {
            root,
            search: search::SearchState::default(),
            scan_task: None,
            scan_revision: 0,
            result: None,
            selected: 0,
            status: "Укажите диск или папку и нажмите «Сканировать». Для быстрого сканирования NTFS запустите программу от имени администратора.".to_owned(),
            progress_files: 0,
            progress_processed: 0,
            progress_total: None,
            progress_bytes: 0,
            scan_phase: String::new(),
            error: None,
            confirm_delete: false,
            delete_target: None,
            delete_task: None,
            admin_offer: false,
            tree: TreeView::default(),
            scroll_to_selected: false,
            map: map_view::MapView::default(),
        }
    }

    fn start_scan(&mut self) {
        if self.scan_task.is_some() || self.delete_task.is_some() {
            return;
        }
        let root = PathBuf::from(self.root.trim());
        match scan_async(root) {
            Ok(task) => self.scan_task = Some(task),
            Err(error) => {
                self.error = Some(error.to_string());
                self.status = error.to_string();
                return;
            }
        }
        self.scan_revision += 1;
        self.result = None;
        self.map.invalidate();
        self.selected = 0;
        self.tree.collapse_all();
        self.scroll_to_selected = false;
        self.progress_files = 0;
        self.progress_processed = 0;
        self.progress_total = None;
        self.progress_bytes = 0;
        self.error = None;
        self.admin_offer = false;
        self.scan_phase = "Подготовка сканирования".to_owned();
        self.status = self.scan_phase.clone();
    }

    fn poll_scan(&mut self) {
        let mut finished = false;
        if let Some(task) = &self.scan_task {
            loop {
                match task.try_recv() {
                    Ok(ScanEvent::Progress {
                        files,
                        processed,
                        total,
                        bytes,
                        phase,
                    }) => {
                        self.progress_files = files;
                        self.progress_processed = processed;
                        self.progress_total = total;
                        self.progress_bytes = bytes;
                        self.scan_phase = phase.label().to_owned();
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
                                let skipped =
                                    result.warnings.skipped + result.warnings.corrupt_records;
                                if skipped > 0 {
                                    self.status.push_str(&format!("  •  Пропущено: {skipped}"));
                                }
                                self.tree.collapse_all();
                                self.scan_revision += 1;
                                self.result = Some(result);
                            }
                            Err(ScanError::Cancelled) => {
                                self.status = "Сканирование остановлено".to_owned()
                            }
                            Err(error) => {
                                self.status = error.to_string();
                                self.error = Some(error.to_string());
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
            self.scan_task = None;
        }
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.result
            .as_ref()
            .filter(|result| self.selected < result.nodes.len())
            .map(|result| result.nodes.path(self.selected))
    }

    fn select_treemap_node(&mut self, index: usize) {
        let Some(nodes) = self.result.as_ref().map(|result| result.nodes.clone()) else {
            return;
        };
        if index >= nodes.len() {
            return;
        }
        self.selected = index;
        self.search.query.clear();
        let mut ancestor = nodes[index].parent();
        while let Some(parent) = ancestor {
            self.tree.expand(parent);
            ancestor = nodes[parent].parent();
        }
        self.scroll_to_selected = true;
    }

    fn open_selected(&mut self) {
        #[cfg(windows)]
        if let Some(result) = &self.result
            && let Some(node) = result.nodes.get(self.selected)
            && let Err(error) = actions::open(&result.nodes.path(self.selected), node.is_dir)
        {
            self.status = error;
        }
    }

    fn poll_delete(&mut self) {
        if let Some(result) = self
            .delete_task
            .as_ref()
            .and_then(actions::DeleteTask::poll)
        {
            self.delete_task = None;
            match result {
                Ok(()) => {
                    self.result = None;
                    self.scan_revision += 1;
                    self.map.invalidate();
                    self.tree.collapse_all();
                    self.selected = 0;
                    self.status =
                        "Объект удалён. Просканируйте папку для обновления результатов.".to_owned();
                }
                Err(error) => self.status = error,
            }
        }
    }
}

impl eframe::App for DiskApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

impl DiskApp {
    fn show(&mut self, ui: &mut egui::Ui) {
        self.poll_scan();
        self.poll_delete();
        if self.search.update(
            self.result.as_ref().map(|result| &result.nodes),
            self.scan_revision,
        ) {
            ui.ctx().request_repaint_after(Duration::from_millis(40));
        }
        let is_scanning = self.scan_task.is_some();
        let is_busy = is_scanning || self.delete_task.is_some();
        let map_progress = self
            .map
            .progress
            .as_ref()
            .filter(|_| self.map.error.is_none())
            .map(|p| p.snapshot());
        let mut start = false;
        let mut cancel = false;
        let mut open_selected = false;
        let mut delete_selected = false;

        egui::Panel::top("menubar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("Файл", |ui| {
                    if ui
                        .add_enabled(!is_busy, egui::Button::new("Сканировать"))
                        .clicked()
                    {
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
                            self.selected != 0 && self.result.is_some() && !is_busy,
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
                        self.tree.expand(0);
                        ui.close();
                    }
                    if ui.button("Свернуть всё").clicked() {
                        self.tree.collapse_all();
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
                    .add_enabled(!is_busy, egui::Button::new("Сканировать"))
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
                        self.selected != 0 && self.result.is_some() && !is_busy,
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
                        match actions::relaunch_elevated(PathBuf::from(self.root.trim()).as_path())
                        {
                            Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                            Err(error) => self.status = error,
                        }
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Поиск:");
                ui.add(egui::TextEdit::singleline(&mut self.search.query).desired_width(340.0));
                ui.label("по имени или пути");
                if let Some(error) = &self.search.error {
                    ui.label(format!("Ошибка поиска: {error}"));
                }
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let response = ui.label(&self.status);
                if let Some(result) = &self.result {
                    response.on_hover_text(format!(
                        "{}\n{}",
                        result.timings.describe(),
                        result.warnings.examples.join("\n")
                    ));
                }
                if is_scanning {
                    ui.separator();
                    ui.label(format!(
                        "{} · {}",
                        self.scan_phase,
                        human_size(self.progress_bytes)
                    ));
                } else if let Some(error) = &self.map.error {
                    ui.separator();
                    ui.label(format!("Ошибка treemap: {error}"));
                } else if let Some((phase, _, _)) = map_progress.filter(|(phase, _, _)| {
                    *phase == treemap::RenderPhase::Preparing
                        || *phase == treemap::RenderPhase::Layout
                        || *phase == treemap::RenderPhase::Raster
                }) {
                    ui.separator();
                    ui.label(match phase {
                        treemap::RenderPhase::Preparing => "Подготовка treemap",
                        treemap::RenderPhase::Layout => "Раскладка плиток",
                        _ => "Отрисовка treemap",
                    });
                } else if map_progress
                    .is_some_and(|(phase, _, _)| phase == treemap::RenderPhase::Limited)
                {
                    ui.separator();
                    ui.label("Treemap: превышен лимит временной памяти");
                }
            });
            if is_scanning {
                let fraction = self.progress_total.map(|total| {
                    if total == 0 {
                        0.0
                    } else {
                        (self.progress_processed as f32 / total as f32).clamp(0.0, 1.0)
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
                *phase == treemap::RenderPhase::Preparing
                    || *phase == treemap::RenderPhase::Layout
                    || *phase == treemap::RenderPhase::Raster
            }) {
                let fraction = if total == 0 {
                    0.0
                } else {
                    (done as f32 / total as f32).clamp(0.0, 1.0)
                };
                let label = match phase {
                    treemap::RenderPhase::Preparing => "Подготовка treemap",
                    treemap::RenderPhase::Layout => "Раскладка плиток",
                    _ => "Отрисовка treemap",
                };
                ui.add(egui::ProgressBar::new(fraction).text(format!("{label}: {done}/{total}")));
            } else if self.result.is_some() && self.map.error.is_none() {
                // Keep the status panel's height fixed while the map worker starts,
                // finishes, or is restarted for a resized viewport.
                let label = self.map.elapsed.map_or_else(
                    || "Treemap ready".to_owned(),
                    |elapsed| format!("Treemap ready · {:.2} s", elapsed.as_secs_f64()),
                );
                ui.add(egui::ProgressBar::new(1.0).text(label));
            }
        });

        egui::Panel::top("directory_table_v2")
            .resizable(true)
            .default_size(ui.available_height() * 0.46)
            .min_size(170.0)
            .show(ui, |ui| {
                if let Some(result) = &self.result {
                    self.tree.update(&result.nodes, self.scan_revision);
                    let rows = if self.search.query.trim().is_empty() {
                        self.tree.rows()
                    } else {
                        self.search.results.as_slice()
                    };
                    let mut toggled_nodes = Vec::new();
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
                            for ((title, column), width) in
                                TREE_COLUMNS.iter().zip(TREE_COLUMN_WIDTHS)
                            {
                                sort_header(
                                    ui,
                                    title,
                                    width,
                                    *column,
                                    &self.tree.column,
                                    self.tree.descending,
                                    &mut clicked_sort,
                                );
                            }
                        });
                        if rows.is_empty() {
                            ui.label(if self.search.query.trim().is_empty() {
                                "Результаты сканирования появятся здесь."
                            } else {
                                "Совпадений нет."
                            });
                        } else {
                            let mut row_scroll = egui::ScrollArea::vertical();
                            if let Some(row) = scroll_row {
                                row_scroll = row_scroll.vertical_scroll_offset(
                                    tree_view::centered_row_offset(
                                        row,
                                        ui.available_height(),
                                        TREE_ROW_HEIGHT,
                                        ui.spacing().item_spacing.y,
                                    ),
                                );
                            }
                            row_scroll.show_rows(ui, TREE_ROW_HEIGHT, rows.len(), |ui, range| {
                                for row in range {
                                    let (index, depth) = rows[row];
                                    let node = &result.nodes[index];
                                    let (row_response, toggled) = draw_directory_row(
                                        ui,
                                        node,
                                        index,
                                        depth,
                                        result.nodes[0].size,
                                        self.selected == index,
                                        self.tree.is_expanded(index),
                                    );
                                    row_response.clone().on_hover_ui(|ui| {
                                        ui.label(result.nodes.path(index).display().to_string());
                                    });
                                    if scroll_row
                                        .is_some_and(|selected_row| rows[selected_row].0 == index)
                                    {
                                        row_response.scroll_to_me(Some(egui::Align::Center));
                                    }
                                    if row_response.clicked() {
                                        self.selected = index;
                                    }
                                    if toggled {
                                        toggled_nodes.push(index);
                                    }
                                }
                            });
                            if scroll_row.is_some() {
                                self.scroll_to_selected = false;
                            }
                        }
                    });
                    for index in toggled_nodes {
                        self.tree.toggle(index);
                    }
                    if let Some(column) = clicked_sort {
                        self.tree.sort_by(column);
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
                self.map.poll(ui.ctx(), &nodes, physical);
                if let Some(texture) = &self.map.texture {
                    ui.painter().image(
                        texture.id(),
                        rect,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    if response.hovered()
                        && let Some(pointer) = response.hover_pos()
                    {
                        let x = ((pointer.x - rect.left()) / rect.width() * self.map.size[0] as f32)
                            .floor()
                            .max(0.0) as usize;
                        let y = ((pointer.y - rect.top()) / rect.height() * self.map.size[1] as f32)
                            .floor()
                            .max(0.0) as usize;
                        if let Some(&hit) = self.map.hits.get(y * self.map.size[0] + x)
                            && hit != u32::MAX
                        {
                            let Some(node) = nodes.get(hit as usize) else {
                                return;
                            };
                            response.clone().on_hover_text(format!(
                                "{}\n{}",
                                nodes.path(hit as usize).display(),
                                human_size(node.size)
                            ));
                            if response.clicked() {
                                self.select_treemap_node(hit as usize);
                            }
                        }
                    }
                    if let Some(r) = self.map.rects.get(self.selected) {
                        let sx = rect.width() / self.map.size[0] as f32;
                        let sy = rect.height() / self.map.size[1] as f32;
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
            if let Some(task) = &self.scan_task {
                task.cancel();
            }
            self.status = "Останавливаю сканирование…".to_owned();
        }
        if open_selected {
            self.open_selected();
        }
        if delete_selected && let Some(path) = self.selected_path() {
            self.delete_target = Some(path.to_path_buf());
            self.confirm_delete = true;
        }

        if self.confirm_delete {
            egui::Window::new("Подтверждение удаления")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    let path = self
                        .delete_target
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    ui.label("Удалить выбранный объект с диска?");
                    ui.label(path);
                    ui.horizontal(|ui| {
                        if ui.button("Удалить навсегда").clicked() {
                            if let Some(path) = self.delete_target.take() {
                                match actions::DeleteTask::start(path) {
                                    Ok(task) => self.delete_task = Some(task),
                                    Err(error) => self.error = Some(error),
                                }
                                self.status = "Удаляю объект…".to_owned();
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
                            match actions::relaunch_elevated(
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

        if is_scanning || self.delete_task.is_some() {
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
) -> (egui::Response, bool) {
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
        node.name.to_string_lossy(),
        FontId::proportional(14.0),
        Color32::from_gray(32),
    );

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
    (response, toggle)
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

#[cfg(test)]
mod app_tests {
    use super::*;

    #[test]
    fn selecting_a_distant_tile_reveals_its_row_and_survives_repeated_frames() {
        let root = PathBuf::from("root");
        let mut builder = model::TreeBuilder::new(root.clone());
        for index in 0..10_000 {
            builder.add(
                root.join(format!("folder/file-{index:05}.bin")),
                false,
                index + 10,
                None,
                0,
            );
        }
        builder.add(root.join("folder/target-file.bin"), false, 1, None, 0);
        let nodes = Arc::new(builder.finish());
        let target = nodes
            .iter()
            .position(|node| node.name.as_ref() == std::ffi::OsStr::new("target-file.bin"))
            .unwrap();
        let mut app = DiskApp::with_root("root".into());
        app.result = Some(ScanResult {
            nodes,
            mode: scanner::ScanMode::DirectoryWalk,
            files: 10_001,
            elapsed: Duration::ZERO,
            warnings: scanner::ScanWarnings::default(),
            timings: scanner::ScanTimings::default(),
        });
        app.scan_revision = 1;
        app.select_treemap_node(target);
        let ctx = egui::Context::default();
        DiskApp::configure(&ctx);
        let mut visible = false;
        for frame in 0..100 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1360.0, 850.0),
                )),
                time: Some(frame as f64 / 60.0),
                ..egui::RawInput::default()
            };
            let output = ctx.run_ui(input, |ui| app.show(ui));
            visible |= output.shapes.iter().any(|shape| match &shape.shape {
                egui::Shape::Text(text) => text.galley.job.text.contains("target-file.bin"),
                _ => false,
            });
            assert_eq!(app.selected, target);
        }
        assert!(
            visible,
            "the selected file must actually be painted, not just selected in the model"
        );
        assert!(!app.scroll_to_selected);
    }
}
