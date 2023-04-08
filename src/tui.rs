use crate::errors::*;
use crate::iocs;
use crate::rules;
use crate::scan;
use crate::utils;
use crossterm::event::EventStream;
use crossterm::event::{KeyEvent, KeyModifiers};
use crossterm::{
    event::{Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use forensic_adb::{AndroidStorageInput, DeviceInfo, Host};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Span, Spans, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use std::io;
use std::io::Stdout;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

const DARK_GREY: Color = Color::Rgb(0x3b, 0x3b, 0x3b);

#[derive(Debug)]
pub enum Message {
    ScanEnded,
    Suspicion(iocs::Suspicion),
}

pub struct App {
    adb_host: Host,
    events_tx: mpsc::Sender<Message>,
    events_rx: mpsc::Receiver<Message>,
    devices: Vec<DeviceInfo>,
    cursor: usize,
    report: Option<Vec<iocs::Suspicion>>,
    cancel_scan: Option<mpsc::Sender<()>>,
}

impl App {
    pub fn new(adb_host: Host) -> Self {
        let (events_tx, events_rx) = mpsc::channel(5);
        Self {
            adb_host,
            events_tx,
            events_rx,
            devices: Vec::new(),
            cursor: 0,
            report: None,
            cancel_scan: None,
        }
    }

    pub async fn init(&mut self) -> Result<()> {
        let devices = self
            .adb_host
            .devices::<Vec<_>>()
            .await
            .map_err(|e| anyhow!("Failed to list devices from adb: {}", e))?;
        self.devices = devices;

        Ok(())
    }

    pub fn key_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn key_down(&mut self) {
        let max = self.devices.len().saturating_sub(1);
        if self.cursor < max {
            self.cursor += 1;
        }
    }

    pub async fn refresh_devices(&mut self) -> Result<()> {
        let devices = self
            .adb_host
            .devices::<Vec<_>>()
            .await
            .map_err(|e| anyhow!("Failed to list devices from adb: {}", e))?;
        self.devices = devices;
        if self.devices.get(self.cursor).is_none() {
            self.cursor = match self.devices.len() {
                0 => 0,
                n => n - 1,
            };
        }
        Ok(())
    }
}

pub enum Action {
    Shutdown,
    Clear,
}

pub async fn run_scan(
    adb_host: Host,
    device: DeviceInfo,
    events_tx: mpsc::Sender<Message>,
) -> Result<()> {
    let device = adb_host
        .clone()
        .device_or_default(Some(&device.serial), AndroidStorageInput::Auto)
        .await
        .with_context(|| anyhow!("Failed to access device: {:?}", device.serial))?;

    let repo = iocs::Repository::ioc_file_path()?;
    let (rules, _sha256) = rules::load_map_from_file(repo).context("Failed to load rules")?;
    scan::run(
        &device,
        &rules,
        &mut scan::Settings { skip_apps: false },
        &mut scan::ScanNotifier::Channel(events_tx),
    )
    .await?;

    Ok(())
}

pub async fn handle_key(app: &mut App, event: Event) -> Result<Option<Action>> {
    match event {
        Event::Key(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            ..
        })
        | Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
        | Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            ..
        }) => {
            if let Some(tx) = app.cancel_scan.take() {
                tx.send(()).await.ok();
            } else if app.report.take().is_none() {
                println!("Exiting...");
                return Ok(Some(Action::Shutdown));
            }
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char('Q'),
            modifiers: KeyModifiers::SHIFT,
            ..
        }) => {
            println!("Exiting...");
            return Ok(Some(Action::Shutdown));
        }
        Event::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            ..
        }) => {
            let adb_host = app.adb_host.clone();
            let device = app.devices[app.cursor].clone();
            let events_tx = app.events_tx.clone();

            let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel_rx.recv() => {
                        debug!("Scan has been canceled");
                        events_tx.send(Message::ScanEnded).await.ok();
                    }
                    ret = run_scan(adb_host, device, events_tx.clone()) => {
                        debug!("Scan has completed: {:?}", ret); // TODO print errors in UI
                        events_tx.send(Message::ScanEnded).await.ok();
                    }
                }
            });
            app.report = Some(Vec::new());
            app.cancel_scan = Some(cancel_tx);
        }
        Event::Key(KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            ..
        }) => {
            app.key_up();
        }
        Event::Key(KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            ..
        }) => {
            app.key_down();
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char('r'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) => {
            // TODO: check if we're in device list or report view
            app.refresh_devices().await?;
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char('l'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) => {
            return Ok(Some(Action::Clear));
        }
        _ => (),
    }
    Ok(None)
}

