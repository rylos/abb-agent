//! Guided restore in the terminal, with the steps of the Synology recovery wizard: connect,
//! device and task, restore mode, version, disk mapping, summary, progress.

use crate::local::{self, LocalDisk, size_str};
use crate::plan::{self, Mode, Place, Plan, VolumeTarget};
use crate::restore::{self, Event};
use crate::server::Server;
use crate::spec::{Task, Version};
use abb_proto::transport::{DEFAULT_PORT, fingerprint_hex};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const ACCENT: Color = Color::Rgb(0x10, 0x87, 0xe8);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Welcome,
    Login,
    Tasks,
    Mode,
    Versions,
    Mapping,
    Summary,
    Progress,
}

struct Login {
    fields: [String; 4],
    focus: usize,
}

struct Running {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    started: Instant,
    ended: Option<Instant>,
    done: u64,
    total: u64,
    steps: Vec<(bool, String)>,
    result: Option<Result<(), String>>,
}

enum Msg {
    Event(Event),
    Finished(Result<(), String>),
}

struct App {
    page: Page,
    login: Login,
    server: Option<Server>,
    server_version: String,
    tasks: Vec<Task>,
    task_state: TableState,
    mode: usize,
    versions: Vec<Version>,
    version_state: ListState,
    /// Volumes mode: chosen (disk, partition) of the selected version, and the cursor in the
    /// version's volume list.
    chosen: Vec<(usize, usize)>,
    volume_cursor: usize,
    locals: Vec<LocalDisk>,
    plan: Option<Plan>,
    map_cursor: usize,
    grow: bool,
    efi_fallback: bool,
    confirm: bool,
    message: Option<(bool, String)>,
    /// Server waiting for the certificate to be confirmed, with its fingerprint.
    trust: Option<(Server, String)>,
    running: Option<Running>,
    quit: bool,
}

const MODES: [(Mode, &str); 3] = [
    (Mode::Device, "Restore every disk of the backup, partition tables included, onto whole disks of this computer. Everything on those disks is overwritten."),
    (Mode::System, "Restore only the volumes needed to boot (EFI and BIOS boot partitions, / and /boot), in place on the original disk. Use it when the system no longer boots but the data volumes are fine."),
    (Mode::Volumes, "Restore the volumes you choose onto existing partitions or free space of any disk. For advanced users."),
];

pub fn run() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("abb-restore needs root to write to the disks: run it with sudo");
        std::process::exit(1);
    }
    let mut terminal = ratatui::init();
    let mut app = App {
        page: Page::Welcome,
        login: Login { fields: [String::new(), String::new(), String::new(), String::new()], focus: 0 },
        server: None,
        server_version: String::new(),
        tasks: vec![],
        task_state: TableState::default(),
        mode: 0,
        versions: vec![],
        version_state: ListState::default(),
        chosen: vec![],
        volume_cursor: 0,
        locals: local::disks(),
        plan: None,
        map_cursor: 0,
        grow: true,
        efi_fallback: true,
        confirm: false,
        message: None,
        trust: None,
        running: None,
        quit: false,
    };
    let res = app.main_loop(&mut terminal);
    ratatui::restore();
    if let Err(e) = res {
        eprintln!("abb-restore: {e}");
    }
}

