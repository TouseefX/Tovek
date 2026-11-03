//! A small built-in file/folder browser. Native dialogs don't exist on
//! Android (and pull heavy deps on desktop), so one egui window serves every
//! platform — and doubles as the no-typing path picker for touch screens.

use std::path::{Path, PathBuf};

use eframe::egui;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickKind {
    File,
    Dir,
}

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

pub struct Picker {
    pub kind: PickKind,
    pub title: String,
    dir: PathBuf,
    entries: Vec<Entry>,
    error: Option<String>,
    quick_links: Vec<(String, PathBuf)>,
    /// Set when the user confirms a selection.
    pub picked: Option<PathBuf>,
    /// Set when the user cancels.
    pub cancelled: bool,
}

impl Picker {
    pub fn new(kind: PickKind, title: impl Into<String>, start: &Path) -> Self {
        let mut start = start.to_path_buf();
        if !start.is_dir() {
            start = start
                .parent()
                .filter(|parent| parent.is_dir())
                .map(Path::to_path_buf)
                .unwrap_or_else(default_root);
        }
        let mut picker = Self {
            kind,
            title: title.into(),
            dir: start,
            entries: Vec::new(),
            error: None,
            quick_links: quick_links(),
            picked: None,
            cancelled: false,
        };
        picker.refresh();
        picker
    }

    fn refresh(&mut self) {
        self.entries.clear();
        self.error = None;
        match std::fs::read_dir(&self.dir) {
            Ok(read) => {
                for entry in read.flatten() {
                    let path = entry.path();
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let name = entry.file_name().to_string_lossy().into_owned();
                    self.entries.push(Entry { name, path, is_dir });
                }
                self.entries.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
            }
            Err(error) => {
                self.error = Some(format!("cannot list {}: {error}", self.dir.display()));
            }
        }
    }

    fn enter(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.refresh();
    }

    /// Draw the picker window. Returns `true` while it should stay open.
    pub fn ui(&mut self, ctx: &egui::Context) -> bool {
        let mut open = true;
        let mut enter: Option<PathBuf> = None;
        egui::Window::new(&self.title)
            .id(egui::Id::new("tovek-picker"))
            .collapsible(false)
            .resizable(true)
            .default_size([520.0, 480.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("⬆ Up").clicked()
                        && let Some(parent) = self.dir.parent()
                    {
                        enter = Some(parent.to_path_buf());
                    }
                    for (label, path) in &self.quick_links {
                        if ui.button(label).clicked() {
                            enter = Some(path.clone());
                        }
                    }
                });
                ui.label(
                    egui::RichText::new(self.dir.display().to_string())
                        .monospace()
                        .weak(),
                );
                ui.separator();
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for entry in &self.entries {
                            let label = if entry.is_dir {
                                format!("📁 {}", entry.name)
                            } else {
                                format!("📄 {}", entry.name)
                            };
                            let enabled = entry.is_dir || self.kind == PickKind::File;
                            if ui
                                .add_enabled(enabled, egui::Button::new(label).frame(false))
                                .clicked()
                            {
                                if entry.is_dir {
                                    enter = Some(entry.path.clone());
                                } else if self.kind == PickKind::File {
                                    self.picked = Some(entry.path.clone());
                                }
                            }
                        }
                        if self.entries.is_empty() && self.error.is_none() {
                            ui.weak("(empty)");
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if self.kind == PickKind::Dir && ui.button("✔ Use this folder").clicked() {
                        self.picked = Some(self.dir.clone());
                    }
                    if ui.button("Cancel").clicked() {
                        self.cancelled = true;
                    }
                });
            });
        if let Some(dir) = enter {
            self.enter(dir);
        }
        if self.picked.is_some() || self.cancelled {
            open = false;
        }
        open
    }
}

fn default_root() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        let shared = PathBuf::from("/storage/emulated/0");
        if shared.is_dir() {
            return shared;
        }
    }
    if let Some(home) = home_dir() {
        return home;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

fn quick_links() -> Vec<(String, PathBuf)> {
    let mut links = Vec::new();
    #[cfg(target_os = "android")]
    {
        for (label, path) in [
            ("Storage", "/storage/emulated/0"),
            ("Download", "/storage/emulated/0/Download"),
        ] {
            let path = PathBuf::from(path);
            if path.is_dir() {
                links.push((label.to_owned(), path));
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        if let Some(home) = home_dir() {
            links.push(("🏠 Home".to_owned(), home));
        }
        if let Ok(cwd) = std::env::current_dir() {
            links.push(("• Current".to_owned(), cwd));
        }
    }
    links
}
