use anyhow::Result;
use std::{
    path::PathBuf,
    time::Duration,
    io::{stdout, Write},
    hash::{Hash, Hasher},
    collections::hash_map::DefaultHasher,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use crossterm::{
    execute,
    cursor::{MoveTo, Show, Hide},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, self},
    event::{Event, KeyCode, KeyModifiers, EnableMouseCapture, DisableMouseCapture, self},
};

use libvictoria::{
    torrent::{Torrent, control::*},
    util::*,
    types::*,
};
use super::{
    table::*,
    table::state::*,
};

impl Row for TrackerInfo {
    fn id(&self) -> RowHash {
        let mut hasher = DefaultHasher::new();
        self.url.hash(&mut hasher);
        hasher.finish()
    }

    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column {
                value: |tracker, _| tracker.succeeded.map(
                    |s| String::from(if s {"✓"} else {"✗"})
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "Url",
                value: |tracker, _| tracker.url.clone(),
                ..Column::DEFAULT
            },
            Column {
                header: "T",
                alignment: Right,
                value: |tracker, _| tracker.tier.to_string(),
                ..Column::DEFAULT
            },
            Column {
                header: "Peer",
                alignment: Right,
                value: |tracker, _| tracker.endpoints.as_ref()
                    .map(|e| e.len().to_string()).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "Seed",
                alignment: Right,
                value: |tracker, _| tracker.seeders
                    .map(|s| s.to_string()).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "Leech",
                alignment: Right,
                value: |tracker, _| tracker.leechers
                    .map(|s| s.to_string()).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "Int",
                alignment: Right,
                value: |tracker, _| tracker.interval
                    .map(|i| pretty_duration(i)).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "MinInt",
                alignment: Right,
                value: |tracker, _| tracker.min_interval
                    .map(|m| pretty_duration(m)).unwrap_or_default(),
                ..Column::DEFAULT
            },
        ]
    }

    fn sub_sections() -> &'static [&'static dyn SubSection<Self>]
    where Self: Sized,
    {
        &[
            &TextSubSection {
                header: "Peers",
                key: 'p',
                content: |tracker: &Self, _| {
                    tracker.endpoints.as_ref().map(|e| e.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")).unwrap_or_default()
                },
            },
        ]
    }
}

