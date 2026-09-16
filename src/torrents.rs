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

impl Row for FileProgress {
    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column {
                header: "Name",
                ..Column::DEFAULT
            },
            Column {
                header: "Size",
                alignment: Right,
                ..Column::DEFAULT
            },
        ]
    }

    fn display_column(&self, index: usize, width: Option<usize>) -> String {
        match index {
            0 => self.relative_path.to_string(),
            1 => pretty_size(self.size),
            _ => unreachable!(),
        } 
    }

    fn display_sub(&self, width: usize) -> String {
        String::new()
    }
}

impl Row for PieceProgress {
    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column {
                header: "Idx",
                ..Column::DEFAULT
            },
            Column {
                header: "Blocks",
                flex: Some(1),
                ..Column::DEFAULT
            },
            Column {
                header: "%",
                alignment: Right,
                ..Column::DEFAULT
            },
            Column {
                header: "Num",
                alignment: Right,
                ..Column::DEFAULT
            },
            Column {
                header: "Tot",
                alignment: Right,
                ..Column::DEFAULT
            },
        ]
    }

    fn display_column(&self, index: usize, width: Option<usize>) -> String {
        match index {
            0 => self.index.to_string(),
            1 => format!("{:WIDTH$}", self.block_bitfield, WIDTH = width.unwrap()),
            2 => format!("{:.2}", self.num_obtained_blocks as f64 * 100. / self.num_blocks as f64),
            3 => self.num_obtained_blocks.to_string(),
            4 => self.num_blocks.to_string(),
            _ => unreachable!(),
        } 
    }

    fn display_sub(&self, width: usize) -> String {
        String::new()
    }
}

impl Row for Progress {
    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column::DEFAULT,
            Column {
                header: "Con",
                alignment: Right,
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
                ..Column::DEFAULT
            },
            Column {
                header: "Size",
                alignment: Right,
                ..Column::DEFAULT
            },
            Column {
                header: "Pieces",
                flex: Some(1),
                ..Column::DEFAULT
            },
            Column {
                header: "%",
                alignment: Right,
                ..Column::DEFAULT
            },
            Column {
                header: "ETA",
                alignment: Right,
                ..Column::DEFAULT
            },
        ]
    }

    fn display_column(&self, index: usize, width: Option<usize>) -> String {
        let transfer = self.transfer.as_ref();
        match index {
            0 => String::from("▶"),
            1 => self.num_connected_peers.to_string(),
            2 => transfer.map_or(String::new(), |t| pretty_size(t.down_speed)),
            3 => transfer.map_or(String::new(), |t| pretty_size(t.up_speed)),
            4 => {
                if let Some(width) = width {
                    self.display_name.chars().take(width - 1).chain(['…']).collect()
                } else {
                    self.display_name.to_string()
                }
            },
            5 => transfer.map_or(String::new(), |t| pretty_size(t.size)),
            6 => transfer.map_or(
                format!("{:WIDTH$}", self.metadata_bitfield, WIDTH = width.unwrap()),
                |t| format!("{:WIDTH$}", t.piece_bitfield, WIDTH = width.unwrap())
            ),
            7 => format!("{:.2}", transfer.map_or(0., |t| t.percentage())),
            8 => transfer.map_or(
                String::new(),
                |t| t.eta().map_or(String::new(), |e| pretty_duration(e))
            ),
            _ => unreachable!(),
        }
    }

    fn display_sub(&self, width: usize) -> String {
        let mut sub = String::new();
        if let Some(transfer) = &self.transfer {
            let mut active_table = Table::new(1, false, false);
            sub.extend(active_table.render(&transfer.active_pieces, 40).chars());
            let mut files_table = Table::new(1, false, false);
            sub.extend(files_table.render(&transfer.files, 50).chars());
        }
        sub
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

    let mut interval = tokio::time::interval(Duration::from_millis(100));
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

        let mut frame = table.render(&rows, width.into())
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
