mod render;

use crate::scan::{self, Node};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use std::{
    path::{Path, PathBuf},
    sync::{atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

pub fn human_size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

// Filenames can contain terminal escapes, newlines, or other control characters.
pub fn safe_text(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

pub fn size_explanation(apparent: bool) -> &'static str {
    if apparent {
        "Logical size is not disk usage; sparse/compressed files and cloud placeholders can exceed disk capacity."
    } else {
        "Disk usage = allocated blocks; logical size includes sparse holes and cloud-only content."
    }
}

fn find_node<'a>(root: &'a Node, path: &Path) -> Option<&'a Node> {
    if root.path == path {
        Some(root)
    } else {
        root.children
            .iter()
            .find(|child| path.starts_with(&child.path))
            .and_then(|child| find_node(child, path))
    }
}

struct Confirmation {
    path: PathBuf,
    choice: usize,
}

struct Browser {
    stack: Vec<(PathBuf, usize)>,
    apparent: bool,
    filter: String,
    editing_filter: bool,
    sort_name: bool,
    help: bool,
    table_state: TableState,
    confirmation: Option<Confirmation>,
    ask_delete: bool,
    requested_delete: Option<PathBuf>,
    requested_rescan: bool,
    status: String,
}

impl Browser {
    fn new(root: &Node, apparent: bool) -> Self {
        Self {
            stack: vec![(root.path.clone(), 0)],
            apparent,
            filter: String::new(),
            editing_filter: false,
            sort_name: false,
            help: false,
            table_state: TableState::default(),
            confirmation: None,
            ask_delete: true,
            requested_delete: None,
            requested_rescan: false,
            status: String::new(),
        }
    }

    fn entries<'a>(&mut self, directory: &'a Node) -> Vec<&'a Node> {
        let query = self.filter.to_lowercase();
        let mut entries: Vec<_> = directory
            .children
            .iter()
            .filter(|node| node.name().to_lowercase().contains(&query))
            .collect();
        entries.sort_by(|a, b| {
            if self.sort_name {
                a.path.cmp(&b.path)
            } else {
                b.size(self.apparent)
                    .cmp(&a.size(self.apparent))
                    .then_with(|| a.path.cmp(&b.path))
            }
        });
        let selected = self
            .stack
            .last()
            .unwrap()
            .1
            .min(entries.len().saturating_sub(1));
        self.select(selected);
        self.table_state
            .select((!entries.is_empty()).then_some(selected));
        entries
    }

    fn select(&mut self, index: usize) {
        self.stack.last_mut().unwrap().1 = index;
    }

    fn refresh_after_delete(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        root: &mut Node,
        options: &scan::Options,
    ) -> Result<()> {
        let Some(path) = self.requested_delete.take() else {
            return Ok(());
        };
        self.status = delete_with_progress(terminal, root, &path, options)?;
        while self.stack.len() > 1 && find_node(root, &self.stack.last().unwrap().0).is_none() {
            self.stack.pop();
        }
        self.table_state = TableState::default();
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent, entries: &[&Node], height: u16) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if self.confirmation.is_some() {
            self.handle_confirmation(key.code);
        } else if self.editing_filter {
            self.handle_filter(key.code);
        } else {
            return self.handle_navigation(key, entries, height);
        }
        false
    }

    fn handle_confirmation(&mut self, key: KeyCode) {
        let confirmation = self.confirmation.as_mut().unwrap();
        match key {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => self.confirmation = None,
            KeyCode::Up | KeyCode::Left => confirmation.choice = (confirmation.choice + 2) % 3,
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                confirmation.choice = (confirmation.choice + 1) % 3
            }
            KeyCode::Enter => {
                if confirmation.choice != 1 {
                    self.requested_delete = Some(confirmation.path.clone());
                    if confirmation.choice == 2 {
                        self.ask_delete = false;
                    }
                }
                self.confirmation = None;
            }
            _ => {}
        }
    }

    fn handle_filter(&mut self, key: KeyCode) {
        match key {
            KeyCode::Enter => self.editing_filter = false,
            KeyCode::Esc => {
                self.editing_filter = false;
                self.filter.clear();
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(ch) => self.filter.push(ch),
            _ => {}
        }
        self.select(0);
    }

    fn request_delete(&mut self, node: &Node) {
        self.status.clear();
        if self.ask_delete {
            self.confirmation = Some(Confirmation {
                path: node.path.clone(),
                choice: 1,
            });
        } else {
            self.requested_delete = Some(node.path.clone());
        }
    }

    fn reset_directory_view(&mut self) {
        self.filter.clear();
        self.table_state = TableState::default();
    }

    fn handle_navigation(&mut self, key: KeyEvent, entries: &[&Node], height: u16) -> bool {
        let selected = self.stack.last().unwrap().1;
        let last = entries.len().saturating_sub(1);
        let page = usize::from(height.saturating_sub(if self.help { 15 } else { 9 })).max(1);
        match key.code {
            KeyCode::Char('r') if key.kind == KeyEventKind::Press => self.requested_rescan = true,
            KeyCode::Char('q') => return true,
            KeyCode::Char('d') if key.kind == KeyEventKind::Press => {
                if let Some(node) = entries.get(selected) {
                    self.request_delete(node);
                }
            }
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Esc => {
                self.help = false;
                self.filter.clear();
            }
            KeyCode::Char('/') => {
                self.editing_filter = true;
                self.filter.clear();
            }
            KeyCode::Char('a') => self.apparent = !self.apparent,
            KeyCode::Char('s') => {
                self.sort_name = !self.sort_name;
                self.select(0);
            }
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select((selected + 1).min(last)),
            KeyCode::PageUp => self.select(selected.saturating_sub(page)),
            KeyCode::PageDown => self.select(selected.saturating_add(page).min(last)),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(last),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(node) = entries.get(selected).filter(|node| node.is_dir) {
                    self.stack.push((node.path.clone(), 0));
                    self.reset_directory_view();
                }
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') if self.stack.len() > 1 => {
                self.stack.pop();
                self.reset_directory_view();
            }
            _ => {}
        }
        false
    }
}

