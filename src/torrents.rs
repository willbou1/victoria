use anyhow::{Result, anyhow};
use tokio_util::sync::CancellationToken;
use tracing::{error};
use std::{
    path::PathBuf,
    time::Duration,
    io::{stdout, Write},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, watch},
    task::JoinHandle,
    fs,
    select,
};
use crossterm::{
    execute,
    terminal,
    cursor::{MoveTo},
    event::{Event, KeyCode, KeyModifiers, EnableMouseCapture, DisableMouseCapture, MouseEventKind, MouseButton, self},
};
use interprocess::local_socket::{
    tokio::prelude::*,
    GenericNamespaced, ListenerOptions, ToNsName,
};
use directories::{BaseDirs, UserDirs};
use bencodedit::{accessor::BencodeAccessor, BencodeValue};
use super::libvictoria::{
    torrent::{Torrent, control::*},
    dht::{DHTManager, DHTEvent},
    types::*
};
use whodatrow::{*, state::*};

struct TorrentTask {
    task: JoinHandle<Result<()>>,
    tx: mpsc::Sender<Command>,
    rx: watch::Receiver<Progress>,
}

async fn add_torrent(
    torrent_tasks: &mut Vec<TorrentTask>,
    config_rx: watch::Receiver<Config>,
    dht_tx: mpsc::Sender<DHTEvent>,
    uri: &str,
) -> Result<()> {
    let torrent = if uri.starts_with("magnet:?") {
        Torrent::from_magnet(uri, config_rx, dht_tx).await
    } else {
        Torrent::from_torrent_file(&PathBuf::from(uri), config_rx, dht_tx).await
    }?;

    if let Some(mut torrent) = torrent {
        torrent_tasks.push(TorrentTask {
            rx: torrent.progress_rx.clone(),
            tx: torrent.command_tx.clone(),
            task: tokio::task::Builder::new()
                .name("torrent")
                .spawn( async move {
                    torrent.run().await
                })?
        });
    }
    Ok(())
}

