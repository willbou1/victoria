use anyhow::{Result, anyhow};
use bencodedit::{BencodeValue, accessor::BencodeAccessor};
use crossterm::{
    cursor::MoveTo,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute, terminal,
};
use interprocess::local_socket::{
    self, GenericNamespaced, ListenerOptions, ToNsName, tokio::prelude::*,
};
use std::{
    io::{Write, stdout},
    sync::LazyLock,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    select,
    sync::watch,
};
use tracing::warn;

use crate::{config::AppConfig, theme::Theme};

use super::{
    libvictoria::{TorrentClient, torrent::control::*, types::*},
    rows::*,
};
use whodatrow::{histogram::*, state::*, *};

static IPC_NAME: LazyLock<local_socket::Name> =
    LazyLock::new(|| "victoria".to_ns_name::<GenericNamespaced>().unwrap());

#[derive(PartialEq, Eq)]
enum KeyState {
    Global,
    ToggleSection,
    FocusSection,
}

pub struct MainUI {
    config_rx: watch::Receiver<Config>,
    config_tx: watch::Sender<Config>,

    client: TorrentClient,
    histogram: Histogram,
    table: Table<Progress>,
    state: TableState<Section>,
    scroll: usize,
    key_state: KeyState,
}

impl MainUI {
    pub async fn new() -> Result<Self> {
        let app_config = AppConfig::read().await?;
        app_config.write().await?;
        let (config_tx, config_rx) = watch::channel(Config {
            data_path: app_config.data_path,
            download_path: app_config.download_path,
            client_id: app_config.bit_torrent.id.parse()?,
            bit_torrent_port: app_config.bit_torrent.port,
            connection_cap: app_config.bit_torrent.connection_cap,
            discovery_cap: app_config.bit_torrent.discovery_cap,

            node_id: app_config.dht.id.parse()?,
            dht_port: app_config.dht.port,
        });

        Ok(Self {
            client: TorrentClient::new(config_rx.clone(), config_tx.clone()).await?,
            config_rx,
            config_tx,

            histogram: Histogram::new(4),
            table: Table::new(2),
            state: TableState::new::<Progress>(true),
            scroll: 0,
            key_state: KeyState::Global,
        })
    }

    fn prepare_terminal() -> Result<()> {
        use crossterm::{
            cursor::Hide,
            execute,
            terminal::{EnterAlternateScreen, SetTitle, enable_raw_mode},
        };
        use std::io::IsTerminal;
        let interactive_terminal =
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        assert!(interactive_terminal, "This program only runs in a terminal");
        execute!(
            stdout(),
            SetTitle("Victoria"),
            EnterAlternateScreen,
            Hide,
            EnableMouseCapture,
        )?;
        enable_raw_mode()?;
        Ok(())
    }

    fn restore_terminal() -> Result<()> {
        use crossterm::{
            cursor::Show,
            execute,
            terminal::{LeaveAlternateScreen, disable_raw_mode},
        };
        execute!(stdout(), Show, LeaveAlternateScreen, DisableMouseCapture,)?;
        disable_raw_mode()?;
        Ok(())
    }

    async fn send_uris_to_existing(
        stream: &mut LocalSocketStream,
        torrent_uris: &[String],
    ) -> Result<()> {
        let bencode = BencodeValue::List(
            torrent_uris
                .iter()
                .map(|u| BencodeValue::ByteString(u.clone().into_bytes()))
                .collect(),
        );
        stream
            .write_all(&bencode.to_bytes())
            .await
            .map_err(|e| anyhow!(e))
    }

    async fn receive_uris(&mut self, mut stream: LocalSocketStream) -> Result<()> {
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await?;
        let root_val = BencodeValue::from_bytes(&buf)?.0;
        let root = BencodeAccessor::new(&root_val);
        for uri in root.as_str_list()? {
            self.client.add_torrent(uri).await?;
        }
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent) {
        use MouseButton::*;
        use MouseEventKind::*;
        match event.kind {
            ScrollDown => self.scroll += 3,
            ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            Down(Left) => self.state.handle_event(TableEvent::Select(
                event.column as usize,
                event.row as usize + self.scroll,
            )),
            Down(Right) => self.state.handle_event(TableEvent::ToggleSub(Some((
                event.column as usize,
                event.row as usize + self.scroll,
            )))),
            _ => (),
        }
    }

