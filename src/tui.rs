//! The interactive interface: [`HEIGHT`] lines drawn below the prompt, not a full screen, with
//! the keys on the last. Each finished job prints what it wrote, or why it failed, above them,
//! into the terminal's scrollback, where it stays after reims-util exits.
//!
//! A job runs on a thread of its own and reports through a channel (progress.rs); stopping one
//! (Esc) lets a download end between reads, so starting it again resumes it, and Esc again
//! leaves without waiting for that.

use crate::cli::{self, OpencoreArgs};
use crate::installer::Installer;
use crate::opencore::{config, smbios};
use crate::progress::{self, Event, Rate, Reporter};
use crate::{installer, opencore, recovery};
use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{LineGauge, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame, TerminalOptions, Viewport};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

const HEIGHT: u16 = 13;

pub fn run() -> Result<()> {
    let mut terminal = ratatui::try_init_with_options(TerminalOptions {
        viewport: Viewport::Inline(HEIGHT),
    })?;
    let result = App::new().run(&mut terminal);
    // Leave the scrollback as the jobs wrote it: the interface goes.
    let _ = terminal.clear();
    let top = terminal.get_frame().area().as_position();
    let _ = terminal.set_cursor_position(top);
    ratatui::restore();
    result
}

const MENU: &[(&str, &str)] = &[
    (
        "Make an OpenCore boot image",
        "OpenCore 1.0.8 for a q35 VM on KVM, with a Mac identity of its own, and OVMF's \
         OVMF_CODE.fd and OVMF_VARS.fd beside it, where reims-ctl finds them.",
    ),
    (
        "Make an online installer",
        "Apple's recovery image for macOS 10.13 to 26, about 1 GB. A VM boots it, and Recovery's \
         Reinstall macOS downloads and installs the newest release of that macOS.",
    ),
    (
        "Make an offline installer",
        "A disk image with the whole installer for one exact release of macOS 11 to 26, 13 to 20 \
         GB. A VM boots it, and installs that release from it with no network.",
    ),
];

enum Screen {
    Menu(usize),
    Form(Job, Form),
    Running(Running),
}

#[derive(Clone)]
enum Job {
    OpenCore,
    Online,
    Offline(Vec<Installer>),
}

enum Msg {
    Event(Event),
    /// A finished job: the lines to keep in the scrollback, or the error.
    Done(std::result::Result<Vec<Line<'static>>, String>),
    Catalog(std::result::Result<Vec<Installer>, String>),
}

struct Running {
    title: String,
    step: String,
    progress: Option<(u64, Option<u64>)>,
    rate: Rate,
    notes: VecDeque<String>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Msg>,
}

struct ChannelReporter {
    tx: Sender<Msg>,
    cancel: Arc<AtomicBool>,
}

impl Reporter for ChannelReporter {
    fn event(&self, e: Event) {
        let _ = self.tx.send(Msg::Event(e));
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

struct App {
    screen: Screen,
    /// The line under the box: the last job's outcome.
    status: Option<(String, bool)>,
    quit: bool,
}

impl App {
    fn new() -> App {
        App {
            screen: Screen::Menu(0),
            status: None,
            quit: false,
        }
    }

    fn run(&mut self, t: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            t.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))?
                && let TermEvent::Key(k) = event::read()?
                && k.kind != KeyEventKind::Release
            {
                self.key(k);
            }
            self.drain(t)?;
        }
        Ok(())
    }