pub fn browse(root: &mut Node, apparent: bool, options: &scan::Options) -> Result<()> {
    let mut terminal = ratatui::try_init()?;
    let result = run_browser(&mut terminal, root, apparent, options);
    ratatui::restore();
    result
}

fn run_browser(
    terminal: &mut ratatui::DefaultTerminal,
    root: &mut Node,
    apparent: bool,
    options: &scan::Options,
) -> Result<()> {
    let mut browser = Browser::new(root, apparent);
    loop {
        browser.refresh_after_delete(terminal, root, options)?;
        if std::mem::take(&mut browser.requested_rescan) {
            let path = browser.stack.last().unwrap().0.clone();
            browser.status = rescan_with_progress(terminal, root, &path, options)?;
            browser.table_state = TableState::default();
        }
        let directory = find_node(root, &browser.stack.last().unwrap().0).unwrap();
        let entries = browser.entries(directory);
        terminal.draw(|frame| render::draw(frame, &mut browser, directory, &entries))?;
        if let Event::Key(key) = event::read()? {
            if browser.handle_key(key, &entries, terminal.size()?.height) {
                return Ok(());
            }
        }
    }
}

fn rescan_with_progress(
    terminal: &mut ratatui::DefaultTerminal,
    root: &mut Node,
    path: &Path,
    options: &scan::Options,
) -> Result<String> {
    let progress = scan::Progress::default();
    let started = Instant::now();
    terminal
        .draw(|frame| render::draw_rescan_progress(frame, path, &progress, started.elapsed()))?;
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        let progress = &progress;
        scope.spawn(move || {
            let _ = sender.send(scan::rescan(root, path, options, progress));
        });
        loop {
            match receiver.try_recv() {
                Ok(result) => {
                    return Ok(match result {
                        Ok(()) => format!(
                            "Rescanned {} ({} entries, {} allocated scanned, {} errors)",
                            safe_text(&path.to_string_lossy()),
                            progress.entries.load(Ordering::Relaxed),
                            human_size(progress.allocated.load(Ordering::Relaxed)),
                            progress.errors.load(Ordering::Relaxed)
                        ),
                        Err(error) => {
                            format!("Rescan failed: {}", safe_text(&format!("{error:#}")))
                        }
                    })
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("rescan worker disconnected")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            terminal.draw(|frame| {
                render::draw_rescan_progress(frame, path, progress, started.elapsed())
            })?;
            if event::poll(Duration::from_millis(100))? {
                let _ = event::read()?;
            }
        }
    })
}