fn date(t: u64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = t as i64;
    unsafe { libc::localtime_r(&t as *const i64 as *const _, &mut tm) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

/// IPv4 addresses of the interfaces that are up, and whether there is a default route.
fn network() -> (Vec<String>, bool) {
    let mut out = Vec::new();
    unsafe {
        let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) == 0 {
            let mut p = ifa;
            while !p.is_null() {
                let a = &*p;
                if !a.ifa_addr.is_null() && (*a.ifa_addr).sa_family as i32 == libc::AF_INET {
                    let sin = &*(a.ifa_addr as *const libc::sockaddr_in);
                    let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    let name = std::ffi::CStr::from_ptr(a.ifa_name).to_string_lossy();
                    if !ip.is_loopback() {
                        out.push(format!("{name}: {ip}"));
                    }
                }
                p = a.ifa_next;
            }
            libc::freeifaddrs(ifa);
        }
    }
    let route = std::fs::read_to_string("/proc/net/route")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .any(|l| l.split_whitespace().nth(1) == Some("00000000"));
    (out, route)
}

fn header(f: &mut Frame, area: Rect, title: &str, step: usize) {
    let block = Block::default().style(Style::default().bg(ACCENT).fg(Color::White));
    let text = vec![
        Line::from(vec![
            Span::styled(" Active Backup for Business ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("· bare-metal restore"),
            Span::raw(format!("{:>width$}", format!("step {step} of 7 ", ), width = (area.width as usize).saturating_sub(48))),
        ]),
        Line::from(Span::styled(format!(" {title}"), Style::default().add_modifier(Modifier::BOLD))),
    ];
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn footer(f: &mut Frame, area: Rect, keys: &str) {
    f.render_widget(Paragraph::new(Line::from(format!(" {keys}")).dark_gray()), area);
}

fn dialog(f: &mut Frame, title: &str, text: Vec<Line>, error: bool) {
    let area = f.area();
    let w = area.width.min(76);
    let h = (text.len() as u16 + 4).min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let color = if error { Color::Red } else { ACCENT };
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(color)).title(format!(" {title} "));
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(block), r);
}

impl App {
    fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if let Some(r) = &mut self.running {
                while let Ok(m) = r.rx.try_recv() {
                    match m {
                        Msg::Event(Event::Step(s)) => r.steps.push((false, s)),
                        Msg::Event(Event::Warning(s)) => r.steps.push((true, s)),
                        Msg::Event(Event::Progress { done, total }) => (r.done, r.total) = (done, total),
                        Msg::Finished(res) => {
                            r.result = Some(res);
                            r.ended = Some(Instant::now());
                        }
                    }
                }
            }
            if event::poll(Duration::from_millis(200))?
                && let TermEvent::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                self.key(k, terminal)?;
            }
        }
        Ok(())
    }

    /// Draws a "working" note, for the blocking calls to the server.
    fn busy(&mut self, terminal: &mut DefaultTerminal, what: &str) -> std::io::Result<()> {
        self.message = Some((false, what.to_string()));
        terminal.draw(|f| self.draw(f))?;
        self.message = None;
        Ok(())
    }

    fn key(&mut self, k: KeyEvent, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            if let Some(r) = &self.running
                && r.result.is_none()
            {
                r.cancel.store(true, Ordering::Relaxed);
                return Ok(());
            }
            self.quit = true;
            return Ok(());
        }
        if self.message.is_some() {
            self.message = None;
            return Ok(());
        }
        if let Some((s, _)) = self.trust.take() {
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.connect(s, terminal)?;
            }
            return Ok(());
        }
        match self.page {
            Page::Welcome => match k.code {
                KeyCode::Enter => self.page = Page::Login,
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('r') => self.locals = local::disks(),
                _ => {}
            },
            Page::Login => self.key_login(k, terminal)?,
            Page::Tasks => match k.code {
                KeyCode::Up => self.task_state.select_previous(),
                KeyCode::Down => self.task_state.select_next(),
                KeyCode::Esc => self.page = Page::Login,
                KeyCode::Enter if self.task_state.selected().is_some() => self.page = Page::Mode,
                _ => {}
            },
            Page::Mode => match k.code {
                KeyCode::Up => self.mode = self.mode.saturating_sub(1),
                KeyCode::Down => self.mode = (self.mode + 1).min(2),
                KeyCode::Esc => self.page = Page::Tasks,
                KeyCode::Enter => {
                    let t = &self.tasks[self.task_state.selected().unwrap()];
                    let (dev, task) = (t.device_id, t.task_id);
                    self.busy(terminal, "Reading the versions…")?;
                    match self.server.as_mut().unwrap().versions(dev, task) {
                        Ok(resp) => {
                            self.versions = Version::list(&resp);
                            self.version_state.select(if self.versions.is_empty() { None } else { Some(0) });
                            self.chosen.clear();
                            self.volume_cursor = 0;
                            self.page = Page::Versions;
                        }
                        Err(e) => self.message = Some((true, format!("Cannot read the versions: {e}"))),
                    }
                }
                _ => {}
            },
            Page::Versions => self.key_versions(k),
            Page::Mapping => self.key_mapping(k),
            Page::Summary => self.key_summary(k),
            Page::Progress => self.key_progress(k),
        }
        Ok(())
    }

    fn key_login(&mut self, k: KeyEvent, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        let l = &mut self.login;
        match k.code {
            KeyCode::Esc => self.page = Page::Welcome,
            KeyCode::Tab | KeyCode::Down => l.focus = (l.focus + 1) % 4,
            KeyCode::BackTab | KeyCode::Up => l.focus = (l.focus + 3) % 4,
            KeyCode::Backspace => {
                l.fields[l.focus].pop();
            }
            KeyCode::Char(c) => l.fields[l.focus].push(c),
            KeyCode::Enter if l.focus < 2 => l.focus += 1,
            KeyCode::Enter => {
                let [addr, user, pass, otp] = l.fields.clone();
                if addr.is_empty() || user.is_empty() {
                    self.message = Some((true, "Enter the server address and the user name.".into()));
                    return Ok(());
                }
                let (host, port) = match addr.rsplit_once(':') {
                    Some((h, p)) if p.parse::<u16>().is_ok() && !h.contains(':') => (h.to_string(), p.parse().unwrap()),
                    _ => (addr.trim_matches(['[', ']']).to_string(), DEFAULT_PORT),
                };
                self.busy(terminal, &format!("Connecting to {host}…"))?;
                let mut s = Server::new(&host, port, &user, &pass, &otp);
                match s.probe() {
                    Ok(fp) => self.trust = Some((s, fingerprint_hex(&fp))),
                    Err(e) => self.message = Some((true, format!("Cannot connect: {e}"))),
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Logs in (the certificate has been confirmed) and lists the tasks.
    fn connect(&mut self, mut s: Server, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        self.busy(terminal, "Logging in…")?;
        match s.login().and_then(|v| s.tasks().map(|t| (v, t))) {
            Ok((v, tasks)) => {
                self.server_version = format!("{v}, certificate {}", fingerprint_hex(&s.pin.unwrap()));
                self.tasks = tasks.iter().map(Task::from).filter(|t| t.latest != 0).collect();
                self.tasks.sort_by(|a, b| a.host_name.cmp(&b.host_name));
                self.task_state.select(if self.tasks.is_empty() { None } else { Some(0) });
                self.server = Some(s);
                self.page = Page::Tasks;
            }
            Err(e) => {
                let hint = match &e {
                    abb_proto::Error::Server { code: 4101, .. } => " (a two-factor code is needed)",
                    _ => "",
                };
                self.message = Some((true, format!("Cannot log in: {e}{hint}")));
            }
        }
        Ok(())
    }

    fn version(&self) -> Option<&Version> {
        self.version_state.selected().and_then(|i| self.versions.get(i))
    }

    /// Volumes of the selected version, as (disk, partition), in display order.
    fn volume_list(&self) -> Vec<(usize, usize)> {
        let Some(v) = self.version() else { return vec![] };
        v.disks
            .iter()
            .enumerate()
            .flat_map(|(i, d)| d.partitions.iter().enumerate().filter(|(_, p)| !p.excluded).map(move |(j, _)| (i, j)))
            .collect()
    }

    fn key_versions(&mut self, k: KeyEvent) {
        let volumes = MODES[self.mode].0 == Mode::Volumes;
        match k.code {
            KeyCode::Esc => self.page = Page::Mode,
            KeyCode::Up => {
                self.version_state.select_previous();
                self.chosen.clear();
            }
            KeyCode::Down => {
                self.version_state.select_next();
                if self.version_state.selected().is_some_and(|i| i >= self.versions.len()) {
                    self.version_state.select(Some(self.versions.len().saturating_sub(1)));
                }
                self.chosen.clear();
            }
            KeyCode::Left | KeyCode::Char('k') if volumes => self.volume_cursor = self.volume_cursor.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('j') if volumes => {
                self.volume_cursor = (self.volume_cursor + 1).min(self.volume_list().len().saturating_sub(1))
            }
            KeyCode::Char(' ') if volumes => {
                if let Some(&v) = self.volume_list().get(self.volume_cursor) {
                    if let Some(i) = self.chosen.iter().position(|c| *c == v) {
                        self.chosen.remove(i);
                    } else {
                        self.chosen.push(v);
                    }
                }
            }
            KeyCode::Enter => {
                self.locals = local::disks();
                let Some(v) = self.version() else { return };
                let mode = MODES[self.mode].0;
                let mut plan = Plan { mode, disks: vec![None; v.disks.len()], volumes: vec![], grow: self.grow, efi_fallback: self.efi_fallback };
                match mode {
                    Mode::Device => plan.disks = plan::auto_map(v, &self.locals),
                    Mode::System => {
                        // Only disks that hold system volumes.
                        let auto = plan::auto_map(v, &self.locals);
                        for (i, d) in v.disks.iter().enumerate() {
                            if d.partitions.iter().any(|p| p.is_system()) {
                                plan.disks[i] = auto[i];
                            }
                        }
                    }
                    Mode::Volumes => {
                        if self.chosen.is_empty() {
                            self.message = Some((true, "Choose at least one volume with the space bar.".into()));
                            return;
                        }
                        let mut chosen = self.chosen.clone();
                        chosen.sort();
                        for (disk, part) in chosen {
                            let local = (0..self.locals.len())
                                .find(|&j| plan::first_fit(v, &self.locals, j, disk, part, &plan.volumes).is_some() && self.locals[j].busy.is_none())
                                .unwrap_or(0);
                            let place = plan::first_fit(v, &self.locals, local, disk, part, &plan.volumes).map_or(Place::Partition(1), Place::Free);
                            plan.volumes.push(VolumeTarget { disk, part, local, place });
                        }
                    }
                }
                self.plan = Some(plan);
                self.map_cursor = 0;
                self.page = Page::Mapping;
            }
            _ => {}
        }
    }

    /// Rows of the mapping page: one per disk (device/system) or volume, then the two options.
    fn mapping_rows(&self) -> usize {
        let p = self.plan.as_ref().unwrap();
        let n = match p.mode {
            Mode::Volumes => p.volumes.len(),
            _ => self.version().map_or(0, |v| v.disks.len()),
        };
        n + 2
    }

    fn key_mapping(&mut self, k: KeyEvent) {
        let rows = self.mapping_rows();
        let v = &self.versions[self.version_state.selected().unwrap()];
        let locals = &self.locals;
        let plan = self.plan.as_mut().unwrap();
        let item = self.map_cursor;
        let n = rows - 2;
        let step = |forward: bool, len: usize, cur: usize| if forward { (cur + 1) % len } else { (cur + len - 1) % len };
        match k.code {
            KeyCode::Esc => self.page = Page::Versions,
            KeyCode::Up => self.map_cursor = self.map_cursor.saturating_sub(1),
            KeyCode::Down => self.map_cursor = (self.map_cursor + 1).min(rows - 1),
            KeyCode::Char(' ') if item == n => self.grow = !self.grow,
            KeyCode::Char(' ') if item == n + 1 => self.efi_fallback = !self.efi_fallback,
            KeyCode::Left | KeyCode::Right if item < n && plan.mode != Mode::Volumes => {
                // Cycle through "skip" and the local disks.
                let cur = plan.disks[item].map_or(0, |j| j + 1);
                let next = step(k.code == KeyCode::Right, locals.len() + 1, cur);
                plan.disks[item] = if next == 0 { None } else { Some(next - 1) };
            }
            KeyCode::Left | KeyCode::Right if item < n => {
                let forward = k.code == KeyCode::Right;
                let (disk, part) = (plan.volumes[item].disk, plan.volumes[item].part);
                // Targets: each local disk's existing partitions and its first free area.
                let mut targets: Vec<(usize, Place)> = Vec::new();
                for (j, l) in locals.iter().enumerate() {
                    if let Some(s) = plan::first_fit(v, locals, j, disk, part, &plan.volumes[..item]) {
                        targets.push((j, Place::Free(s)));
                    }
                    for q in &l.partitions {
                        targets.push((j, Place::Partition(q.number)));
                    }
                }
                if targets.is_empty() {
                    return;
                }
                let t = &plan.volumes[item];
                let cur = targets.iter().position(|(j, p)| *j == t.local && *p == t.place).unwrap_or(0);
                let (local, place) = targets[step(forward, targets.len(), cur)].clone();
                plan.volumes[item].local = local;
                plan.volumes[item].place = place;
                // Later volumes on free space may have to move.
                for k2 in item + 1..plan.volumes.len() {
                    let o = plan.volumes[k2].clone();
                    if let Place::Free(_) = o.place
                        && let Some(s) = plan::first_fit(v, locals, o.local, o.disk, o.part, &plan.volumes[..k2])
                    {
                        plan.volumes[k2].place = Place::Free(s);
                    }
                }
            }
            KeyCode::Enter => {
                plan.grow = self.grow;
                plan.efi_fallback = self.efi_fallback;
                let errs = plan.check(v, locals);
                if errs.is_empty() {
                    self.confirm = false;
                    self.page = Page::Summary;
                } else {
                    self.message = Some((true, errs.join("\n")));
                }
            }
            _ => {}
        }
    }

    fn key_summary(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc if self.confirm => self.confirm = false,
            KeyCode::Esc => self.page = Page::Mapping,
            KeyCode::Enter if !self.confirm => self.confirm = true,
            KeyCode::Char('y') | KeyCode::Char('Y') if self.confirm => self.start(),
            KeyCode::Char('n') | KeyCode::Char('N') => self.confirm = false,
            _ => {}
        }
    }

    fn start(&mut self) {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut server = self.server.take().unwrap();
        let t = &self.tasks[self.task_state.selected().unwrap()];
        let task = Task { device_id: t.device_id, task_id: t.task_id, task_name: t.task_name.clone(), host_name: t.host_name.clone(), os_name: t.os_name.clone(), latest: t.latest };
        let version_index = self.version_state.selected().unwrap();
        let versions = std::mem::take(&mut self.versions);
        let plan = self.plan.clone().unwrap();
        let locals = self.locals.clone();
        let c = cancel.clone();
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let res = restore::run(&mut server, &task, &versions[version_index], &plan, &locals, &c, &mut |e| {
                let _ = tx2.send(Msg::Event(e));
            });
            let _ = tx.send(Msg::Finished(res));
        });
        self.running = Some(Running { rx, cancel, started: Instant::now(), ended: None, done: 0, total: 1, steps: vec![], result: None });
        self.page = Page::Progress;
    }

    fn key_progress(&mut self, k: KeyEvent) {
        let Some(r) = &self.running else { return };
        if r.result.is_none() {
            if k.code == KeyCode::Esc {
                r.cancel.store(true, Ordering::Relaxed);
            }
            return;
        }
        match k.code {
            KeyCode::Char('r') => power(libc::RB_AUTOBOOT),
            KeyCode::Char('p') => power(libc::RB_POWER_OFF),
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => self.quit = true,
            _ => {}
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [head, body, foot] = Layout::vertical([Constraint::Length(2), Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
        let body = body.inner(ratatui::layout::Margin { horizontal: 2, vertical: 1 });
        match self.page {
            Page::Welcome => self.draw_welcome(f, head, body, foot),
            Page::Login => self.draw_login(f, head, body, foot),
            Page::Tasks => self.draw_tasks(f, head, body, foot),
            Page::Mode => self.draw_mode(f, head, body, foot),
            Page::Versions => self.draw_versions(f, head, body, foot),
            Page::Mapping => self.draw_mapping(f, head, body, foot),
            Page::Summary => self.draw_summary(f, head, body, foot),
            Page::Progress => self.draw_progress(f, head, body, foot),
        }
        if let Some((s, fp)) = &self.trust {
            let lines = vec![
                Line::from(format!("Certificate of {}:", s.host)),
                Line::from(""),
                Line::from(fp.clone()).bold(),
                Line::from(""),
                Line::from("The password is sent only to a server you trust."),
                Line::from("Is this your NAS? (y/n)").bold(),
            ];
            dialog(f, "Server certificate", lines, false);
        }
        if let Some((error, m)) = &self.message {
            let lines = m.lines().map(|l| Line::from(l.to_string())).collect();
            dialog(f, if *error { "Error" } else { "Please wait" }, lines, *error);
        }
    }

    fn draw_welcome(&self, f: &mut Frame, head: Rect, body: Rect, foot: Rect) {
        header(f, head, "Before the recovery begins", 1);
        let (ips, route) = network();
        let mut lines = vec![
            Line::from("This tool restores a computer backed up by Active Backup for Business, from any Linux live system."),
            Line::from(""),
            Line::from("Network".bold()),
        ];
        if ips.is_empty() {
            lines.push(Line::from("  no IPv4 address: connect the network first (e.g. dhcpcd, nmtui, iwctl)").red());
        }
        lines.extend(ips.iter().map(|i| Line::from(format!("  {i}"))));
        if !ips.is_empty() && !route {
            lines.push(Line::from("  no default route: the NAS must be on the same network").yellow());
        }
        lines.push(Line::from(""));
        lines.push(Line::from("Disks of this computer".bold()));
        for d in &self.locals {
            let mut l = Line::from(format!("  {:<10} {:>10}  {}", d.name, size_str(d.size), d.model));
            if let Some(b) = &d.busy {
                l.push_span(Span::from(format!("  in use: {b}")).yellow());
            }
            lines.push(l);
        }
        if self.locals.is_empty() {
            lines.push(Line::from("  no disk found: the disk driver may be missing").red());
        }
        lines.push(Line::from(""));
        lines.push(Line::from("Disks in use (e.g. the live USB stick) cannot be restored to.").dark_gray());
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        footer(f, foot, "Enter next · r rescan disks · q quit");
    }

    fn draw_login(&self, f: &mut Frame, head: Rect, body: Rect, foot: Rect) {
        header(f, head, "Log in to continue", 2);
        let labels = ["NAS address", "User name", "Password", "2FA code (if enabled)"];
        let rows = Layout::vertical([Constraint::Length(3); 4]).split(Rect { width: body.width.min(60), ..body });
        for (i, label) in labels.iter().enumerate() {
            let value = if i == 2 { "•".repeat(self.login.fields[i].chars().count()) } else { self.login.fields[i].clone() };
            let focused = self.login.focus == i;
            let style = if focused { Style::default().fg(ACCENT) } else { Style::default() };
            let cursor = if focused { "▏" } else { "" };
            let p = Paragraph::new(format!("{value}{cursor}"))
                .block(Block::default().borders(Borders::ALL).border_style(style).title(format!(" {label} ")));
            f.render_widget(p, rows[i]);
        }
        footer(f, foot, "Tab/↑↓ move · Enter connect · Esc back · address as host or host:port");
    }

    fn draw_tasks(&mut self, f: &mut Frame, head: Rect, body: Rect, foot: Rect) {
        header(f, head, "Select the device and task", 3);
        let [info, table] = Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(body);
        f.render_widget(Paragraph::new(format!("Active Backup for Business {}", self.server_version)).dark_gray(), info);
        let rows = self.tasks.iter().map(|t| {
            Row::new(vec![Cell::from(t.host_name.clone()), Cell::from(t.task_name.clone()), Cell::from(t.os_name.clone()), Cell::from(date(t.latest))])
        });
        let t = Table::new(rows, [Constraint::Percentage(25), Constraint::Percentage(25), Constraint::Percentage(30), Constraint::Percentage(20)])
            .header(Row::new(["Device", "Task", "System", "Latest version"]).style(Style::default().fg(ACCENT).bold()))
            .row_highlight_style(Style::default().bg(ACCENT).fg(Color::White))
            .highlight_symbol("› ");
        f.render_stateful_widget(t, table, &mut self.task_state);
        if self.tasks.is_empty() {
            f.render_widget(Paragraph::new("\n\nNo device with a backup version for this account."), table);
        }
        footer(f, foot, "↑↓ choose · Enter next · Esc back");
    }

    fn draw_mode(&self, f: &mut Frame, head: Rect, body: Rect, foot: Rect) {
        header(f, head, "Select the restore mode", 4);
        let mut lines = vec![];
        for (i, (m, desc)) in MODES.iter().enumerate() {
            let mark = if i == self.mode { "(•) " } else { "( ) " };
            let style = if i == self.mode { Style::default().fg(ACCENT).bold() } else { Style::default().bold() };
            lines.push(Line::from(Span::styled(format!("{mark}{}", m.title()), style)));
            lines.push(Line::from(format!("    {desc}")).dark_gray());
            lines.push(Line::from(""));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        footer(f, foot, "↑↓ choose · Enter next · Esc back");
    }

    fn draw_versions(&mut self, f: &mut Frame, head: Rect, body: Rect, foot: Rect) {
        header(f, head, "Select a version", 5);
        let [left, right] = Layout::horizontal([Constraint::Length(24), Constraint::Min(30)]).areas(body);
        let items: Vec<ListItem> = self.versions.iter().map(|v| ListItem::new(date(v.time))).collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Versions "))
            .highlight_style(Style::default().bg(ACCENT).fg(Color::White))
            .highlight_symbol("› ");
        f.render_stateful_widget(list, left, &mut self.version_state);
        let volumes = MODES[self.mode].0 == Mode::Volumes;
        let mut lines = vec![];
        if let Some(v) = self.version() {
            let list = self.volume_list();
            for (i, d) in v.disks.iter().enumerate() {
                lines.push(Line::from(format!("Disk {}  ·  {}  ·  {}", d.name(), size_str(d.size), if d.gpt { "GPT" } else { "MBR" })).fg(ACCENT).bold());
                for (j, p) in d.partitions.iter().enumerate() {
                    let pos = list.iter().position(|x| *x == (i, j));
                    let check = match (volumes, pos) {
                        (true, Some(_)) if self.chosen.contains(&(i, j)) => "[x] ",
                        (true, Some(_)) => "[ ] ",
                        (true, None) => "    ",
                        _ => "  ",
                    };
                    let mut tags = vec![];
                    if p.is_system() {
                        tags.push("system".to_string());
                    }
                    if !p.fs.is_empty() {
                        tags.push(p.fs.clone());
                    }
                    if p.excluded {
                        tags.push("not backed up".into());
                    }
                    let mut line = Line::from(format!("{check}{:<24} {:>10}  {}", p.label, size_str(p.length), tags.join(" · ")));
                    if volumes && pos == Some(self.volume_cursor) {
                        line = line.style(Style::default().bg(ACCENT).fg(Color::White));
                    } else if MODES[self.mode].0 == Mode::System && !p.is_system() {
                        line = line.dark_gray();
                    }
                    lines.push(line);
                }
                lines.push(Line::from(""));
            }
            if !v.bootable {
                lines.push(Line::from("This version does not contain a bootable system.").yellow());
            }
        }
        f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Contents ")), right);
        let keys = if volumes { "↑↓ version · ←→ volume · Space choose · Enter next · Esc back" } else { "↑↓ choose · Enter next · Esc back" };
        footer(f, foot, keys);
    }

    fn draw_mapping(&self, f: &mut Frame, head: Rect, body: Rect, foot: Rect) {
        header(f, head, "Disk mapping of this computer", 6);
        let v = self.version().unwrap();
        let p = self.plan.as_ref().unwrap();
        let mut lines = vec![
            Line::from(match p.mode {
                Mode::Volumes => "Where each volume goes: an existing partition (overwritten) or a new partition in free space.",
                _ => "Which disk of this computer receives each disk of the backup. Everything on it is overwritten.",
            })
            .dark_gray(),
            Line::from(""),
        ];
        let sel = |i: usize| if self.map_cursor == i { Style::default().bg(ACCENT).fg(Color::White) } else { Style::default() };
        match p.mode {
            Mode::Volumes => {
                for (i, t) in p.volumes.iter().enumerate() {
                    let src = &v.disks[t.disk].partitions[t.part];
                    let l = &self.locals[t.local];
                    let target = match &t.place {
                        Place::Partition(n) => {
                            let q = l.partitions.iter().find(|q| q.number == *n);
                            format!("{} ({}, overwritten)", l.partition_path(*n).display(), q.map_or("?".into(), |q| size_str(q.length)))
                        }
                        Place::Free(s) => format!("/dev/{}: new partition at {}", l.name, size_str(s * l.sector_size)),
                    };
                    let free = plan::free_areas(v, l, t.local, &p.volumes[..i]);
                    let status = match plan::place_room(src, l, &t.place, &free) {
                        Ok(_) => Span::from("  ✓").green(),
                        Err(e) => Span::from(format!("  ✗ {e}")).red(),
                    };
                    lines.push(Line::from(vec![Span::styled(format!("{:<20} {:>10}  →  {target}", src.label, size_str(src.length)), sel(i)), status]));
                }
            }
            _ => {
                for (i, d) in v.disks.iter().enumerate() {
                    let target = match p.disks[i] {
                        Some(j) => {
                            let l = &self.locals[j];
                            format!("/dev/{} ({}, {})", l.name, size_str(l.size), l.model)
                        }
                        None => "skip".into(),
                    };
                    let status = match p.disks[i] {
                        Some(j) => match plan::disk_fits(d, &self.locals[j]) {
                            Ok(()) if self.locals[j].size > d.size => Span::from("  ✓ bigger disk").green(),
                            Ok(()) if self.locals[j].size < d.size => Span::from("  ✓ smaller disk: the partitions fit").green(),
                            Ok(()) => Span::from("  ✓").green(),
                            Err(e) => Span::from(format!("  ✗ {e}")).red(),
                        },
                        None => Span::from(""),
                    };
                    lines.push(Line::from(vec![Span::styled(format!("Disk {:<8} {:>10}  →  {target}", d.name(), size_str(d.size)), sel(i)), status]));
                }
            }
        }
        let n = self.mapping_rows() - 2;
        lines.push(Line::from(""));
        let check = |b: bool| if b { "[x]" } else { "[ ]" };
        let grow = match p.mode {
            Mode::Volumes => "Grow restored ext/btrfs volumes to fill a bigger partition",
            _ => "On a bigger disk, grow the last partition and its ext/btrfs filesystem",
        };
        lines.push(Line::from(Span::styled(format!("{} {grow}", check(self.grow)), sel(n))));
        lines.push(Line::from(Span::styled(
            format!("{} Make the EFI system partition bootable without an NVRAM entry (\\EFI\\BOOT)", check(self.efi_fallback)),
            sel(n + 1),
        )));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        footer(f, foot, "↑↓ choose · ←→ change target · Space toggle · Enter next · Esc back");
    }

    fn draw_summary(&self, f: &mut Frame, head: Rect, body: Rect, foot: Rect) {
        header(f, head, "Restore summary", 7);
        let t = &self.tasks[self.task_state.selected().unwrap()];
        let v = self.version().unwrap();
        let p = self.plan.as_ref().unwrap();
        let kv = |k: &str, val: String| Line::from(vec![Span::from(format!("{k:<18}")).dark_gray(), Span::from(val).bold()]);
        let mut lines = vec![
            kv("Device", t.host_name.clone()),
            kv("Task", t.task_name.clone()),
            kv("Version", date(v.time)),
            kv("Restore mode", p.mode.title().into()),
            Line::from(""),
            Line::from("Overwritten on this computer:").bold(),
        ];
        match p.mode {
            Mode::Volumes => {
                for t in &p.volumes {
                    let l = &self.locals[t.local];
                    let target = match t.place {
                        Place::Partition(n) => l.partition_path(n).display().to_string(),
                        Place::Free(_) => format!("new partition on /dev/{}", l.name),
                    };
                    lines.push(Line::from(format!("  {}  →  {target}", v.disks[t.disk].partitions[t.part].label)));
                }
            }
            _ => {
                for (i, m) in p.disks.iter().enumerate() {
                    if let Some(j) = m {
                        let what = if p.mode == Mode::System { "boot volumes of disk" } else { "disk" };
                        lines.push(Line::from(format!("  {what} {}  →  /dev/{} ({})", v.disks[i].name(), self.locals[*j].name, self.locals[*j].model)));
                    }
                }
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(format!("Grow filesystems: {}   EFI fallback: {}", if p.grow { "yes" } else { "no" }, if p.efi_fallback { "yes" } else { "no" })).dark_gray());
        f.render_widget(Paragraph::new(lines), body);
        footer(f, foot, "Enter restore · Esc back");
        if self.confirm {
            dialog(
                f,
                "Confirm",
                vec![
                    Line::from("Once the restore begins it cannot be undone."),
                    Line::from("The disks and partitions listed will be overwritten."),
                    Line::from(""),
                    Line::from("Start the restore? (y/n)").bold(),
                ],
                true,
            );
        }
    }

    fn draw_progress(&self, f: &mut Frame, head: Rect, body: Rect, foot: Rect) {
        header(f, head, "Restoring", 7);
        let r = self.running.as_ref().unwrap();
        let [gauge, stats, log] = Layout::vertical([Constraint::Length(3), Constraint::Length(2), Constraint::Min(3)]).areas(body);
        let ratio = (r.done as f64 / r.total.max(1) as f64).min(1.0);
        let (label, color) = match &r.result {
            Some(Ok(())) => ("Restore complete".to_string(), Color::Green),
            Some(Err(_)) => ("Restore failed".to_string(), Color::Red),
            None => (format!("{:.0}%", ratio * 100.0), ACCENT),
        };
        f.render_widget(Gauge::default().block(Block::default().borders(Borders::ALL)).gauge_style(Style::default().fg(color)).ratio(ratio).label(label), gauge);
        let secs = r.ended.unwrap_or_else(Instant::now).duration_since(r.started).as_secs();
        let speed = r.done / secs.max(1);
        f.render_widget(
            Paragraph::new(format!(
                "{} of {} done  ·  {}/s  ·  elapsed {:02}:{:02}:{:02}  ·  unused areas are skipped",
                size_str(r.done),
                size_str(r.total),
                size_str(speed),
                secs / 3600,
                secs / 60 % 60,
                secs % 60
            ))
            .dark_gray(),
            stats,
        );
        let mut lines: Vec<Line> = r.steps.iter().map(|(warn, s)| if *warn { Line::from(format!("! {s}")).yellow() } else { Line::from(format!("· {s}")) }).collect();
        match &r.result {
            Some(Ok(())) => {
                lines.push(Line::from(""));
                lines.push(Line::from("The device has been restored. Remove the live medium, then restart.").green().bold());
            }
            Some(Err(e)) => {
                lines.push(Line::from(""));
                lines.push(Line::from(format!("✗ {e}")).red().bold());
            }
            None => {}
        }
        let skip = lines.len().saturating_sub(log.height as usize);
        f.render_widget(Paragraph::new(lines.split_off(skip)).wrap(Wrap { trim: false }), log);
        let keys = if r.result.is_some() { "r restart · p power off · q quit" } else { "Esc cancel" };
        footer(f, foot, keys);
    }
}

fn power(how: libc::c_int) {
    unsafe {
        libc::sync();
        libc::reboot(how);
    }
}
