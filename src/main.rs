//! ftx - resumable, chunk-verified file transfer from a host to a client.
//! Run without arguments for the window UI, or use the subcommands below.

mod engine;
mod gui;

use clap::{Parser, Subcommand};
use engine::{Entry, Link, Progress, SyncJob, human};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(about = "Resumable, chunk-verified file transfer (host -> client). Run without arguments for the UI.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Share a directory (run this on the remote PC)
    Serve {
        dir: PathBuf,
        #[arg(long, default_value = "0.0.0.0:9000")]
        bind: String,
        #[arg(long, env = "FTX_TOKEN")]
        token: String,
        /// Also connect out to a receiver started with `get --wait` (host:port);
        /// use when this PC can't be reached from the internet
        #[arg(long)]
        connect: Option<String>,
    },
    /// Show what the host shares
    List {
        /// host[:port]
        host: String,
        #[arg(long, env = "FTX_TOKEN")]
        token: String,
        /// List every file instead of a top-level summary
        #[arg(long)]
        all: bool,
    },
    /// Download into DEST, verifying and reusing whatever is already there
    Get {
        /// host[:port] of the sharing PC (omit with --wait)
        #[arg(required_unless_present = "wait")]
        host: Option<String>,
        #[arg(long)]
        dest: PathBuf,
        /// Instead of connecting, wait on this port for the sharing PC to
        /// connect in (`serve --connect`)
        #[arg(long)]
        wait: Option<u16>,
        #[arg(long, env = "FTX_TOKEN")]
        token: String,
        /// Only paths matching this glob (repeatable), e.g. "Movies/*"
        #[arg(long)]
        include: Vec<String>,
        /// Skip paths matching this glob (repeatable)
        #[arg(long)]
        exclude: Vec<String>,
        /// Interactively choose top-level folders/files
        #[arg(long)]
        pick: bool,
        /// Files transferred in parallel
        #[arg(short, long, default_value_t = 4)]
        jobs: usize,
        /// Re-verify files already recorded as complete
        #[arg(long)]
        recheck: bool,
    },
}

fn print_summary(groups: &[(String, u64, usize)]) {
    for (i, (name, size, n)) in groups.iter().enumerate() {
        println!("{:>4}  {:>10}  {:>6} files  {name}", i + 1, human(*size), n);
    }
}

fn pick(files: Vec<Entry>) -> io::Result<Vec<Entry>> {
    let groups = engine::summarize(&files);
    print_summary(&groups);
    print!("select (e.g. 1,3,5-7 or 'all'): ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let line = line.trim();
    if line.eq_ignore_ascii_case("all") {
        return Ok(files);
    }
    let mut chosen = Vec::new();
    for part in line.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let parse = |s: &str| s.trim().parse::<usize>().map_err(|_| io::Error::other(format!("bad selection: {part}")));
        for i in parse(a)?..=parse(b)? {
            let g = groups.get(i.wrapping_sub(1)).ok_or_else(|| io::Error::other(format!("no item {i}")))?;
            chosen.push(g.0.as_str());
        }
    }
    Ok(files.into_iter().filter(|e| chosen.contains(&engine::top_level(&e.path))).collect())
}

fn glob_set(patterns: &[String]) -> io::Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(GlobBuilder::new(p).case_insensitive(true).build().map_err(io::Error::other)?);
    }
    b.build().map_err(io::Error::other)
}

fn stdout_log() -> engine::Log {
    Arc::new(|s| println!("{s}"))
}

