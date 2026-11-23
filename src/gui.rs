//! Window UI: a "Receive files" tab for this PC and a "Share files" tab for the remote one.

use crate::engine::{self, Entry, Link, Log, Progress, SyncJob, human};
use eframe::egui::{self, Color32, RichText};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ftx - file transfer")
            .with_inner_size([780.0, 720.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).unwrap_or_default()),
        ..Default::default()
    };
    eframe::run_native("ftx", options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    share_dir: String,
    share_port: u16,
    share_password: String,
    /// Receiver to connect out to (when the receiver waits for us).
    share_connect: String,
    host: String,
    /// Receive by waiting for the sharing PC to connect in.
    recv_wait: bool,
    recv_port: u16,
    password: String,
    dest: String,
    jobs: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            share_dir: String::new(),
            share_port: engine::DEFAULT_PORT,
            share_password: new_password(),
            share_connect: String::new(),
            host: String::new(),
            recv_wait: false,
            recv_port: engine::DEFAULT_PORT + 1,
            password: String::new(),
            dest: String::new(),
            jobs: 4,
        }
    }
}

/// %APPDATA%\ftx, ~/Library/Application Support/ftx or ~/.config/ftx.
fn config_path() -> PathBuf {
    let env_dir = |name| std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let base = if cfg!(windows) {
        env_dir("APPDATA")
    } else if cfg!(target_os = "macos") {
        env_dir("HOME").map(|h| h.join("Library/Application Support"))
    } else {
        env_dir("XDG_CONFIG_HOME").or_else(|| env_dir("HOME").map(|h| h.join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("ftx").join("config.json")
}

fn new_password() -> String {
    engine::random_hex(16)
}

const FIREWALL_HINT: &str = if cfg!(windows) {
    "- If Windows asks whether to allow network access, click Allow."
} else if cfg!(target_os = "macos") {
    "- If macOS asks whether to accept incoming network connections, click Allow."
} else {
    "- If a firewall is enabled (e.g. ufw), allow the port above."
};

type Lines = Arc<Mutex<Vec<String>>>;

fn make_log(lines: &Lines, ctx: &egui::Context) -> Log {
    let (lines, ctx) = (lines.clone(), ctx.clone());
    Arc::new(move |s| {
        let mut l = lines.lock().unwrap();
        if l.len() > 5000 {
            l.drain(..1000);
        }
        l.push(s);
        ctx.request_repaint();
    })
}

struct Running<T> {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<T>,
}

/// A file list being fetched in the background, with its cancel flag.
type Listing = (Arc<AtomicBool>, JoinHandle<io::Result<Vec<Entry>>>);
/// A running download (its result is the list of failed files) and its progress.
type Download = (Running<io::Result<Vec<String>>>, Arc<Progress>);

struct Group {
    name: String,
    files: Vec<usize>,
    size: u64,
}

#[derive(PartialEq)]
enum Tab {
    Receive,
    Share,
}

struct App {
    cfg: Config,
    tab: Tab,

    share_log: Lines,
    sharing: Option<Running<io::Result<()>>>,
    share_error: Option<String>,
    lan_ip: Option<String>,
    public: engine::PublicAddr,

    recv_log: Lines,
    /// Listening for the sharing PC (receiver-waits mode): stop flag, link, UPnP status.
    waiting: Option<(Arc<AtomicBool>, Link, engine::PublicAddr)>,
    link: Option<Link>,
    listing: Option<Listing>,
    files: Vec<Entry>,
    selected: Vec<bool>,
    groups: Vec<Group>,
    recv_error: Option<String>,
    download: Option<Download>,
    result: Option<(bool, String)>,
    recheck: bool,
    /// (sample time, received bytes then, smoothed speed)
    speed: (Instant, u64, f64),
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        ctx.set_zoom_factor(1.15);
        let cfg = std::fs::read(config_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        App {
            cfg,
            tab: Tab::Receive,
            share_log: Lines::default(),
            sharing: None,
            share_error: None,
            lan_ip: None,
            public: engine::PublicAddr::default(),
            recv_log: Lines::default(),
            waiting: None,
            link: None,
            listing: None,
            files: Vec::new(),
            selected: Vec::new(),
            groups: Vec::new(),
            recv_error: None,
            download: None,
            result: None,
            recheck: false,
            speed: (Instant::now(), 0, 0.0),
        }
    }

    fn save_config(&self) {
        let p = config_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(b) = serde_json::to_vec_pretty(&self.cfg) {
            let _ = std::fs::write(p, b);
        }
    }

    /// Collect results of finished background threads.
    fn poll(&mut self) {
        if self.listing.as_ref().is_some_and(|(_, h)| h.is_finished()) {
            match self.listing.take().unwrap().1.join().unwrap() {
                Ok(files) => self.set_files(files),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => self.recv_error = Some(connect_error(&e)),
            }
        }
        if self.sharing.as_ref().is_some_and(|r| r.handle.is_finished())
            && let Err(e) = self.sharing.take().unwrap().handle.join().unwrap()
        {
            self.share_error = Some(format!("Could not share: {e}"));
        }
        if self.download.as_ref().is_some_and(|(r, _)| r.handle.is_finished()) {
            let (run, p) = self.download.take().unwrap();
            let stopped = run.stop.load(Relaxed);
            self.result = Some(match run.handle.join().unwrap() {
                Err(e) => (false, format!("Error: {e}")),
                Ok(failed) if !failed.is_empty() => (
                    false,
                    format!("{} file(s) failed - press Start again to retry them (see details below).", failed.len()),
                ),
                Ok(_) if stopped => (false, "Stopped. Press Start to continue later - nothing is lost.".into()),
                Ok(_) => (
                    true,
                    format!(
                        "All done! Downloaded {}, reused {} that was already here.",
                        human(p.received.load(Relaxed)),
                        human(p.reused.load(Relaxed) + p.already_bytes.load(Relaxed))
                    ),
                ),
            });
        }
    }

    fn set_files(&mut self, files: Vec<Entry>) {
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        for (i, e) in files.iter().enumerate() {
            let name = engine::top_level(&e.path).to_string();
            let g = groups.entry(name.clone()).or_insert(Group { name, files: Vec::new(), size: 0 });
            g.files.push(i);
            g.size += e.size;
        }
        self.groups = groups.into_values().collect();
        self.selected = vec![true; files.len()];
        self.files = files;
        self.recv_error = None;
    }

    fn start_listing(&mut self, ctx: &egui::Context) {
        self.save_config();
        self.recv_error = None;
        let link = if self.cfg.recv_wait {
            if self.waiting.is_none() {
                let (stop, public) = (Arc::new(AtomicBool::new(false)), engine::PublicAddr::default());
                let log = make_log(&self.recv_log, ctx);
                match engine::wait_for_host(self.cfg.recv_port, stop.clone(), public.clone(), log) {
                    Ok(link) => self.waiting = Some((stop, link, public)),
                    Err(e) => {
                        self.recv_error = Some(format!("Could not wait on port {}: {e}", self.cfg.recv_port));
                        return;
                    }
                }
            }
            self.lan_ip = lan_ip();
            self.waiting.as_ref().unwrap().1.clone()
        } else {
            Link::Dial(engine::with_port(&self.cfg.host))
        };
        self.link = Some(link.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let (pw, ctx, cancel2) = (self.cfg.password.clone(), ctx.clone(), cancel.clone());
        let handle = thread::spawn(move || {
            let r = loop {
                if cancel2.load(Relaxed) {
                    break Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                match engine::fetch_list(&link, &pw) {
                    // Waiting mode: keep waiting until the sharing PC dials in.
                    Err(e) if e.kind() == io::ErrorKind::TimedOut && matches!(link, Link::Wait(_)) => {}
                    r => break r,
                }
            };
            ctx.request_repaint();
            r
        });
        self.listing = Some((cancel, handle));
    }

    fn stop_waiting(&mut self) {
        if let Some((stop, _, _)) = self.waiting.take() {
            stop.store(true, Relaxed);
        }
        if let Some((cancel, _)) = &self.listing {
            cancel.store(true, Relaxed);
        }
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        self.save_config();
        let files = self.files.iter().zip(&self.selected).filter(|(_, s)| **s).map(|(e, _)| e.clone()).collect();
        let Some(link) = self.link.clone() else { return };
        let job = SyncJob {
            link,
            token: self.cfg.password.clone(),
            dest: PathBuf::from(self.cfg.dest.trim()),
            files,
            jobs: self.cfg.jobs.max(1),
            recheck: self.recheck,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let p = Arc::new(Progress::default());
        let log = make_log(&self.recv_log, ctx);
        let (stop2, p2) = (stop.clone(), p.clone());
        let handle = thread::spawn(move || engine::sync(&job, log, &stop2, &p2));
        self.download = Some((Running { stop, handle }, p));
        self.result = None;
        self.speed = (Instant::now(), 0, 0.0);
    }

    fn start_sharing(&mut self, ctx: &egui::Context) {
        self.save_config();
        self.share_error = None;
        self.lan_ip = lan_ip();
        let stop = Arc::new(AtomicBool::new(false));
        let log = make_log(&self.share_log, ctx);
        let dir = PathBuf::from(self.cfg.share_dir.trim());
        let (bind, pw, stop2) = (format!("0.0.0.0:{}", self.cfg.share_port), self.cfg.share_password.clone(), stop.clone());
        self.public = engine::PublicAddr::default();
        let public = self.public.clone();
        let dial = Some(self.cfg.share_connect.trim()).filter(|a| !a.is_empty()).map(engine::with_port);
        let handle = thread::spawn(move || engine::serve(&dir, &bind, &pw, log, stop2, public, dial));
        self.sharing = Some(Running { stop, handle });
    }

    fn receive_ui(&mut self, ui: &mut egui::Ui) {
        let busy = self.download.is_some();
        let connecting = self.listing.is_some();

        ui.heading("1. Connect to the PC that has the files");
        let was_wait = self.cfg.recv_wait;
        ui.add_enabled_ui(!busy && !connecting, |ui| {
            ui.radio_value(&mut self.cfg.recv_wait, false, "I connect to the sharing PC");
            ui.radio_value(
                &mut self.cfg.recv_wait,
                true,
                "The sharing PC connects to me  (use this if the first way can't reach it)",
            );
        });
        if was_wait && !self.cfg.recv_wait {
            self.stop_waiting();
        }
        egui::Grid::new("conn").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            if !self.cfg.recv_wait {
                ui.label("Address:");
                ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut self.cfg.host).hint_text("e.g. 203.0.113.5:9000").desired_width(280.0),
                );
                ui.end_row();
            }
            ui.label("Password:");
            ui.add_enabled(!busy, egui::TextEdit::singleline(&mut self.cfg.password).password(true).desired_width(280.0));
            ui.end_row();
            if self.cfg.recv_wait {
                ui.label("Port:");
                ui.add_enabled(
                    !busy && self.waiting.is_none(),
                    egui::DragValue::new(&mut self.cfg.recv_port).range(1..=65535),
                );
                ui.end_row();
            }
        });
        ui.label(
            RichText::new("Use the password shown in the \"Share files\" tab on the other PC.").weak(),
        );
        ui.horizontal(|ui| {
            let (label, can) = if self.cfg.recv_wait {
                ("Wait for the sharing PC", !busy && !connecting)
            } else {
                ("Connect", !busy && !connecting && !self.cfg.host.trim().is_empty())
            };
            if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                self.start_listing(ui.ctx());
            }
            if connecting {
                ui.spinner();
                ui.label(if self.cfg.recv_wait { "Waiting for the sharing PC to connect..." } else { "Connecting..." });
                if ui.button("Cancel").clicked()
                    && let Some((cancel, _)) = &self.listing
                {
                    cancel.store(true, Relaxed);
                }
            }
        });
        if let Some((_, _, public)) = &self.waiting {
            let port = self.cfg.recv_port;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                let public = public.lock().unwrap().clone();
                match &public {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Asking the router to open the connection...");
                        });
                    }
                    Some(Ok(addr)) => {
                        ui.label(RichText::new("Send this to the person sharing the files:").strong());
                        ui.horizontal(|ui| {
                            ui.monospace(RichText::new(addr).size(16.0));
                            if ui.small_button("Copy").clicked() {
                                ui.ctx().copy_text(addr.clone());
                            }
                        });
                        ui.label(
                            "They enter it in \"Receiver's address\" in their Share files tab and press Start sharing.",
                        );
                    }
                    Some(Err(e)) => {
                        ui.label(
                            RichText::new(format!("Could not open the connection automatically: {e}."))
                                .color(Color32::from_rgb(220, 140, 30)),
                        );
                        ui.label(format!(
                            "This way only works if one of the two PCs can be reached. On the same network, \
                             the sharing PC can use {}:{port}.",
                            self.lan_ip.as_deref().unwrap_or("this PC's local address")
                        ));
                    }
                }
            });
        }
        if let Some(err) = &self.recv_error {
            ui.colored_label(Color32::from_rgb(220, 60, 60), err);
        }
        if self.files.is_empty() {
            return;
        }

        ui.separator();
        ui.heading("2. Choose what to download");
        let (n_sel, sel_bytes) = self
            .files
            .iter()
            .zip(&self.selected)
            .filter(|(_, s)| **s)
            .fold((0, 0), |(n, b), (e, _)| (n + 1, b + e.size));
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("Select all")).clicked() {
                self.selected.iter_mut().for_each(|s| *s = true);
            }
            if ui.add_enabled(!busy, egui::Button::new("Select none")).clicked() {
                self.selected.iter_mut().for_each(|s| *s = false);
            }
            ui.label(format!("Selected: {n_sel} of {} files, {}", self.files.len(), human(sel_bytes)));
        });
        egui::Frame::group(ui.style()).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("files").max_height(230.0).auto_shrink([false, true]).show(ui, |ui| {
                for (gi, g) in self.groups.iter().enumerate() {
                    let n = g.files.iter().filter(|&&i| self.selected[i]).count();
                    let mut all = n == g.files.len();
                    let single = g.files.len() == 1 && self.files[g.files[0]].path == g.name;
                    if single {
                        let label = format!("{}   ({})", g.name, human(g.size));
                        ui.add_enabled(!busy, egui::Checkbox::new(&mut self.selected[g.files[0]], label));
                        continue;
                    }
                    let label = format!("{}   ({}, {} files)", g.name, human(g.size), g.files.len());
                    let id = ui.make_persistent_id(("group", gi));
                    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
                        .show_header(ui, |ui| {
                            let partial = n > 0 && !all;
                            let cb = egui::Checkbox::new(&mut all, label).indeterminate(partial);
                            if ui.add_enabled(!busy, cb).changed() {
                                for &i in &g.files {
                                    self.selected[i] = all;
                                }
                            }
                        })
                        .body(|ui| {
                            for &i in &g.files {
                                let e = &self.files[i];
                                let label = format!("{}   ({})", &e.path[g.name.len() + 1..], human(e.size));
                                ui.add_enabled(!busy, egui::Checkbox::new(&mut self.selected[i], label));
                            }
                        });
                }
            });
        });

        ui.separator();
        ui.heading("3. Where to save");
        ui.horizontal(|ui| {
            ui.add_enabled(!busy, egui::TextEdit::singleline(&mut self.cfg.dest).desired_width(420.0));
            if ui.add_enabled(!busy, egui::Button::new("Browse...")).clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                self.cfg.dest = p.display().to_string();
            }
        });
        ui.label(
            RichText::new(
                "Already have part of these files (e.g. from a torrent)? Pick the same folder: what is \
                 there gets checked and only missing or damaged pieces are downloaded. Close the torrent \
                 program first.",
            )
            .weak(),
        );
        ui.add_enabled(!busy, egui::Checkbox::new(&mut self.recheck, "Re-check files that were already finished"));

        ui.add_space(8.0);
        if let Some((run, _)) = &self.download {
            let stopping = run.stop.load(Relaxed);
            let btn = egui::Button::new(RichText::new(if stopping { "Stopping..." } else { "Stop" }).size(18.0));
            if ui.add_enabled(!stopping, btn).clicked() {
                run.stop.store(true, Relaxed);
            }
        } else {
            let ready = n_sel > 0 && !self.cfg.dest.trim().is_empty();
            let btn = egui::Button::new(RichText::new("Start download").size(18.0)).fill(Color32::from_rgb(40, 120, 200));
            if ui.add_enabled(ready, btn).clicked() {
                self.start_download(ui.ctx());
            }
            if !ready {
                ui.label(RichText::new("Select some files and a folder to save to.").weak());
            }
        }

        if let Some((_, p)) = &self.download {
            let recv = p.received.load(Relaxed);
            let dt = self.speed.0.elapsed().as_secs_f64();
            if dt >= 2.0 {
                let now = (recv - self.speed.1) as f64 / dt;
                let smooth = if self.speed.2 == 0.0 { now } else { self.speed.2 * 0.6 + now * 0.4 };
                self.speed = (Instant::now(), recv, smooth);
            }
            let (total, done) = (p.total_bytes.load(Relaxed), p.done_bytes());
            let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).show_percentage().desired_height(22.0));
            let mut line = format!("{} of {}   |   {}/s", human(done), human(total), human(self.speed.2 as u64));
            if self.speed.2 > 1.0 {
                line += &format!("   |   about {} left", eta(total.saturating_sub(done) as f64 / self.speed.2));
            }
            ui.label(line);
            ui.label(format!(
                "Files: {} done, {} failed, {} total",
                p.files_done.load(Relaxed),
                p.files_failed.load(Relaxed),
                p.files_total.load(Relaxed)
            ));
            let hashed = p.hashed.load(Relaxed);
            if hashed > 0 {
                ui.label(format!(
                    "Checked existing data: {} (kept {} of good data)",
                    human(hashed),
                    human(p.reused.load(Relaxed))
                ));
            }
            for a in p.active.lock().unwrap().iter().filter(|a| !a.is_empty()) {
                ui.label(RichText::new(format!("- {a}")).small());
            }
        }
        if let Some((ok, msg)) = &self.result {
            let color = if *ok { Color32::from_rgb(40, 160, 70) } else { Color32::from_rgb(220, 140, 30) };
            ui.label(RichText::new(msg).color(color).strong());
        }
        ui.add_space(6.0);
        log_view(ui, &self.recv_log, "recv_log");
    }

    fn share_ui(&mut self, ui: &mut egui::Ui) {
        let running = self.sharing.is_some();
        ui.heading("Share files from this PC");
        ui.label("Use this on the PC that HAS the files. The other PC uses the \"Receive files\" tab.");
        ui.add_space(4.0);
        egui::Grid::new("share").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Folder to share:");
            ui.add_enabled_ui(!running, |ui| ui.add_sized([360.0, 20.0], egui::TextEdit::singleline(&mut self.cfg.share_dir)));
            if ui.add_enabled(!running, egui::Button::new("Browse...")).clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                self.cfg.share_dir = p.display().to_string();
            }
            ui.end_row();
            ui.label("Password:");
            ui.add_enabled_ui(!running, |ui| ui.add_sized([360.0, 20.0], egui::TextEdit::singleline(&mut self.cfg.share_password)));
            if ui.add_enabled(!running, egui::Button::new("New")).clicked() {
                self.cfg.share_password = new_password();
            }
            ui.end_row();
            ui.label("Port:");
            ui.add_enabled(!running, egui::DragValue::new(&mut self.cfg.share_port).range(1..=65535));
            ui.end_row();
            ui.label("Receiver's address:");
            ui.add_enabled_ui(!running, |ui| {
                ui.add_sized(
                    [360.0, 20.0],
                    egui::TextEdit::singleline(&mut self.cfg.share_connect).hint_text("optional - only if the receiver gives you one"),
                )
            });
            ui.end_row();
        });
        ui.label(
            RichText::new(
                "Leave \"Receiver's address\" empty unless the receiver chose \"The sharing PC connects to me\" \
                 and sent you an address.",
            )
            .weak(),
        );
        ui.add_space(8.0);
        if let Some(run) = &self.sharing {
            if ui.button(RichText::new("Stop sharing").size(18.0)).clicked() {
                run.stop.store(true, Relaxed);
            }
        } else {
            let ready = !self.cfg.share_dir.trim().is_empty() && !self.cfg.share_password.is_empty();
            let btn = egui::Button::new(RichText::new("Start sharing").size(18.0)).fill(Color32::from_rgb(40, 120, 200));
            if ui.add_enabled(ready, btn).clicked() {
                self.start_sharing(ui.ctx());
            }
        }
        if let Some(err) = &self.share_error {
            ui.colored_label(Color32::from_rgb(220, 60, 60), err);
        }

        if running {
            ui.add_space(8.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new("Sharing is ON. Send these to the person receiving the files:").strong());
                ui.label(RichText::new("(These are the addresses of THIS computer - the one sharing the files.)").weak());
                let port = self.cfg.share_port;
                let public = self.public.lock().unwrap().clone();
                let upnp_failed = matches!(public, Some(Err(_)));
                match &public {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Asking the router to open the connection (like torrent programs do)...");
                        });
                    }
                    Some(Ok(_)) => {
                        ui.label(
                            RichText::new("The router opened the connection automatically - no router setup needed.")
                                .color(Color32::from_rgb(40, 160, 70)),
                        );
                    }
                    Some(Err(e)) => {
                        ui.label(
                            RichText::new(format!("Could not open the connection automatically: {e}."))
                                .color(Color32::from_rgb(220, 140, 30)),
                        );
                        if self.cfg.share_connect.trim().is_empty() {
                            ui.label(
                                "People outside this network can't connect to this PC. Ask the receiver to choose \
                                 \"The sharing PC connects to me\" and send you their address; put it in \
                                 \"Receiver's address\" above and start sharing again.",
                            );
                        }
                    }
                }
                if !self.cfg.share_connect.trim().is_empty() {
                    ui.label(
                        RichText::new(format!(
                            "Also connecting out to the receiver at {} - they will see the files as soon as it connects.",
                            engine::with_port(&self.cfg.share_connect)
                        ))
                        .color(Color32::from_rgb(40, 160, 70)),
                    );
                }
                let rows = [
                    ("Address (over the internet):", public.and_then(Result::ok)),
                    ("Address (same home/office network):", self.lan_ip.as_ref().map(|ip| format!("{ip}:{port}"))),
                    ("Password:", Some(self.cfg.share_password.clone())),
                ];
                egui::Grid::new("info").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                    for (label, value) in rows {
                        ui.label(label);
                        match value {
                            Some(v) => {
                                ui.monospace(RichText::new(&v).size(16.0));
                                if ui.small_button("Copy").clicked() {
                                    ui.ctx().copy_text(v);
                                }
                            }
                            None => {
                                ui.label(RichText::new(if upnp_failed { "not available" } else { "..." }).weak());
                            }
                        }
                        ui.end_row();
                    }
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("{FIREWALL_HINT}\n- Keep this window open until the transfer finishes."))
                    .small(),
                );
            });
        }
        ui.add_space(6.0);
        log_view(ui, &self.share_log, "share_log");
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        if self.download.is_some() || self.sharing.is_some() || self.listing.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let recv = if self.download.is_some() { "Receive files (running)" } else { "Receive files" };
                let share = if self.sharing.is_some() { "Share files (on)" } else { "Share files" };
                ui.selectable_value(&mut self.tab, Tab::Receive, RichText::new(recv).size(17.0));
                ui.selectable_value(&mut self.tab, Tab::Share, RichText::new(share).size(17.0));
            });
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("page").show(ui, |ui| match self.tab {
                Tab::Receive => self.receive_ui(ui),
                Tab::Share => self.share_ui(ui),
            });
        });
    }
}

fn log_view(ui: &mut egui::Ui, lines: &Lines, id: &str) {
    egui::CollapsingHeader::new("Details").id_salt(id).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt(id).max_height(200.0).stick_to_bottom(true).show(ui, |ui| {
            for l in lines.lock().unwrap().iter() {
                ui.label(RichText::new(l).monospace().small());
            }
        });
    });
}

fn connect_error(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::PermissionDenied => "Wrong password - use the one shown in the other PC's \"Share files\" tab.".into(),
        io::ErrorKind::TimedOut | io::ErrorKind::ConnectionRefused => format!(
            "Could not reach the other PC ({e}). Check the address, that sharing is ON there, \
             and that the firewall/router lets the connection through."
        ),
        _ => format!("Could not connect: {e}"),
    }
}

fn eta(secs: f64) -> String {
    let s = secs as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

/// Address of this PC on the local network.
fn lan_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