impl Row for PeerProgress {
    fn id(&self) -> RowHash {
        let mut hasher = DefaultHasher::new();
        self.id.hash(&mut hasher);
        hasher.finish()
    }

    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column::DEFAULT,
            Column {
                header: "Id",
                value: |peer, _| {
                    let string_id = peer.id.to_string();
                    format!("{}..{}", &string_id[..8], &string_id[32..40])
                },
                ..Column::DEFAULT
            },
            Column {
                header: "F P M D",
                value: |peer, _| [
                    peer.supports_fast,
                    peer.supports_pex,
                    peer.supports_metadata,
                    peer.supports_dht
                ].map(|ext| if ext {"■"} else {"□"}).join(" "),
                ..Column::DEFAULT
            },
            Column {
                header: "Client",
                max_width: Some(18),
                value: |peer, _|
                    peer.client.clone().unwrap_or(String::new()),
                ..Column::DEFAULT
            },
            Column {
                header: "Pieces",
                flex: Some(1),
                value: |peer, width| peer.connection.as_ref().map(
                    |c| format!("{:WIDTH$}", c.piece_bitfield, WIDTH = width.unwrap())
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "I C",
                value: |peer, _| peer.connection.as_ref().map(
                    |c| [c.am_interested, c.peer_choking]
                        .map(|ext| if ext {"■"} else {"□"}).join(" ")
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "PC",
                alignment: Right,
                value: |peer, _| peer.connection.as_ref().map(
                    |c| c.piece_cursor.map_or(String::new(), |pc| pc.to_string())
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "PL",
                alignment: Right,
                value: |peer, _| peer.connection.as_ref().map(
                    |c| c.pipeline.to_string()
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "t/s",
                alignment: Right,
                value: |peer, _| peer.connection.as_ref().map(
                    |c| c.timeout_rate.to_string()
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "r/s",
                alignment: Right,
                value: |peer, _| peer.connection.as_ref().map(
                    |c| c.reject_rate.to_string()
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "Down",
                alignment: Right,
                value: |peer, _| peer.connection.as_ref().map(
                    |c| pretty_size(c.down_speed)
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "I C",
                value: |peer, _| peer.connection.as_ref().map(
                    |c| [c.peer_interested, c.am_choking]
                        .map(|ext| if ext {"■"} else {"□"}).join(" ")
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "Up",
                alignment: Right,
                value: |peer, _| peer.connection.as_ref().map(
                    |c| pretty_size(c.up_speed)
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
        ]
    }

    fn sub_sections() -> &'static [&'static dyn SubSection<Self>]
    where Self: Sized,
    {
        &[
            &TextSubSection {
                header: "Errors",
                key: 'r',
                content: |peer: &Self, _| {
                    peer.errors.join("\n")
                },
            },
            &TextSubSection {
                header: "Endpoints",
                key: 'e',
                content: |peer: &Self, _| {
                    peer.endpoints.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            },
        ]
    }
}

impl Row for FileInfo {
    fn id(&self) -> RowHash {
        let mut hasher = DefaultHasher::new();
        self.relative_path.hash(&mut hasher);
        hasher.finish()
    }

    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column {
                header: "Relative path",
                value: |file, _| file.relative_path.clone(),
                ..Column::DEFAULT
            },
            Column {
                header: "Size",
                alignment: Right,
                value: |file, _| pretty_size(file.size),
                ..Column::DEFAULT
            },
        ]
    }
}

impl Row for PieceProgress {
    fn id(&self) -> RowHash {
        let mut hasher = DefaultHasher::new();
        self.index.hash(&mut hasher);
        hasher.finish()
    }

    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column {
                header: "Idx",
                value: |piece, _| piece.index.to_string(),
                ..Column::DEFAULT
            },
            Column {
                header: "Blocks",
                flex: Some(1),
                value: |piece, width|
                    format!("{:WIDTH$}", piece.block_bitfield, WIDTH = width.unwrap()),
                ..Column::DEFAULT
            },
            Column {
                header: "%",
                alignment: Right,
                value: |piece, _|
                    format!("{:.2}", piece.num_obtained_blocks as f64 * 100. / piece.num_blocks as f64),
                ..Column::DEFAULT
            },
            Column {
                header: "Num",
                alignment: Right,
                value: |piece, _| piece.num_obtained_blocks.to_string(),
                ..Column::DEFAULT
            },
            Column {
                header: "Tot",
                alignment: Right,
                value: |piece, _| piece.num_blocks.to_string(),
                ..Column::DEFAULT
            },
        ]
    }
}

impl Row for Progress {
    fn id(&self) -> RowHash {
        let mut hasher = DefaultHasher::new();
        self.display_name.hash(&mut hasher);
        hasher.finish()
    }

    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column {
                value: |_, _| String::from("▶"),
                ..Column::DEFAULT
            },
            Column {
                header: "Con",
                alignment: Right,
                value: |prog, _| prog.num_connected_peers.to_string(),
                total: Some(|rows| {
                    let total: usize = rows.iter()
                        .map(|r| r.num_connected_peers).sum();
                    format!("{total}")
                }),
                ..Column::DEFAULT
            },
            Column {
                header: "Down",
                alignment: Right,
                value: |prog, _| prog.transfer.as_ref()
                    .map(|t| pretty_size(t.down_speed)).unwrap_or_default(),
                total: Some(|rows| {
                    let total: usize = rows.iter()
                        .map(|r| r.transfer.as_ref().map_or(0, |t| t.down_speed)).sum();
                    format!("{}", pretty_size(total))
                }),
                ..Column::DEFAULT
            },
            Column {
                header: "Up",
                alignment: Right,
                value: |prog, _| prog.transfer.as_ref()
                    .map(|t| pretty_size(t.up_speed)).unwrap_or_default(),
                total: Some(|rows| {
                    let total: usize = rows.iter()
                        .map(|r| r.transfer.as_ref().map_or(0, |t| t.up_speed)).sum();
                    format!("{}", pretty_size(total))
                }),
                ..Column::DEFAULT
            },
            Column {
                header: "Name",
                flex: Some(3),
                value: |prog, _| prog.display_name.to_string(),
                ..Column::DEFAULT
            },
            Column {
                header: "Size",
                alignment: Right,
                value: |prog, _| prog.transfer.as_ref()
                    .map(|t| pretty_size(t.size)).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "Pieces",
                flex: Some(1),
                value: |prog, width| prog.transfer.as_ref()
                    .map_or(
                        format!("{:WIDTH$}", prog.metadata_bitfield, WIDTH = width.unwrap()),
                        |t| format!("{:WIDTH$}", t.piece_bitfield, WIDTH = width.unwrap())
                    ),
                ..Column::DEFAULT
            },
            Column {
                header: "%",
                alignment: Right,
                value: |prog, _|
                    prog.transfer.as_ref().map(
                        |t|  format!("{:.2}", t.percentage())
                    ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "ETA",
                alignment: Right,
                value: |prog, _| prog.transfer.as_ref().map(
                    |t| t.eta().map_or(String::new(), |e| pretty_duration(e))
                ).unwrap_or_default(),
                total: Some(|rows| {
                    let max = rows.iter()
                        .map(|r| r.transfer.as_ref().and_then(|t| t.eta())).max();
                    max.map(|e| e.map(pretty_duration).unwrap_or_default()).unwrap_or_default()
                }),
                ..Column::DEFAULT
            },
        ]
    }

    fn sub_sections() -> &'static [&'static dyn SubSection<Self>]
    where Self: Sized,
    {
        &[
            &TableSubSection {
                header: "Files",
                key: 'f',
                content: |prog: &Self, _| {
                    prog.transfer.as_ref().map(|transfer| {
                        let table = Table::new(1);
                        (table, transfer.files.iter().collect())
                    })
                },
            },
            &TableSubSection {
                header: "Blocks",
                key: 'b',
                content: |prog: &Self, _| {
                    prog.transfer.as_ref().map(|transfer| {
                        let table = Table::new(1);
                        (table, transfer.active_pieces.iter().collect())
                    })
                },
            },
            &TableSubSection {
                header: "Peers",
                key: 'p',
                content: |prog: &Self, _| {
                    let table = Table::new(1);
                    Some((table, prog.peers.values().collect()))
                },
            },
            &TableSubSection {
                header: "Trackers",
                key: 't',
                content: |prog: &Self, _| {
                    let table = Table::new(1);
                    Some((table, prog.trackers.values().collect()))
                },
            },
        ]
    }
}

struct TorrentTask {
    task: JoinHandle<Result<()>>,
    tx: mpsc::Sender<Command>,
    rx: watch::Receiver<Progress>,
}

pub fn prepare_terminal() -> Result<()> {
    execute!(
        stdout(),
        EnterAlternateScreen,
        Hide,
        EnableMouseCapture,
    )?;
    terminal::enable_raw_mode()?;
    Ok(())
}

pub fn restore_terminal() -> Result<()> {
    execute!(
        stdout(),
        Show,
        LeaveAlternateScreen,
        DisableMouseCapture,
    )?;
    terminal::disable_raw_mode()?;
    Ok(()) 
}

#[derive(PartialEq, Eq)]
enum KeyState {
    Global,
    ToggleSection,
    FocusSection,
}

pub async fn run_torrents(torrent_uris: &[String]) -> Result<()> {
    let client_id = PeerId::random();
    println!("Client id: {client_id}");

    let mut torrent_tasks = Vec::new();
    for arg in torrent_uris {
        let uri = arg.clone();

        let mut torrent = if uri.starts_with("magnet:?") {
            Torrent::from_magnet(&uri, client_id).await.unwrap()
        } else {
            Torrent::from_torrent_file(&PathBuf::from(uri), client_id).await.unwrap()           
        };

        torrent_tasks.push(TorrentTask {
            rx: torrent.progress_rx.clone(),
            tx: torrent.command_tx.clone(),
            task: tokio::task::Builder::new()
                .name("torrent")
                .spawn( async move {
                    torrent.run().await
                }).unwrap()
        });
    }

    let mut interval = tokio::time::interval(Duration::from_millis(75));
    let mut table = Table::<Progress>::new(2);
    let mut state = TableState::new::<Progress>(true);
    let mut vertical_position: usize = 0;
    let mut key_state = KeyState::Global;

    prepare_terminal()?;
    let mut out = stdout();
    'main: loop {
        interval.tick().await;
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) => {
                    if key_state == KeyState::ToggleSection && let KeyCode::Char(key) = key.code {
                        state.handle_event(TableEvent::ToggleSection(key));
                        key_state = KeyState::Global;
                    } else if key_state == KeyState::FocusSection && let KeyCode::Char(key) = key.code {
                        state.handle_event(TableEvent::FocusSection(key));
                        key_state = KeyState::Global;
                    } else {
                        key_state = KeyState::Global;
                        match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                break 'main,
                            KeyCode::Char('q') =>
                                break 'main,
                            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                vertical_position += 3,
                            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                vertical_position = vertical_position.saturating_sub(3),

                            KeyCode::Char('t') =>
                                key_state = KeyState::ToggleSection,
                            KeyCode::Char('f') =>
                                key_state = KeyState::FocusSection,

                            KeyCode::Esc =>
                                state.handle_event(TableEvent::UnfocusSection),
                            KeyCode::Char('m')=>
                                state.handle_event(TableEvent::Mark),
                            KeyCode::Char('u')=>
                                state.handle_event(TableEvent::Unmark),
                            KeyCode::Char('+') =>
                                state.handle_event(TableEvent::ToggleTotal),
                            KeyCode::Char('g') | KeyCode::KeypadBegin =>
                                state.handle_event(TableEvent::First),
                            KeyCode::Char('G') | KeyCode::End =>
                                state.handle_event(TableEvent::Last),
                            KeyCode::Char('j') | KeyCode::Down =>
                                state.handle_event(TableEvent::Down),
                            KeyCode::Char('k') | KeyCode::Up =>
                                state.handle_event(TableEvent::Up),
                            KeyCode::Tab =>
                                state.handle_event(TableEvent::ToggleSub),
                            _ => (),
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    match mouse.kind {
                        event::MouseEventKind::ScrollDown => vertical_position += 3,
                        event::MouseEventKind::ScrollUp =>
                            vertical_position = vertical_position.saturating_sub(3),
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

        let mut frame = table.render(rows.iter().map(|r| &**r), &mut state, width.into())
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

        execute!(
            out,
            MoveTo(0, 0),
        )?;
        write!(out, "{frame}")?;
        out.flush()?;
    }
    
    restore_terminal()?;
    for torrent_task in torrent_tasks {
        let _ = torrent_task.tx.send(Command::Stop).await;
        torrent_task.task.await??;
    }
    Ok(())
}
