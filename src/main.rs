mod delete;
mod platform;
mod scan;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{atomic::Ordering, mpsc},
    time::Duration,
};

#[derive(Parser)]
#[command(version, about = "What’s using your disk, and what to do about it.")]
struct Args {
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Number of scan/deletion workers (defaults to available CPU parallelism)
    #[arg(short = 't', long, value_parser = clap::value_parser!(u16).range(1..))]
    threads: Option<u16>,
    /// Display logical file lengths instead of allocated disk space
    #[arg(short = 'a', long)]
    apparent_size: bool,
    /// Do not cross filesystem boundaries
    #[arg(short = 'x', long)]
    one_file_system: bool,
    /// Count each hard link separately instead of deduplicating
    #[arg(long)]
    count_links: bool,
    /// Print immediate children, without opening the interactive browser
    #[arg(long, conflicts_with = "json")]
    no_ui: bool,
    /// Export the complete scanned tree as JSON
    #[arg(long)]
    json: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("whatdu: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let path = std::path::absolute(&args.path).context("cannot resolve scan path")?;
    let threads = args.threads.map(usize::from).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
    });
    let options = scan::Options {
        threads,
        one_file_system: args.one_file_system,
        count_links: args.count_links,
    };
    let mut root = scan_with_progress(&path, &options)?;
    if args.json {
        let mut output = io::BufWriter::new(io::stdout().lock());
        serde_json::to_writer_pretty(&mut output, &root)?;
        writeln!(output)?;
        output.flush()?;
    } else if args.no_ui || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print_summary(&root, args.apparent_size)?;
    } else {
        ui::browse(&mut root, args.apparent_size, &options)?;
    }
    if root.errors > 0 {
        eprintln!(
            "whatdu: {} scan errors; totals may be incomplete",
            root.errors
        );
        std::process::exit(2);
    }
    Ok(())
}

fn scan_with_progress(path: &Path, options: &scan::Options) -> Result<scan::Node> {
    let progress = scan::Progress::default();
    std::thread::scope(|scope| {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        if io::stderr().is_terminal() {
            let progress = &progress;
            scope.spawn(move || {
                while done_rx.recv_timeout(Duration::from_millis(100)).is_err() {
                    eprint!(
                        "\rScanning with {} workers: {} entries, {} allocated scanned, {} errors",
                        options.threads,
                        progress.entries.load(Ordering::Relaxed),
                        ui::human_size(progress.allocated.load(Ordering::Relaxed)),
                        progress.errors.load(Ordering::Relaxed)
                    );
                    let _ = io::stderr().flush();
                }
                eprint!("\r\x1b[2K");
            });
        }
        let result = scan::scan(path, options, &progress);
        let _ = done_tx.send(());
        result
    })
    .with_context(|| format!("cannot scan {}", path.display()))
}

fn print_summary(root: &scan::Node, apparent: bool) -> Result<()> {
    let mut children: Vec<_> = root.children.iter().collect();
    children.sort_by(|a, b| {
        b.size(apparent)
            .cmp(&a.size(apparent))
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut output = io::BufWriter::new(io::stdout().lock());
    writeln!(
        output,
        "{:>10}  {:>10}  Name",
        if apparent { "Logical" } else { "Disk usage" },
        if apparent { "Disk usage" } else { "Logical" }
    )?;
    for child in children {
        writeln!(
            output,
            "{:>10}  {:>10}  {}{}{}",
            ui::human_size(child.size(apparent)),
            ui::human_size(child.size(!apparent)),
            ui::safe_text(&child.name()),
            if child.is_dir {
                "/"
            } else if child.is_symlink {
                "@"
            } else {
                ""
            },
            if child.errors > 0 { " [errors]" } else { "" }
        )?;
    }
    writeln!(
        output,
        "{:>10}  {:>10}  total: {} ({} items, {} errors)",
        ui::human_size(root.size(apparent)),
        ui::human_size(root.size(!apparent)),
        ui::safe_text(&root.path.to_string_lossy()),
        root.items,
        root.errors
    )?;
    writeln!(output, "{}", ui::size_explanation(apparent))?;
    output.flush()?;
    Ok(())
}
