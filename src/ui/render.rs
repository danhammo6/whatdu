use super::{human_size, safe_text, size_explanation, Browser, Confirmation};
use crate::scan::Node;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};
use std::{path::Path, sync::atomic::Ordering, time::Duration};

pub(super) fn draw_rescan_progress(
    frame: &mut Frame,
    path: &Path,
    progress: &crate::scan::Progress,
    elapsed: Duration,
) {
    let text = format!("{}\n\n{} entries scanned | {} allocated scanned\n{} errors | {:.1}s elapsed\n\nPlease wait; rescanning the current directory and updating totals.", safe_text(&path.to_string_lossy()), progress.entries.load(Ordering::Relaxed), human_size(progress.allocated.load(Ordering::Relaxed)), progress.errors.load(Ordering::Relaxed), elapsed.as_secs_f64());
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Rescan progress "),
        ),
        frame.area(),
    );
}

pub(super) fn draw_delete_progress(
    frame: &mut Frame,
    path: &Path,
    progress: &crate::delete::Progress,
    threads: usize,
    elapsed: Duration,
) {
    let phase = match progress.phase.load(Ordering::Relaxed) {
        0 => "Checking filesystem boundaries",
        1 => "Deleting",
        _ => "Updating totals",
    };
    let text = format!("{}\n\n{phase} | {threads} workers | {:.1}s elapsed\n{} entries checked | {} entries deleted\n{} allocated processed (summed deleted-entry sizes, NOT space freed)\n\n{}\nFiles, symlinks, and directories count as one entry each.",
        safe_text(&path.to_string_lossy()), elapsed.as_secs_f64(),
        progress.checked.load(Ordering::Relaxed), progress.removed.load(Ordering::Relaxed),
        human_size(progress.removed_allocated.load(Ordering::Relaxed)),
        if progress.cancel.load(Ordering::Relaxed) { "Cancellation requested; waiting for workers and updating totals." }
        else { "Esc/q/Ctrl-C: cancel (already deleted entries stay deleted)" });
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Delete progress "),
        ),
        frame.area(),
    );
}

pub(super) fn draw(frame: &mut Frame, browser: &mut Browser, directory: &Node, entries: &[&Node]) {
    let areas = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(if browser.help { 10 } else { 4 }),
    ])
    .split(frame.area());
    draw_header(frame, areas[0], browser, directory);
    draw_entries(
        frame,
        areas[1],
        directory,
        entries,
        browser.apparent,
        &mut browser.table_state,
    );
    draw_footer(frame, areas[2], browser, directory, entries);
    if let Some(confirmation) = &browser.confirmation {
        draw_confirmation(frame, confirmation);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, browser: &Browser, directory: &Node) {
    let sort = if browser.sort_name {
        "name"
    } else if browser.apparent {
        "logical size"
    } else {
        "disk usage"
    };
    let text = format!(
        "{}\nDisk: {} | Logical: {} | {} items | {} errors | sort: {}",
        safe_text(&directory.path.to_string_lossy()),
        human_size(directory.allocated),
        human_size(directory.apparent),
        directory.items,
        directory.errors,
        sort
    );
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::BOTTOM).title("whatdu")),
        area,
    );
}

fn entry_row(node: &Node, total: u64, apparent: bool) -> Row<'static> {
    let percentage = if total == 0 {
        0.0
    } else {
        node.size(apparent) as f64 / total as f64 * 100.0
    };
    let suffix = if node.is_dir {
        "/"
    } else if node.is_symlink {
        "@"
    } else {
        ""
    };
    let name = format!(
        "{}{}{}",
        safe_text(&node.name()),
        suffix,
        if node.errors > 0 { " [errors]" } else { "" }
    );
    Row::new(vec![
        Cell::from(human_size(node.size(apparent))),
        Cell::from(human_size(node.size(!apparent))),
        Cell::from(format!("{percentage:5.1}%")),
        Cell::from(node.items.to_string()),
        Cell::from(name),
    ])
}