    fn drain(&mut self, t: &mut DefaultTerminal) -> Result<()> {
        let Screen::Running(r) = &mut self.screen else {
            return Ok(());
        };
        let mut finished = None;
        while let Ok(m) = r.rx.try_recv() {
            match m {
                Msg::Event(Event::Step { message }) => {
                    r.step = message;
                    r.progress = None;
                    r.rate = Rate::default();
                }
                Msg::Event(Event::Progress { done, total }) => {
                    r.rate.add(done);
                    r.progress = Some((done, total));
                }
                Msg::Event(Event::Note { message }) => {
                    r.notes.push_back(message);
                    while r.notes.len() > 3 {
                        r.notes.pop_front();
                    }
                }
                Msg::Done(d) => finished = Some(Ok(d)),
                Msg::Catalog(c) => finished = Some(Err(c)),
            }
        }
        match finished {
            None => {}
            Some(Ok(Ok(lines))) => {
                print_above(t, lines)?;
                self.status = Some(("Done: what it wrote is above.".into(), false));
                self.screen = Screen::Menu(0);
            }
            Some(Ok(Err(e))) | Some(Err(Err(e))) => {
                if r.cancel.load(Ordering::Relaxed) {
                    self.status = Some(("Stopped. Start it again to resume.".into(), false));
                } else {
                    // Above, where it's wrapped whole: an error often starts with a long URL.
                    let red = Style::new().fg(Color::Red);
                    print_above(
                        t,
                        vec![
                            Line::styled(format!("Failed: {}", r.title), red.bold()),
                            Line::styled(format!("  {e}"), red),
                        ],
                    )?;
                    self.status = Some(("Failed: why is above.".into(), true));
                }
                self.screen = Screen::Menu(0);
            }
            Some(Err(Ok(list))) => {
                let form = offline_form(&list);
                self.screen = Screen::Form(Job::Offline(list), form);
            }
        }
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) {
        let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
        match &mut self.screen {
            Screen::Menu(i) => match k.code {
                _ if ctrl_c => self.quit = true,
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Up | KeyCode::Char('k') => *i = i.checked_sub(1).unwrap_or(MENU.len() - 1),
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => *i = (*i + 1) % MENU.len(),
                KeyCode::Char(c @ '1'..='3') => self.open(c as usize - '1' as usize),
                KeyCode::Enter => {
                    let i = *i;
                    self.open(i);
                }
                _ => {}
            },
            Screen::Form(job, form) => {
                if ctrl_c || k.code == KeyCode::Esc {
                    self.screen = Screen::Menu(match job {
                        Job::OpenCore => 0,
                        Job::Online => 1,
                        Job::Offline(_) => 2,
                    });
                } else if k.code == KeyCode::Enter {
                    let (job, form) = (job.clone(), form.clone());
                    self.start(job, &form);
                } else {
                    form.key(k);
                }
            }
            Screen::Running(r) => {
                if ctrl_c || matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
                    if r.cancel.swap(true, Ordering::Relaxed) {
                        // A second press leaves at once, for a download stalled in a read that
                        // only its time budget ends; it resumes next time.
                        self.quit = true;
                    }
                    r.step = "Stopping… (press Esc again to leave now)".into();
                }
            }
        }
    }

    fn open(&mut self, i: usize) {
        self.status = None;
        match i {
            0 => self.screen = Screen::Form(Job::OpenCore, opencore_form()),
            1 => self.screen = Screen::Form(Job::Online, online_form()),
            _ => self.spawn("Offline installers".into(), |r, tx| {
                let _ = tx.send(Msg::Catalog(
                    installer::list(r).map_err(|e| format!("{e:#}")),
                ));
            }),
        }
    }

    fn start(&mut self, job: Job, form: &Form) {
        match job {
            Job::OpenCore => {
                let a = OpencoreArgs {
                    output: expand(&form.text(0)),
                    model: form.choice(1).to_string(),
                    serial: None,
                    mlb: None,
                    uuid: None,
                    rom: None,
                    timeout: match form.text(2).trim().parse() {
                        Ok(t) => t,
                        Err(_) => {
                            self.status =
                                Some(("The picker timeout is a number of seconds.".into(), true));
                            return;
                        }
                    },
                    boot_args: config::DEFAULT_BOOT_ARGS.into(),
                    verbose_boot: form.toggle(6),
                    disable_sip: !form.toggle(4),
                    no_cpuid_spoof: false,
                    show_vm: !form.toggle(5),
                    text_picker: !form.toggle(7),
                    show_picker: form.toggle(8),
                    add_kext: Vec::new(),
                    _retired: 0,
                    no_ovmf: !form.toggle(3),
                    ovmf_dir: None,
                };
                let o = match cli::opencore_options(&a) {
                    Ok(o) => o,
                    Err(e) => {
                        self.status = Some((format!("{e:#}"), true));
                        return;
                    }
                };
                self.spawn("OpenCore boot image".into(), move |r, tx| {
                    let res = opencore::build(&o, r).map(|out| opencore_lines(&out));
                    let _ = tx.send(Msg::Done(res.map_err(|e| format!("{e:#}"))));
                });
            }
            Job::Online => {
                let Some(v) = recovery::VERSIONS.get(form.choice_index(0)).copied() else {
                    return;
                };
                let dir = expand(&form.text(1));
                self.spawn(format!("{} online installer", v.title()), move |r, tx| {
                    let res = recovery::online_installer(&v, &dir, r).map(|out| {
                        vec![
                            ok_line(&format!(
                                "{} online installer (Apple's product {})",
                                out.title, out.product
                            )),
                            detail("Installer", &abs(&out.image)),
                            next_line(&format!(
                                "reims-ctl create mac --opencore OpenCore.qcow2 --installer {}",
                                shell(&abs(&out.image))
                            )),
                            detail(
                                "Then",
                                "in Recovery, erase the VM's disk in Disk Utility (APFS) and \
                                 pick Reinstall macOS",
                            ),
                        ]
                    });
                    let _ = tx.send(Msg::Done(res.map_err(|e| format!("{e:#}"))));
                });
            }
            Job::Offline(list) => {
                let Some(inst) = list.get(form.choice_index(0)).cloned() else {
                    return;
                };
                let dir = expand(&form.text(1));
                self.spawn(
                    format!("{} offline installer", inst.label()),
                    move |r, tx| {
                        let res = installer::offline_installer(&inst, &dir, r)
                            .map(|out| offline_lines(&out));
                        let _ = tx.send(Msg::Done(res.map_err(|e| format!("{e:#}"))));
                    },
                );
            }
        }
    }

    fn spawn(
        &mut self,
        title: String,
        work: impl FnOnce(&ChannelReporter, &Sender<Msg>) + Send + 'static,
    ) {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let reporter = ChannelReporter {
            tx: tx.clone(),
            cancel: cancel.clone(),
        };
        std::thread::spawn(move || work(&reporter, &tx));
        self.screen = Screen::Running(Running {
            title,
            step: "Starting".into(),
            progress: None,
            rate: Rate::default(),
            notes: VecDeque::new(),
            cancel,
            rx,
        });
    }

    fn draw(&self, f: &mut Frame) {
        let [main, status, keys_row] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());
        // The menu needs no title: the prompt above it says reims-util.
        let (title, keys) = match &self.screen {
            Screen::Menu(_) => (None, " ↑↓ choose · Enter open · q quit"),
            Screen::Form(_, form) => (
                Some(form.title.as_str()),
                " ↑↓ field · ←→ or Space change · Enter start · Esc back",
            ),
            Screen::Running(r) => (
                Some(r.title.as_str()),
                " Esc stop (a download resumes when started again)",
            ),
        };
        let main = match title {
            Some(t) => {
                let [head, rest] =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(main);
                f.render_widget(
                    Paragraph::new(Line::styled(
                        format!(" {t}"),
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
                    head,
                );
                rest
            }
            None => main,
        };
        match &self.screen {
            Screen::Menu(i) => draw_menu(f, main, *i),
            Screen::Form(_, form) => form.draw(f, main),
            Screen::Running(r) => draw_running(f, main, r),
        }
        f.render_widget(Paragraph::new(Line::from(keys).dim()), keys_row);
        if let Some((s, err)) = &self.status {
            let style = if *err {
                Style::new().fg(Color::Red)
            } else {
                Style::new().fg(Color::Green)
            };
            f.render_widget(Paragraph::new(Line::styled(format!(" {s}"), style)), status);
        }
    }
}