pub fn prepare_terminal() -> Result<()> {
    use std::io::IsTerminal;
    use crossterm::{
        execute,
        cursor::Hide,
        terminal::{EnterAlternateScreen, SetTitle, enable_raw_mode},
    };
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

pub fn restore_terminal() -> Result<()> {
    use crossterm::{
        execute,
        cursor::Show,
        terminal::{LeaveAlternateScreen, disable_raw_mode},
    };
    execute!(
        stdout(),
        Show,
        LeaveAlternateScreen,
        DisableMouseCapture,
    )?;
    disable_raw_mode()?;
    Ok(()) 
}

#[derive(PartialEq, Eq)]
enum KeyState {
    Global,
    ToggleSection,
    FocusSection,
}

async fn send_uris_to_existing(stream: &mut LocalSocketStream, torrent_uris: &[String]) -> Result<()> {
    let bencode = BencodeValue::List(
        torrent_uris.iter().map(
            |u| BencodeValue::ByteString(u.clone().into_bytes())
        ).collect()
    );
    stream.write_all(
        &bencode.to_bytes()
    ).await.map_err(|e| anyhow!(e))
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

pub async fn run_torrents(torrent_uris: &[String]) -> Result<()> {
    let name = "victoria".to_ns_name::<GenericNamespaced>()?;
    match LocalSocketStream::connect(name.clone()).await {
        Ok(mut stream) => {
            send_uris_to_existing(&mut stream, torrent_uris).await?;
            return Ok(());
        }
        Err(_) => ()
    }
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()?;

    let (config_tx, config_rx) = watch::channel(Config {
        data_path: BaseDirs::new().ok_or(anyhow!("Can't get base directories"))?
            .data_dir().join("victoria").join("torrents"),
        download_path: UserDirs::new().ok_or(anyhow!("Can't get user directories"))?
            .download_dir().ok_or(anyhow!("Can't find download directory"))?
            .join("victoria"),
        client_id: PeerId::random(),
    });

    let cancel_dht = CancellationToken::new();
    let dht = DHTManager::new(cancel_dht.clone()).await?;
    let dht_tx = dht.tx.clone();
    let dht_rx = dht.info_rx.clone();
    let dht_task = tokio::task::Builder::new()
        .name("DHT")
        .spawn(dht.run())?;

    let mut torrent_tasks = Vec::new();
    let dir = &config_rx.borrow().data_path;
    fs::create_dir_all(dir).await?;
    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            let mut torrent = Torrent::from_info_hash(
                path.file_name().unwrap().to_string_lossy().as_ref(),
                config_rx.clone(),
                dht_tx.clone(),
            ).await?;

            torrent_tasks.push(TorrentTask {
                rx: torrent.progress_rx.clone(),
                tx: torrent.command_tx.clone(),
                task: tokio::task::Builder::new()
                    .name("torrent")
                    .spawn( async move {
                        torrent.run().await
                    })?
            });
        }
    }
    
    for arg in torrent_uris {
        add_torrent(&mut torrent_tasks, config_rx.clone(), dht_tx.clone(), arg).await?;
    }

    let mut interval = tokio::time::interval(Duration::from_millis(50));
    let mut table = Table::<Progress>::new(2);
    let mut state = TableState::new::<Progress>(true);
    let mut vertical_position: usize = 0;
    let mut key_state = KeyState::Global;

    prepare_terminal()?;
    let mut out = stdout();
    'main: loop {
        select! {
            result = listener.accept() => {
                let mut stream = result?;
                let mut buf = Vec::new();
                stream.read_to_end(&mut buf).await?;
                let uris = read_uris(&buf).map_err(|e| anyhow!(e))?;
                for uri in uris {
                    add_torrent(&mut torrent_tasks, config_rx.clone(), dht_tx.clone(), &uri).await?;
                }
            }
            _ = interval.tick() => {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) => {
                            use KeyCode::*;
                            if key_state == KeyState::ToggleSection && let Char(key) = key.code {
                                state.handle_event(TableEvent::ToggleSection(key));
                                key_state = KeyState::Global;
                            } else if key_state == KeyState::FocusSection && let Char(key) = key.code {
                                state.handle_event(TableEvent::FocusSection(key));
                                key_state = KeyState::Global;
                            } else {
                                key_state = KeyState::Global;
                                match key.code {
                                    Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break 'main,
                                    Char('q') => break 'main,
                                    Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                        vertical_position += 3,
                                    Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                        vertical_position = vertical_position.saturating_sub(3),

                                    Char('t') => key_state = KeyState::ToggleSection,
                                    Char('f') => key_state = KeyState::FocusSection,

                                    Char('d') | Char('r') => {
                                        if let Some(Focus {row: torrent_hash, ..}) = state.follow_focus() {
                                            if let Some(index) = torrent_tasks
                                                .iter()
                                                .position(|tt| tt.rx.borrow().id() == torrent_hash)
                                            {
                                                match key.code {
                                                    Char('d') => {
                                                        let tt = torrent_tasks.remove(index);
                                                        let _ = tt.tx.send(Command::Delete).await;
                                                        tt.task.await??;
                                                    }
                                                    Char('r') => {
                                                        let tt = &torrent_tasks[index];
                                                        let _ = tt.tx.send(Command::ReloadTrackers).await;
                                                    }
                                                    _ => (),
                                                }
                                            }
                                        }
                                    }
                                    Esc => state.handle_event(TableEvent::UnfocusSection),
                                    Char('m')=> state.handle_event(TableEvent::Mark),
                                    Char('u')=> state.handle_event(TableEvent::Unmark),
                                    Char('+') => state.handle_event(TableEvent::ToggleTotal),
                                    Char('g') | KeypadBegin => state.handle_event(TableEvent::First),
                                    Char('G') | End => state.handle_event(TableEvent::Last),
                                    Char('j') | Down => state.handle_event(TableEvent::Down),
                                    Char('k') | Up => state.handle_event(TableEvent::Up),
                                    Tab => state.handle_event(TableEvent::ToggleSub(None)),
                                    _ => (),
                                }
                            }
                        }
                        Event::Mouse(mouse) => {
                            use MouseEventKind::*;
                            use MouseButton::*;
                            match mouse.kind {
                                ScrollDown => vertical_position += 3,
                                ScrollUp => vertical_position = vertical_position.saturating_sub(3),
                                Down(Left) => state.handle_event(TableEvent::Select(
                                    mouse.column as usize,
                                    mouse.row as usize + vertical_position,
                                )),
                                Down(Right) => state.handle_event(TableEvent::ToggleSub(Some((
                                    mouse.column as usize,
                                    mouse.row as usize + vertical_position,
                                )))),
                                _ => (),
                            }
                        }
                        _ => continue,
                    }
                }

                let rows: Vec<_> = torrent_tasks.iter()
                    .map(|tt| tt.rx.borrow())
                    .collect();
                let (width, height) = terminal::size()?;

                let mut frame = table.render(rows.iter().map(|r| &**r), &mut state, width.into()).to_string();

                {
                    const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
                    let mut test_data: Vec<_> = dht_rx.borrow().buckets.as_ref().iter()
                        .map(|b| b.nodes.len()).collect();
                    let num = test_data.len();
                    let height = 7;
                    if (width as usize) < num {
                        test_data = test_data.chunks(num.div_ceil(width as usize))
                        .map(|data| data.iter().sum())
                        .collect()
                    }
                    let bar_width = width as usize / test_data.len();
                    let taken = bar_width * test_data.len();
                    let remaining = width as usize - taken;
                    let max = *test_data.iter().max().unwrap();
                    let num_div = height * 8;
                    for l in 0..height {
                        let d_lower = (height - 1 - l) * 8;
                        for v in &test_data {
                            let d = (num_div * v).checked_div(max).unwrap_or(0);
                            let d_sub = (d as i32 - d_lower as i32).max(0).min(8);
                            frame.extend(std::iter::repeat_n(BLOCKS[d_sub as usize], bar_width));
                        }
                        frame.extend(std::iter::repeat_n(' ', remaining));
                        frame.push_str("\r\n");
                    }
                    frame.extend(std::iter::repeat_n('―', taken));
                }
                let _ = frame.pop();

                frame = frame
                    .lines().skip(vertical_position)
                    .take(height as usize)
                    .collect::<Vec<_>>()
                    .join("\r\n");

                let fill = height.saturating_sub(frame.lines().count() as u16);
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
    
    restore_terminal()?;

    cancel_dht.cancel();
    let mut errors = Vec::new();
    for torrent_task in torrent_tasks {
        let _ = torrent_task.tx.send(Command::Stop).await;

        match torrent_task.task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => errors.push(err),
            Err(err) => errors.push(anyhow!("Torrent task panicked: {err}")),
        }
    }
    match dht_task.await {
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
            errors.iter()
                .map(|e| format!("{e:#}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    
    Ok(())
}