fn delete_with_progress(
    terminal: &mut ratatui::DefaultTerminal,
    root: &mut Node,
    path: &Path,
    options: &scan::Options,
) -> Result<String> {
    let progress = crate::delete::Progress::default();
    let started = Instant::now();
    // Paint before starting: even a very short deletion has visible feedback.
    terminal.draw(|frame| {
        render::draw_delete_progress(frame, path, &progress, options.threads, started.elapsed())
    })?;
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        let progress = &progress;
        scope.spawn(move || {
            let status = perform_delete(root, path, options, progress);
            let _ = sender.send(status);
        });
        let result = (|| loop {
            match receiver.try_recv() {
                Ok(status) => return Ok(status),
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("deletion worker disconnected")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            terminal.draw(|frame| {
                render::draw_delete_progress(
                    frame,
                    path,
                    progress,
                    options.threads,
                    started.elapsed(),
                )
            })?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Release
                        && (matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                            || key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        progress.cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
        })();
        if result.is_err() {
            progress.cancel.store(true, Ordering::Relaxed);
        }
        result
    })
}

fn perform_delete(
    root: &mut Node,
    path: &Path,
    options: &scan::Options,
    progress: &crate::delete::Progress,
) -> String {
    let result = find_node(root, path)
        .ok_or_else(|| anyhow::anyhow!("entry no longer exists in the scanned tree"))
        .and_then(|target| crate::delete::remove_parallel(root, target, options.threads, progress));
    let deleted = result.is_ok();
    let count = progress.removed.load(Ordering::Relaxed);
    let processed = human_size(progress.removed_allocated.load(Ordering::Relaxed));
    let mut status = match result {
        Ok(()) => format!(
            "Deleted {count} entries ({processed} allocated processed, not space freed): {}",
            safe_text(&path.to_string_lossy())
        ),
        Err(error) => format!(
            "Stopped after {count} deletions ({processed} allocated processed, not space freed): {}",
            safe_text(&format!("{error:#}"))
        ),
    };
    progress.phase.store(2, Ordering::Relaxed);
    if let Err(error) = scan::update_after_delete(root, path, options, deleted) {
        status.push_str(&format!(
            "; refresh failed: {}; restart to refresh totals",
            safe_text(&error.to_string())
        ));
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn escapes_terminal_controls() {
        assert_eq!(safe_text("a\n\x1b[31m"), "a\\n\\u{1b}[31m");
        assert_eq!(safe_text("normal file"), "normal file");
    }

    fn snapshot(path: &Path) -> Node {
        scan::scan(
            path,
            &scan::Options {
                threads: 2,
                one_file_system: false,
                count_links: false,
            },
            &scan::Progress::default(),
        )
        .unwrap()
    }

    #[test]
    fn confirmation_defaults_to_no_and_session_bypass_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("file"), b"test").unwrap();
        let root = snapshot(temp.path());
        let node = &root.children[0];
        let mut browser = Browser::new(&root, false);

        browser.request_delete(node);
        browser.handle_confirmation(KeyCode::Enter);
        assert!(browser.requested_delete.is_none());
        assert!(browser.ask_delete);

        browser.request_delete(node);
        browser.handle_confirmation(KeyCode::Down);
        browser.handle_confirmation(KeyCode::Enter);
        assert_eq!(browser.requested_delete.take(), Some(node.path.clone()));
        assert!(!browser.ask_delete);
        browser.request_delete(node);
        assert!(browser.confirmation.is_none());
        assert_eq!(browser.requested_delete, Some(node.path.clone()));
        assert!(Browser::new(&root, false).ask_delete);
    }

    #[test]
    fn navigation_filtering_and_empty_selection_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        let root = snapshot(temp.path());
        let mut browser = Browser::new(&root, false);
        let entries = browser.entries(&root);
        browser.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &entries,
            24,
        );
        assert_eq!(browser.stack.len(), 2);
        browser.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &[], 24);
        assert_eq!(browser.stack.len(), 1);
        browser.handle_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &entries,
            24,
        );
        browser.handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &entries,
            24,
        );
        browser.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &entries,
            24,
        );
        let entries = browser.entries(&root);
        assert!(entries.is_empty());
        browser.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &entries,
            24,
        );
        assert!(browser.confirmation.is_none());
        assert!(browser.requested_delete.is_none());
    }
}