fn draw_menu(f: &mut Frame, area: Rect, index: usize) {
    let mut lines: Vec<Line> = MENU
        .iter()
        .enumerate()
        .map(|(i, (label, _))| {
            if i == index {
                Line::from(vec![
                    Span::styled(" ▸ ", Style::new().fg(Color::Cyan)),
                    Span::styled(*label, Style::new().bold()),
                ])
            } else {
                Line::from(format!("   {label}"))
            }
        })
        .collect();
    lines.push(Line::default());
    let [list, help] =
        Layout::vertical([Constraint::Length(lines.len() as u16), Constraint::Min(1)]).areas(area);
    f.render_widget(Paragraph::new(lines), list);
    f.render_widget(
        Paragraph::new(MENU[index].1)
            .wrap(Wrap { trim: true })
            .dim(),
        help.inner(ratatui::layout::Margin::new(1, 0)),
    );
}

fn draw_running(f: &mut Frame, area: Rect, r: &Running) {
    let area = area.inner(ratatui::layout::Margin::new(1, 0));
    let [step, gauge, _, notes] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(Line::from(r.step.as_str()).bold()), step);
    if let Some((done, total)) = r.progress {
        let label = progress::progress_line(done, total, r.rate.per_second());
        let ratio = match total {
            Some(t) if t > 0 => (done as f64 / t as f64).clamp(0.0, 1.0),
            _ => 0.0,
        };
        f.render_widget(
            LineGauge::default()
                .filled_style(Style::new().fg(Color::Cyan))
                .unfilled_style(Style::new().fg(Color::DarkGray))
                .label(label)
                .ratio(ratio),
            gauge,
        );
    }
    let lines: Vec<Line> = r
        .notes
        .iter()
        .map(|n| Line::from(n.as_str()).dim())
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), notes);
}