pub async fn run<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    let mut stream = EventStream::new();

    loop {
        terminal.draw(|f| ui(f, app))?;

        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else { break };
                let event = event.context("Failed to read terminal input")?;
                match handle_key(app, event).await? {
                    Some(Action::Shutdown) => break,
                    Some(Action::Clear) => {
                        terminal.clear()?;
                    },
                    None => (),
                }
            }
            event = app.events_rx.recv() => {
                let Some(event) = event else { break };
                debug!("Received message from channel: event={event:?}");
                match event {
                    Message::ScanEnded => {
                        app.cancel_scan.take();
                    }
                    Message::Suspicion(sus) => {
                        if let Some(report) = &mut app.report {
                            report.push(sus);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

pub fn ui<B: Backend>(f: &mut Frame<'_, B>, app: &App) {
    let white = Style::default().fg(Color::White).bg(Color::Black);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ]
            .as_ref(),
        )
        .split(f.size());

    let text = Text::from(Spans::from(vec![
        Span::raw(if app.cancel_scan.is_some() {
            "scanning - "
        } else {
            "idle - "
        }),
        Span::raw("Press "),
        Span::styled("ESC", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" to exit - "),
        Span::raw(env!("CARGO_PKG_NAME")),
        Span::raw(" v"),
        Span::raw(env!("CARGO_PKG_VERSION")),
    ]));
    let help_message = Paragraph::new(text)
        .style(white)
        .alignment(Alignment::Right);
    f.render_widget(help_message, chunks[0]);

    f.render_widget(Block::default().style(white), chunks[1]);

    let widget = if let Some(report) = &app.report {
        let findings: Vec<ListItem> = report
            .iter()
            .map(|sus| ListItem::new(format!("{sus:?}")))
            .collect();

        let title = Span::styled("Findings", white.add_modifier(Modifier::BOLD));
        List::new(findings).block(
            Block::default()
                .borders(Borders::ALL)
                .style(white)
                .border_style(Style::default().fg(Color::Green))
                .title(title),
        )
    } else {
        let devices: Vec<ListItem> = app
            .devices
            .iter()
            .enumerate()
            .map(|(i, device)| {
                let selected = i == app.cursor;

                let msg = format!(
                    "{:30} device={:?}, model={:?}, product={:?}",
                    device.serial,
                    utils::human_option_str(device.info.get("device")),
                    utils::human_option_str(device.info.get("model")),
                    utils::human_option_str(device.info.get("product")),
                );

                let content = Spans::from(vec![
                    Span::styled(
                        if selected { " > " } else { "   " },
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(msg),
                ]);

                let mut style = Style::default();
                if selected {
                    style = style.bg(DARK_GREY);
                }

                ListItem::new(content).style(style)
            })
            .collect();

        let title = Span::styled("Connected devices", white.add_modifier(Modifier::BOLD));
        List::new(devices).block(
            Block::default()
                .borders(Borders::ALL)
                .style(white)
                .border_style(Style::default().fg(Color::Green))
                .title(title),
        )
    };
    f.render_widget(widget, chunks[2]);
}

pub fn setup() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub fn cleanup(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen,)?;
    terminal.show_cursor()?;
    Ok(())
}
