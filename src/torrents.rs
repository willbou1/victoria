use anyhow::Result;
use std::{
    path::PathBuf,
    time::Duration,
    io::{stdout, Write},
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
use super::table::*;

impl Row for (String, TrackerInfo) {
    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column {
                value: |(_ , tracker), _| tracker.succeeded.map_or(
                    String::new(),
                    |s| if s {String::from("✓")} else {String::from("✗")}
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "Url",
                value: |(url, _), _| url.to_string(),
                ..Column::DEFAULT
            },
            Column {
                header: "T",
                alignment: Right,
                value: |(_, tracker), _| tracker.tier.to_string(),
                ..Column::DEFAULT
            },
            Column {
                header: "Seed",
                alignment: Right,
                value: |(_, tracker), _| tracker.seeders.map_or(String::new(), |s| s.to_string()),
                ..Column::DEFAULT
            },
            Column {
                header: "Leech",
                alignment: Right,
                value: |(_, tracker), _| tracker.leechers.map_or(String::new(), |s| s.to_string()),
                ..Column::DEFAULT
            },
            Column {
                header: "Int",
                alignment: Right,
                value: |(_, tracker), _| tracker.interval.map_or(
                    String::new(),
                    |i| pretty_duration(i)
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "MinInt",
                alignment: Right,
                value: |(_, tracker), _| tracker.min_interval.map_or(
                    String::new(),
                    |m| pretty_duration(m)
                ),
                ..Column::DEFAULT
            },
        ]
    }
}

impl Row for (PeerId, PeerProgress) {
    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column::DEFAULT,
            Column {
                header: "Id",
                value: |(id, _), _| {
                    let string_id = id.to_string();
                    format!("{}..{}", &string_id[..8], &string_id[32..40])
                },
                ..Column::DEFAULT
            },
            Column {
                header: "F P M D",
                value: |(_, peer), _| [
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
                value: |(_, peer), _|
                    peer.client.clone().unwrap_or(String::new()),
                ..Column::DEFAULT
            },
            Column {
                header: "Pieces",
                flex: Some(1),
                value: |(_, peer), width| peer.connection.as_ref().map_or(
                    String::new(),
                    |c| format!("{:WIDTH$}", c.piece_bitfield, WIDTH = width.unwrap())
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "I C",
                value: |(_, peer), _| peer.connection.as_ref().map_or(
                    String::new(),
                    |c| [c.am_interested, c.peer_choking]
                        .map(|ext| if ext {"■"} else {"□"}).join(" ")
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "PC",
                alignment: Right,
                value: |(_, peer), _| peer.connection.as_ref().map_or(
                    String::new(),
                    |c| c.piece_cursor.map_or(String::new(), |pc| pc.to_string())
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "PL",
                alignment: Right,
                value: |(_, peer), _| peer.connection.as_ref().map_or(
                    String::new(),
                    |c| c.pipeline.to_string()
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "t/s",
                alignment: Right,
                value: |(_, peer), _| peer.connection.as_ref().map_or(
                    String::new(),
                    |c| c.timeout_rate.to_string()
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "r/s",
                alignment: Right,
                value: |(_, peer), _| peer.connection.as_ref().map_or(
                    String::new(),
                    |c| c.reject_rate.to_string()
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "Down",
                alignment: Right,
                value: |(_, peer), _| peer.connection.as_ref().map_or(
                    String::new(),
                    |c| pretty_size(c.down_speed)
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "I C",
                value: |(_, peer), _| peer.connection.as_ref().map_or(
                    String::new(),
                    |c| [c.peer_interested, c.am_choking]
                        .map(|ext| if ext {"■"} else {"□"}).join(" ")
                ),
                ..Column::DEFAULT
            },
            Column {
                header: "Up",
                alignment: Right,
                value: |(id, peer), _| peer.connection.as_ref().map_or(
                    String::new(),
                    |c| pretty_size(c.up_speed)
                ),
                ..Column::DEFAULT
            },
        ]
    }
}

impl Row for FileInfo {
    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column {
                header: "Relative path",
                value: |file, _| file.relative_path.to_string(),
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
                    .map_or(String::new(), |t| pretty_size(t.down_speed)),
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
                    .map_or(String::new(), |t| pretty_size(t.up_speed)),
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
                    .map_or(String::new(), |t| pretty_size(t.size)),
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
                    prog.transfer.as_ref().map_or(
                        String::new(),
                        |t|  format!("{:.2}", t.percentage())),
                ..Column::DEFAULT
            },
            Column {
                header: "ETA",
                alignment: Right,
                value: |prog, _| prog.transfer.as_ref().map_or(
                    String::new(),
                    |t| t.eta().map_or(String::new(), |e| pretty_duration(e))
                ),
                ..Column::DEFAULT
            },
        ]
    }

    fn sub_sections() -> &'static [SubSection<Self>] {
        &[
            SubSection {
                header: "Files",
                content: |prog, width| {
                    prog.transfer.as_ref().map_or(String::new(), |transfer| {
                        let mut table = Table::new(1, false, false);
                        table.render(transfer.files.clone(), width).to_string()
                    })
                },
            },
            SubSection {
                header: "Blocks",
                content: |prog, width| {
                    prog.transfer.as_ref().map_or(String::new(), |transfer| {
                        let mut table = Table::new(1, false, false);
                        table.render(transfer.active_pieces.clone(), width).to_string()
                    })
                },
            },
            SubSection {
                header: "Peers",
                content: |prog, width| {
                    let mut table = Table::new(1, false, false);
                    table.render(prog.peers.clone(), width).to_string()
                },
            },
            SubSection {
                header: "Trackers",
                content: |prog, width| {
                    let mut table = Table::new(1, false, false);
                    table.render(prog.trackers.clone(), width).to_string()
                },
            },
        ]
    }
}

struct TorrentTask {
    task: JoinHandle<()>,
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
                    torrent.run().await.unwrap();
                }).unwrap()
        });
    }

    let mut interval = tokio::time::interval(Duration::from_millis(75));
    let mut table = Table::<Progress>::new(2, true, true);
    let mut vertical_position: usize = 0;

    prepare_terminal()?;
    let mut out = stdout();
    'main: loop {
        interval.tick().await;
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) => {
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break 'main,
                        KeyCode::Char('q') => break 'main,
                        KeyCode::Up | KeyCode::Char('k') => table.up(),
                        KeyCode::Down | KeyCode::Char('j') => table.down(),
                        KeyCode::Tab | KeyCode::Char(' ') => table.toggle(),
                        _ => (),
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

        let rows: Vec<Progress> = torrent_tasks.iter()
            .map(|tt| tt.rx.borrow().clone())
            .collect();
        let (width, height) = terminal::size()?;

        let mut frame = table.render(rows, width.into())
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
    
    for torrent_task in torrent_tasks {
        torrent_task.tx.send(Command::Stop).await?;
        torrent_task.task.await?;
    }

    restore_terminal()
}