#[derive(Clone)]
struct Form {
    title: String,
    fields: Vec<Field>,
    focus: usize,
}

#[derive(Clone)]
struct Field {
    label: &'static str,
    help: String,
    kind: Kind,
}

#[derive(Clone)]
enum Kind {
    /// The text, and what shows dim while it is empty.
    Text(String, &'static str),
    Choice(Vec<String>, usize),
    Toggle(bool),
}

impl Form {
    fn text(&self, i: usize) -> String {
        match &self.fields[i].kind {
            Kind::Text(s, _) => s.clone(),
            _ => String::new(),
        }
    }
    fn choice_index(&self, i: usize) -> usize {
        match &self.fields[i].kind {
            Kind::Choice(_, n) => *n,
            _ => 0,
        }
    }
    fn choice(&self, i: usize) -> &str {
        match &self.fields[i].kind {
            Kind::Choice(o, n) => o[*n].split(' ').next().unwrap_or(""),
            _ => "",
        }
    }
    fn toggle(&self, i: usize) -> bool {
        matches!(self.fields[i].kind, Kind::Toggle(true))
    }

    fn key(&mut self, k: KeyEvent) {
        let n = self.fields.len();
        match k.code {
            KeyCode::Up | KeyCode::BackTab => self.focus = (self.focus + n - 1) % n,
            KeyCode::Down | KeyCode::Tab => self.focus = (self.focus + 1) % n,
            _ => match &mut self.fields[self.focus].kind {
                Kind::Text(s, _) => match k.code {
                    KeyCode::Backspace => {
                        s.pop();
                    }
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => s.clear(),
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => s.push(c),
                    _ => {}
                },
                Kind::Choice(o, i) => match k.code {
                    KeyCode::Left | KeyCode::Char('h') => *i = (*i + o.len() - 1) % o.len(),
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                        *i = (*i + 1) % o.len()
                    }
                    KeyCode::Home => *i = 0,
                    KeyCode::End => *i = o.len() - 1,
                    _ => {}
                },
                Kind::Toggle(b) => {
                    if matches!(k.code, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) {
                        *b = !*b;
                    }
                }
            },
        }
    }