fn draw_entries(
    frame: &mut Frame,
    area: Rect,
    directory: &Node,
    entries: &[&Node],
    apparent: bool,
    state: &mut TableState,
) {
    let rows = entries
        .iter()
        .map(|node| entry_row(node, directory.size(apparent), apparent));
    let labels = if apparent {
        ["Logical", "Disk usage"]
    } else {
        ["Disk usage", "Logical"]
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Min(1),
        ],
    )
    .header(
        Row::new([labels[0], labels[1], "%", "Items", "Name"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("> ");
    frame.render_stateful_widget(table, area, state);
}

fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    browser: &Browser,
    directory: &Node,
    entries: &[&Node],
) {
    let selected = browser.stack.last().unwrap().1;
    let details = if !browser.status.is_empty() {
        browser.status.as_str()
    } else {
        entries
            .get(selected)
            .and_then(|node| node.error.as_deref())
            .or(directory.error.as_deref())
            .unwrap_or("")
    };
    let text = if browser.help {
        format!("Up/Down or k/j: select\nEnter/Right/l: open directory\nLeft/Backspace/h: parent directory\na: toggle disk usage/logical size | s: sort by size/name\n/: filter | Esc: clear filter or close help\nHome/End: first/last | PgUp/PgDn: move page\nd: permanently delete selected entry | r: rescan current directory\nq or Ctrl-C: quit | ?: toggle this help\n{}", size_explanation(browser.apparent))
    } else {
        let keys = if browser.editing_filter {
            "Filter (Enter to apply, Esc to clear):"
        } else if browser.ask_delete {
            "q quit | ? help | arrows navigate | a size | s sort | d delete | r rescan | / filter"
        } else {
            "q quit | ? help | d DELETE WITHOUT ASKING | r rescan | / filter"
        };
        format!(
            "{} /{}\n{}\n{}",
            keys,
            safe_text(&browser.filter),
            size_explanation(browser.apparent),
            safe_text(details)
        )
    };
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::TOP)),
        area,
    );
}

fn draw_confirmation(frame: &mut Frame, confirmation: &Confirmation) {
    let area = frame.area();
    let width = area.width.min(76);
    let height = area.height.min(12);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let choices = ["Yes", "No", "Don't ask me again"]
        .iter()
        .enumerate()
        .map(|(index, label)| {
            format!(
                "{} {}",
                if index == confirmation.choice {
                    ">"
                } else {
                    " "
                },
                label
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = format!("Permanently delete (directories recursively)?\n{}\n\n{}\n\nDon't ask me again deletes now and skips prompts this session.\nArrows/Tab: choose | Enter: confirm | Esc: cancel", safe_text(&confirmation.path.to_string_lossy()), choices);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Delete permanently (not Trash) "),
        ),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn progress_screen_displays_live_counts_and_refresh_phase() {
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        let progress = crate::delete::Progress::default();
        progress.checked.store(100, Ordering::Relaxed);
        progress.removed.store(42, Ordering::Relaxed);
        progress.removed_allocated.store(8192, Ordering::Relaxed);
        progress.phase.store(1, Ordering::Relaxed);
        terminal
            .draw(|frame| {
                draw_delete_progress(
                    frame,
                    Path::new("/test"),
                    &progress,
                    4,
                    Duration::from_secs(2),
                )
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("42 entries deleted"));
        assert!(text.contains("100 entries checked"));
        assert!(text.contains("4 workers"));
        assert!(text.contains("8.0 KiB allocated processed"));
        assert!(text.contains("NOT space freed"));
        progress.phase.store(2, Ordering::Relaxed);
        progress.cancel.store(true, Ordering::Relaxed);
        terminal
            .draw(|frame| {
                draw_delete_progress(
                    frame,
                    Path::new("/test"),
                    &progress,
                    4,
                    Duration::from_secs(3),
                )
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Updating totals"));
        assert!(text.contains("Cancellation requested"));
    }

    #[test]
    fn rescan_progress_shows_allocated_space() {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let progress = crate::scan::Progress::default();
        progress.allocated.store(1024 * 1024, Ordering::Relaxed);
        terminal
            .draw(|frame| {
                draw_rescan_progress(frame, Path::new("/test"), &progress, Duration::from_secs(1))
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("1.0 MiB allocated scanned"));
    }
}