    async fn handle_torrent_event(&mut self, key: KeyCode) -> Result<()> {
        use KeyCode::*;
        for Focus {
            row: torrent_hash,
            section,
        } in self.state.follow_focus()
        {
            if let Some((hash, torrent)) = self
                .client
                .torrents()
                .await
                .iter()
                .find(|(_, t)| t.progress().id() == torrent_hash)
            {
                match key {
                    Char('d') => {
                        self.client.remove_torrent(&hash).await?;
                    }
                    Char('r') => {
                        torrent.reload().await;
                    }
                    Char('s') => {
                        torrent.toggle_sequential().await;
                    }
                    Char('p') => {
                        if let Some((Section::Progress(ProgressSection::Files), file_section)) =
                            section
                        {
                            for Focus { row: file_hash, .. } in file_section.follow_focus() {
                                if let Some((file_index, priority)) =
                                    torrent.progress().transfer.as_ref().and_then(|t| {
                                        t.files
                                            .iter()
                                            .enumerate()
                                            .find(|(_, file)| file.id() == file_hash)
                                            .map(|(f, file)| (f, file.priority))
                                    })
                                {
                                    torrent.set_priority(file_index, priority.next()).await;
                                }
                            }
                        }
                    }
                    _ => (),
                }
            }
        }
        Ok(())
    }

    async fn handle_key_event(&mut self, event: KeyEvent) -> Result<bool> {
        use KeyCode::*;
        if self.key_state == KeyState::ToggleSection
            && let Char(key) = event.code
        {
            self.state.handle_event(TableEvent::ToggleSection(key));
            self.key_state = KeyState::Global;
        } else if self.key_state == KeyState::FocusSection
            && let Char(key) = event.code
        {
            self.state.handle_event(TableEvent::FocusSection(key));
            self.key_state = KeyState::Global;
        } else {
            self.key_state = KeyState::Global;
            match event.code {
                Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
                Char('q') => return Ok(true),
                Char('d') if event.modifiers.contains(KeyModifiers::CONTROL) => self.scroll += 3,
                Char('u') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.scroll = self.scroll.saturating_sub(3)
                }

                Char('t') => self.key_state = KeyState::ToggleSection,
                Char('f') => self.key_state = KeyState::FocusSection,

                Char('d') | Char('r') | Char('s') | Char('p') => {
                    self.handle_torrent_event(event.code).await?;
                }
                Esc => self.state.handle_event(TableEvent::UnfocusSection),
                Char('m') => self.state.handle_event(TableEvent::Mark),
                Char('u') => self.state.handle_event(TableEvent::Unmark),
                Char('+') => self.state.handle_event(TableEvent::ToggleTotal),
                Char('g') | KeypadBegin => self.state.handle_event(TableEvent::First),
                Char('G') | End => self.state.handle_event(TableEvent::Last),
                Char('j') | Down => self.state.handle_event(TableEvent::Down),
                Char('k') | Up => self.state.handle_event(TableEvent::Up),
                Tab => self.state.handle_event(TableEvent::ToggleSub(None)),
                _ => (),
            }
        }
        return Ok(false);
    }

    pub async fn run(mut self, torrent_uris: &[String]) -> Result<()> {
        match LocalSocketStream::connect(IPC_NAME.borrow()).await {
            Ok(mut stream) => {
                Self::send_uris_to_existing(&mut stream, torrent_uris).await?;
                return Ok(());
            }
            Err(_) => (),
        }
        let ipc_listener = ListenerOptions::new()
            .name(IPC_NAME.borrow())
            .create_tokio()?;

        self.client.start().await?;
        for arg in torrent_uris {
            self.client.add_torrent(arg.as_str()).await?;
        }

        let mut interval = tokio::time::interval(Duration::from_millis(50));
        Self::prepare_terminal()?;
        let mut out = stdout();
        'main: loop {
            select! {
                result = ipc_listener.accept() => {
                    self.receive_uris(result?).await?;
                }
                _ = interval.tick() => {
                    while event::poll(Duration::ZERO)? {
                        match event::read()? {
                            Event::Key(e) => if self.handle_key_event(e).await? {
                                break 'main;
                            },
                            Event::Mouse(e) => self.handle_mouse_event(e),
                            _ => continue,
                        }
                    }

                    let torrents = self.client.torrents().await;
                    let mut rows: Vec<_> = torrents.values()
                        .map(|t| t.progress())
                        .collect();
                    rows.sort_by_key(|t| t.info_hash);

                    let (width, height) = terminal::size()?;
                    let width = width as usize;
                    let height = height as usize;

                    let mut frame = self.table.render(rows.iter().map(|r| &**r), &mut self.state, width).to_string();

                    if let Some(dht_info) = self.client.dht_info() {
                        let buckets: Vec<_> = dht_info
                            .buckets
                            .as_ref()
                            .iter()
                            .map(|b| b.nodes.len())
                            .collect();
                        frame.push_str(&self.histogram.render::<Theme>(buckets, width));
                    }

                    frame = frame
                        .lines().skip(self.scroll)
                        .take(height)
                        .collect::<Vec<_>>()
                        .join("\r\n");

                    let fill = height.saturating_sub(self.table.num_row() - self.scroll + 5);
                    for l in 0..fill {
                        frame.extend(std::iter::repeat_n(' ', width));
                        if l < fill - 1 {
                            frame.push_str("\r\n");
                        }
                    }

                    execute!(out, MoveTo(0, 0))?;
                    write!(out, "{frame}")?;
                    out.flush()?;
                }
            }
        }

        Self::restore_terminal()?;
        self.client.stop().await
    }
}
