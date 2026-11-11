use anyhow::{Result, anyhow};
use bencodedit::{BencodeValue, accessor::BencodeAccessor};
use crossterm::{
    cursor::MoveTo,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    style::Stylize,
    terminal,
};
use directories::{BaseDirs, UserDirs};
use interprocess::local_socket::{
    self, GenericNamespaced, ListenerOptions, ToNsName, tokio::prelude::*,
};
use std::{
    io::{Write, stdout},
    path::PathBuf,
    sync::LazyLock,
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    select,
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::error;

use super::{
    libvictoria::{
        dht::{DHTEvent, DHTManager},
        torrent::{Torrent, control::*},
        types::*,
    },
    rows::*,
};
use whodatrow::{state::*, *};

static IPC_NAME: LazyLock<local_socket::Name> =
    LazyLock::new(|| "victoria".to_ns_name::<GenericNamespaced>().unwrap());

struct TorrentTask {
    task: JoinHandle<Result<()>>,
    tx: mpsc::Sender<Command>,
    rx: watch::Receiver<Progress>,
}

#[derive(PartialEq, Eq)]
enum KeyState {
    Global,
    ToggleSection,
    FocusSection,
}

pub struct MainUI {
    config_rx: watch::Receiver<Config>,
    config_tx: watch::Sender<Config>,

    dht_cancel: CancellationToken,
    dht_tx: mpsc::Sender<DHTEvent>,
    dht_rx: watch::Receiver<DHTInfo>,
    dht_task: JoinHandle<Result<()>>,

    torrent_tasks: Vec<TorrentTask>,
    state: TableState<Section>,
    scroll: usize,
    key_state: KeyState,
}

impl MainUI {
    pub async fn new() -> Result<Self> {
        let (config_tx, config_rx) = watch::channel(Config {
            data_path: BaseDirs::new()
                .ok_or(anyhow!("Can't get base directories"))?
                .data_dir()
                .join("victoria")
                .join("torrents"),
            download_path: UserDirs::new()
                .ok_or(anyhow!("Can't get user directories"))?
                .download_dir()
                .ok_or(anyhow!("Can't find download directory"))?
                .join("victoria"),
            client_id: PeerId::random(),
        });

        let dht = DHTManager::new().await?;

        Ok(Self {
            config_rx,
            config_tx,

            dht_cancel: dht.cancel.clone(),
            dht_tx: dht.tx.clone(),
            dht_rx: dht.info_rx.clone(),
            dht_task: tokio::task::Builder::new().name("DHT").spawn(dht.run())?,

            torrent_tasks: Vec::new(),
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

    fn read_uris(buf: &[u8]) -> Result<Vec<String>> {
        let mut uris = Vec::new();
        let root_val = BencodeValue::from_bytes(&buf)?.0;
        let root = BencodeAccessor::new(&root_val);

        for uri in root.as_str_list()? {
            uris.push(uri.to_string());
        }
        Ok(uris)
    }

    async fn add_torrent(&mut self, uri: &str) -> Result<()> {
        let config_rx = self.config_rx.clone();
        let dht_tx = self.dht_tx.clone();
        let torrent = if uri.starts_with("magnet:?") {
            Torrent::from_magnet(uri, config_rx, dht_tx).await
        } else {
            Torrent::from_torrent_file(&PathBuf::from(uri), config_rx, dht_tx).await
        }?;

        if let Some(mut torrent) = torrent {
            self.torrent_tasks.push(TorrentTask {
                rx: torrent.progress_rx.clone(),
                tx: torrent.command_tx.clone(),
                task: tokio::task::Builder::new()
                    .name("torrent")
                    .spawn(async move { torrent.run().await })?,
            });
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
        if let Some(Focus {
            row: torrent_hash,
            section,
        }) = self.state.follow_focus()
        {
            if let Some(torrent_index) = self
                .torrent_tasks
                .iter()
                .position(|tt| tt.rx.borrow().id() == torrent_hash)
            {
                match key {
                    Char('d') => {
                        let tt = self.torrent_tasks.remove(torrent_index);
                        let _ = tt.tx.send(Command::Delete).await;
                        tt.task.await??;
                    }
                    Char('r') => {
                        let tt = &self.torrent_tasks[torrent_index];
                        let _ = tt.tx.send(Command::ReloadTrackers).await;
                    }
                    Char('s') => {
                        let tt = &self.torrent_tasks[torrent_index];
                        let _ = tt.tx.send(Command::ToggleSequential).await;
                    }
                    Char('p') => {
                        if let Some((Section::Progress(ProgressSection::Files), state)) = section {
                            if let Some(Focus { row: file_hash, .. }) = self.state.follow_focus() {
                                if let Some((file_index, priority)) = self.torrent_tasks
                                    [torrent_index]
                                    .rx
                                    .borrow()
                                    .transfer
                                    .as_ref()
                                    .and_then(|t| {
                                        t.files
                                            .iter()
                                            .enumerate()
                                            .find(|(_, file)| file.id() == file_hash)
                                            .map(|(f, file)| (f, file.priority))
                                    })
                                {
                                    let tt = &self.torrent_tasks[torrent_index];
                                    let _ = tt
                                        .tx
                                        .send(Command::SetPriority(file_index, priority.next()))
                                        .await;
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
        let listener = ListenerOptions::new()
            .name(IPC_NAME.borrow())
            .create_tokio()?;

        {
            let dir = &self.config_rx.borrow().data_path;
            fs::create_dir_all(dir).await?;
            let mut entries = fs::read_dir(dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.is_dir() {
                    let mut torrent = Torrent::from_info_hash(
                        path.file_name().unwrap().to_string_lossy().as_ref(),
                        self.config_rx.clone(),
                        self.dht_tx.clone(),
                    )
                    .await?;

                    self.torrent_tasks.push(TorrentTask {
                        rx: torrent.progress_rx.clone(),
                        tx: torrent.command_tx.clone(),
                        task: tokio::task::Builder::new()
                            .name("torrent")
                            .spawn(async move { torrent.run().await })?,
                    });
                }
            }
        }

        for arg in torrent_uris {
            self.add_torrent(arg).await?;
        }

        let mut interval = tokio::time::interval(Duration::from_millis(50));
        let mut table = Table::<Progress>::new(2);
        Self::prepare_terminal()?;
        let mut out = stdout();
        'main: loop {
            select! {
                result = listener.accept() => {
                    let mut stream = result?;
                    let mut buf = Vec::new();
                    stream.read_to_end(&mut buf).await?;
                    let uris = Self::read_uris(&buf).map_err(|e| anyhow!(e))?;
                    for uri in uris {
                        self.add_torrent(&uri).await?;
                    }
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

                    let rows: Vec<_> = self.torrent_tasks.iter()
                        .map(|tt| tt.rx.borrow())
                        .collect();
                    let (width, height) = terminal::size()?;

                    let mut frame = table.render(rows.iter().map(|r| &**r), &mut self.state, width.into()).to_string();

                    {
                        const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

                        let mut test_data: Vec<_> = self.dht_rx.borrow().buckets.as_ref().iter()
                            .map(|b| b.nodes.len())
                            .collect();

                        let height = 4;
                        let width = width as usize;
                        let max = *test_data.iter().max().unwrap_or(&0);
                        let scale_width = max.to_string().len() + 3; // label + " ┤ "

                        let plot_width = width.saturating_sub(scale_width);

                        if !test_data.is_empty() && plot_width < test_data.len() {
                            let chunk_size = test_data.len().div_ceil(plot_width.max(1));
                            test_data = test_data.chunks(chunk_size)
                                .map(|data| data.iter().sum())
                                .collect();
                        }

                        if test_data.is_empty() || plot_width == 0 {
                            //return;
                        }

                        let bar_width = plot_width / test_data.len();
                        let taken = bar_width * test_data.len();
                        let remaining = plot_width - taken;

                        let max = *test_data.iter().max().unwrap_or(&0);
                        let max_width = max.to_string().len();
                        let scale_width = max_width + 3;
                        let num_div = height * 8;

                        for l in 0..height {
                            if l == 0 {
                                frame.push_str(&format!("{max:>max_width$} ┤ "));
                            } else if l == height - 1 {
                                frame.push_str(&format!("{:>max_width$} ┤ ", 0));
                            } else {
                                frame.push_str(&format!("{:>max_width$} │ ", ""));
                            }

                            let d_lower = (height - 1 - l) * 8;

                            for v in &test_data {
                                let d = (num_div * *v).checked_div(max).unwrap_or(0);
                                let d_sub = (d as i32 - d_lower as i32).clamp(0, 8);
                                let block = BLOCKS[d_sub as usize];
                                let colored = if *v == 0 {
                                    block.dark_grey()
                                } else if *v * 4 < max {
                                    block.blue()
                                } else if *v * 2 < max {
                                    block.green()
                                } else if *v * 4 < max * 3 {
                                    block.yellow()
                                } else {
                                    block.red()
                                }.to_string();
                                frame.extend(std::iter::repeat_n(colored, bar_width));
                            }

                            frame.extend(std::iter::repeat_n(' ', remaining));
                            frame.push_str("\r\n");
                        }

                        frame.extend(std::iter::repeat_n(' ', scale_width - 3));
                        frame.push_str(" ╰");
                        frame.extend(std::iter::repeat_n('─', taken.saturating_sub(2)));

                        let _ = frame.pop();
                    }
                    let _ = frame.pop();

                    frame = frame
                        .lines().skip(self.scroll)
                        .take(height as usize)
                        .collect::<Vec<_>>()
                        .join("\r\n");

                    let fill = height.saturating_sub((table.num_row() + 5) as u16);
                    for l in 0..fill {
                        frame.extend(std::iter::repeat_n(' ', width.into()));
                        if l < fill - 1 {
                            frame.push('\r');
                            frame.push('\n');
                        }
                    }

                    execute!(out, MoveTo(0, 0))?;
                    write!(out, "{frame}")?;
                    out.flush()?;
                }
            }
        }

        Self::restore_terminal()?;

        self.dht_cancel.cancel();
        for torrent_task in &self.torrent_tasks {
            let _ = torrent_task.tx.send(Command::Stop).await;
        }

        let mut errors = Vec::new();
        for torrent_task in self.torrent_tasks {
            match torrent_task.task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => errors.push(err),
                Err(err) => errors.push(anyhow!("Torrent task panicked: {err}")),
            }
        }
        match self.dht_task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => errors.push(err),
            Err(err) => errors.push(anyhow!("DHT task panicked: {err}")),
        }
        if !errors.is_empty() {
            for error in &errors {
                error!("{:#}", error);
            }
            return Err(anyhow!(
                "Errors while shutting down:\n{}",
                errors
                    .iter()
                    .map(|e| format!("{e:#}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        Ok(())
    }
}