    fn draw(&self, f: &mut Frame, area: Rect) {
        let area = area.inner(ratatui::layout::Margin::new(1, 0));
        let width = self.fields.iter().map(|x| x.label.len()).max().unwrap_or(0) + 2;
        let mut lines: Vec<Line> = Vec::new();
        for (i, field) in self.fields.iter().enumerate() {
            let focused = i == self.focus;
            let value = match &field.kind {
                Kind::Text(s, _) if focused => format!("{s}▏"),
                Kind::Text(s, _) => s.clone(),
                Kind::Choice(o, n) if focused => format!("◂ {} ▸", o[*n]),
                Kind::Choice(o, n) => o[*n].clone(),
                Kind::Toggle(b) => if *b { "[x] yes" } else { "[ ] no" }.to_string(),
            };
            let label = format!("{:<width$}", field.label);
            let mut line = if focused {
                Line::from(vec![
                    Span::styled(label, Style::new().fg(Color::Cyan).bold()),
                    Span::styled(value, Style::new().bold()),
                ])
            } else {
                Line::from(vec![Span::raw(label).dim(), Span::raw(value)])
            };
            if let Kind::Text(s, placeholder) = &field.kind
                && s.is_empty()
            {
                line.push_span(Span::raw(*placeholder).dim());
            }
            lines.push(line);
        }
        let [list, _, help] = Layout::vertical([
            Constraint::Length(lines.len() as u16),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(area);
        f.render_widget(Paragraph::new(lines), list);
        f.render_widget(
            Paragraph::new(self.fields[self.focus].help.as_str())
                .wrap(Wrap { trim: true })
                .dim(),
            help,
        );
    }
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| ".".into())
}

fn opencore_form() -> Form {
    Form {
        title: "Make an OpenCore boot image".into(),
        focus: 0,
        fields: vec![
            Field {
                label: "Save as",
                help: format!(
                    "The image: qcow2 for a .qcow2 name, else a raw disk image. A relative path is \
                     from {}.",
                    cwd()
                ),
                kind: Kind::Text("OpenCore.qcow2".into(), ""),
            },
            Field {
                label: "Mac model",
                help: format!(
                    "The Mac the VM claims to be, and the macOS releases it runs: {}.",
                    smbios::MODELS
                        .iter()
                        .map(|m| format!("{}, {}", m.name, m.runs))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                kind: Kind::Choice(
                    smbios::MODELS
                        .iter()
                        .map(|m| format!("{} ({})", m.name, m.runs))
                        .collect(),
                    0,
                ),
            },
            Field {
                label: "Picker timeout",
                help: "Seconds OpenCore's boot picker, when shown, waits before it boots the \
                       default entry; 0 waits for you."
                    .into(),
                kind: Kind::Text("5".into(), ""),
            },
            Field {
                label: "Write OVMF",
                help: format!(
                    "Write OVMF_CODE.fd and OVMF_VARS.fd (Arch Linux's {}) beside the image, \
                     where reims-ctl finds them.",
                    crate::ovmf::VERSION
                ),
                kind: Kind::Toggle(true),
            },
            Field {
                label: "SIP on",
                help: "System Integrity Protection. Leave it on unless you need it off.".into(),
                kind: Kind::Toggle(true),
            },
            Field {
                label: "Hide the VM",
                help: "Keep macOS from seeing it runs in a VM, so iCloud and the App Store let \
                       you sign in."
                    .into(),
                kind: Kind::Toggle(true),
            },
            Field {
                label: "Verbose boot",
                help: "Show the kernel's messages while macOS boots, instead of the Apple logo."
                    .into(),
                kind: Kind::Toggle(false),
            },
            Field {
                label: "Graphical picker",
                help: "OpenCore's boot picker with icons, or its text list when off.".into(),
                kind: Kind::Toggle(true),
            },
            Field {
                label: "Show the picker",
                help: "OpenCore's boot picker at every boot, or none when off: OpenCore boots the \
                       default entry."
                    .into(),
                kind: Kind::Toggle(false),
            },
        ],
    }
}

fn online_form() -> Form {
    Form {
        title: "Make an online installer".into(),
        focus: 0,
        fields: vec![
            Field {
                label: "Version",
                help: "The newest recovery Apple serves for it, which installs the newest release \
                       of that macOS over the network."
                    .into(),
                kind: Kind::Choice(recovery::VERSIONS.iter().map(|v| v.title()).collect(), 0),
            },
            Field {
                label: "Save to",
                help: format!(
                    "The folder for macOS-<name>-online-installer.dmg: empty for the current \
                     directory, {}, which a relative path is from too.",
                    cwd()
                ),
                kind: Kind::Text(String::new(), "current directory"),
            },
        ],
    }
}

fn offline_form(list: &[Installer]) -> Form {
    Form {
        title: "Make an offline installer".into(),
        focus: 0,
        fields: vec![
            Field {
                label: "Version",
                help: format!(
                    "{} releases run on Intel Macs, newest first; Home and End jump to the ends. \
                     The image is about the size given.",
                    list.len()
                ),
                kind: Kind::Choice(
                    list.iter()
                        .map(|i| {
                            format!(
                                "{}, {}, posted {}",
                                i.label(),
                                progress::bytes(i.size),
                                i.posted
                            )
                        })
                        .collect(),
                    0,
                ),
            },
            Field {
                label: "Save to",
                help: format!(
                    "The folder for macOS-<name>-<version>-<build>-offline-installer.img: empty \
                     for the current directory, {}, which a relative path is from too.",
                    cwd()
                ),
                kind: Kind::Text(String::new(), "current directory"),
            },
        ],
    }
}

fn offline_lines(out: &installer::Outputs) -> Vec<Line<'static>> {
    let mut v = vec![
        ok_line(&format!("{} offline installer", out.installer.label())),
        detail("Installer", &abs(&out.image)),
    ];
    v.push(next_line(&format!(
        "reims-ctl create mac --opencore OpenCore.qcow2 --installer {}",
        shell(&abs(&out.image))
    )));
    v.push(detail(
        "Then",
        "in Recovery, erase the VM's disk in Disk Utility (APFS), and in Utilities > Terminal run",
    ));
    v.push(command_line("", installer::IMAGE_COMMAND));
    v
}

fn opencore_lines(out: &opencore::Outputs) -> Vec<Line<'static>> {
    let mut v = vec![
        ok_line(&format!("OpenCore {} boot image", out.opencore)),
        detail("Image", &abs(&out.image)),
    ];
    if let Some(o) = &out.ovmf {
        v.push(detail("OVMF", &abs(&o.code)));
        v.push(detail("", &abs(&o.vars)));
    }
    let s = &out.smbios;
    v.push(detail(
        "Identity",
        &format!(
            "{}, serial {}, MLB {}, UUID {}, ROM {}",
            s.model, s.serial, s.mlb, s.uuid, s.rom
        ),
    ));
    // The newest macOS the model runs.
    let name = if s.model == "iMac20,1" {
        "Tahoe"
    } else {
        "Sequoia"
    };
    v.push(next_line(&format!(
        "reims-ctl create mac --opencore {} --installer macOS-{name}-online-installer.dmg",
        shell(&abs(&out.image))
    )));
    v
}