#[allow(clippy::too_many_arguments)]
fn get(
    link: Link,
    token: String,
    dest: PathBuf,
    include: Vec<String>,
    exclude: Vec<String>,
    pick_mode: bool,
    jobs: usize,
    recheck: bool,
) -> io::Result<()> {
    let files = loop {
        match engine::fetch_list(&link, &token) {
            Err(e) if e.kind() == io::ErrorKind::TimedOut && matches!(link, Link::Wait(_)) => {
                println!("waiting for the sharing PC to connect...")
            }
            r => break r?,
        }
    };
    let (inc, exc) = (glob_set(&include)?, glob_set(&exclude)?);
    let mut files: Vec<Entry> = files
        .into_iter()
        .filter(|e| (include.is_empty() || inc.is_match(&e.path)) && !exc.is_match(&e.path))
        .collect();
    if pick_mode {
        files = pick(files)?;
    }
    let job = SyncJob { link, token, dest, files, jobs, recheck };
    let (stop, p, finished) = (AtomicBool::new(false), Progress::default(), AtomicBool::new(false));
    let start = Instant::now();
    let failed = thread::scope(|s| {
        s.spawn(|| {
            let mut last = (Instant::now(), 0u64);
            let mut tick = 0;
            while !finished.load(Relaxed) {
                thread::sleep(Duration::from_secs(1));
                tick += 1;
                if tick % 10 != 0 {
                    continue;
                }
                let recv = p.received.load(Relaxed);
                let rate = (recv - last.1) as f64 / last.0.elapsed().as_secs_f64();
                last = (Instant::now(), recv);
                println!(
                    "[{}/{} files] {} of {} | {}/s | checked local {}",
                    p.files_done.load(Relaxed),
                    p.files_total.load(Relaxed),
                    human(p.done_bytes()),
                    human(p.total_bytes.load(Relaxed)),
                    human(rate as u64),
                    human(p.hashed.load(Relaxed)),
                );
            }
        });
        let r = engine::sync(&job, stdout_log(), &stop, &p);
        finished.store(true, Relaxed);
        r
    })?;

    println!(
        "\nfinished in {:.0?}: {} files ok, {} failed, received {}, reused {} of existing data",
        start.elapsed(),
        p.files_done.load(Relaxed),
        failed.len(),
        human(p.received.load(Relaxed)),
        human(p.reused.load(Relaxed)),
    );
    for f in &failed {
        println!("  failed: {f}");
    }
    if !failed.is_empty() {
        println!("run the same command again to retry failed files");
    }
    Ok(())
}

/// When started by double-click, Windows gives us a console window of our own; drop it.
#[cfg(windows)]
fn hide_own_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut ids = [0u32; 2];
    unsafe {
        if GetConsoleProcessList(ids.as_mut_ptr(), 2) <= 1 {
            FreeConsole();
        }
    }
}

#[cfg(not(windows))]
fn hide_own_console() {}

fn main() {
    // Older macOS versions pass "-psn_..." when an app is opened from Finder.
    let args = std::env::args_os().skip(1).filter(|a| !a.to_string_lossy().starts_with("-psn_")).count();
    if args == 0 {
        hide_own_console();
        if let Err(e) = gui::run() {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    let res = match Cli::parse().cmd {
        Cmd::Serve { dir, bind, token, connect } => engine::serve(
            &dir,
            &bind,
            &token,
            stdout_log(),
            Arc::default(),
            Arc::default(),
            connect.as_deref().map(engine::with_port),
        ),
        Cmd::List { host, token, all } => engine::fetch_list(&Link::Dial(engine::with_port(&host)), &token).map(|files| {
            if all {
                for e in &files {
                    println!("{:>10}  {}", human(e.size), e.path);
                }
            } else {
                print_summary(&engine::summarize(&files));
            }
            let total: u64 = files.iter().map(|e| e.size).sum();
            println!("total: {} files, {}", files.len(), human(total));
        }),
        Cmd::Get { host, dest, wait, token, include, exclude, pick, jobs, recheck } => {
            let link = match wait {
                Some(port) => engine::wait_for_host(port, Arc::default(), Arc::default(), stdout_log()),
                None => Ok(Link::Dial(engine::with_port(host.as_deref().unwrap_or_default()))),
            };
            link.and_then(|link| get(link, token, dest, include, exclude, pick, jobs, recheck))
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