fn ok_line(s: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("✓ ", Style::new().fg(Color::Green).bold()),
        Span::styled(s.to_string(), Style::new().bold()),
    ])
}

fn detail(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(format!("  {label:<10}")).dim(),
        Span::raw(value.to_string()),
    ])
}

fn next_line(cmd: &str) -> Line<'static> {
    command_line("Next", cmd)
}

fn command_line(label: &str, cmd: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(format!("  {label:<10}")).dim(),
        Span::styled(cmd.to_string(), Style::new().fg(Color::Cyan)),
    ])
}

/// Prints `lines` above the box, into the scrollback, wrapping long ones.
fn print_above(t: &mut DefaultTerminal, lines: Vec<Line<'static>>) -> Result<()> {
    let width = t.size()?.width.max(20);
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Rows as the paragraph wraps them, at word boundaries.
    let height = p.line_count(width) as u16 + 1;
    t.insert_before(height, |buf| {
        use ratatui::widgets::Widget;
        let area = Rect::new(0, 0, buf.area.width, height - 1);
        p.render(area, buf);
    })?;
    Ok(())
}

/// `~/x` as `$HOME/x`.
fn expand(s: &str) -> PathBuf {
    let s = s.trim();
    match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ if s == "~" => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
        _ if s.is_empty() => PathBuf::from("."),
        _ => PathBuf::from(s),
    }
}

fn abs(p: &Path) -> String {
    std::path::absolute(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .display()
        .to_string()
}

/// A path as a shell word.
fn shell(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}
