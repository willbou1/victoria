use anyhow::Result;
use std::{
    path::PathBuf,
    time::Duration,
    io::{stdout, Write},
};
use tokio::{
    signal,
    sync::{mpsc, watch},
    task::JoinHandle,
};
use crossterm::{
    execute,
    cursor::{MoveTo, Show, Hide},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, self},
    event::{Event, KeyCode, KeyModifiers, self},
};

use libvictoria::{
    torrent::{Torrent, control::*},
    util::*,
    types::*,
};
use super::table::*;

impl Row for FileProgress {
    fn columns() -> &'static [Column<Self>] {
        &[
            Column {
                header: "Name",
                alignment: Alignment::Left,
                max_width: None,
                flex: None,
                total: None,
            },
            Column {
                header: "Size",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: None,
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
        &[
            Column {
                header: "Idx",
                alignment: Alignment::Left,
                max_width: None,
                flex: None,
                total: None,
            },
            Column {
                header: "Blocks",
                alignment: Alignment::Left,
                max_width: None,
                flex: Some(1),
                total: None,
            },
            Column {
                header: "%",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: None,
            },
            Column {
                header: "Num",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: None,
            },
            Column {
                header: "Tot",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: None,
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
        &[
            Column {
                header: "",
                alignment: Alignment::Left,
                max_width: None,
                flex: None,
                total: None,
            },
            Column {
                header: "Con",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: Some(|rows| {
                    let total: usize = rows.iter()
                        .map(|r| r.num_connected_peers).sum();
                    format!("{total}")
                }),
            },
            Column {
                header: "Dis",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: Some(|rows| {
                    let total: usize = rows.iter()
                        .map(|r| r.num_discovery_attempts).sum();
                    format!("{total}")
                }),
            },
            Column {
                header: "Down",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: Some(|rows| {
                    let total: usize = rows.iter()
                        .map(|r| r.transfer.as_ref().map_or(0, |t| t.down_speed)).sum();
                    format!("{}", pretty_size(total))
                }),
            },
            Column {
                header: "Up",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: Some(|rows| {
                    let total: usize = rows.iter()
                        .map(|r| r.transfer.as_ref().map_or(0, |t| t.up_speed)).sum();
                    format!("{}", pretty_size(total))
                }),
            },
            Column {
                header: "Name",
                alignment: Alignment::Left,
                max_width: None,
                flex: Some(3),
                total: None,
            },
            Column {
                header: "Size",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: None,
            },
            Column {
                header: "Pieces",
                alignment: Alignment::Left,
                max_width: None,
                flex: Some(1),
                total: None,
            },
            Column {
                header: "%",
                alignment: Alignment::Right,
                max_width: None,
                flex: None,
                total: None,
            },
        ]
    }

    fn display_column(&self, index: usize, width: Option<usize>) -> String {
        match index {
            0 => {
                if let Some(bitfield) = &self.metadata_bitfield
                    && bitfield.len() != 0
                    && bitfield.len() == bitfield.num_set()
                {
                    "⇆".into()
                } else {
                    "ℹ".into()
                }
            }
            1 => self.num_connected_peers.to_string(),
            2 => self.num_discovery_attempts.to_string(),
            3 => self.transfer
                .as_ref()
                .map(|t| pretty_size(t.down_speed))
                .unwrap_or_default().to_string(),
            4 => self.transfer
                .as_ref()
                .map(|t| pretty_size(t.up_speed))
                .unwrap_or_default().to_string(),
            5 => {
                if let Some(width) = width {
                    self.display_name.chars().take(width - 1).chain(['…']).collect()
                } else {
                    self.display_name.to_string()
                }
            },
            6 => self.transfer
                .as_ref()
                .map(|t| pretty_size(t.size))
                .unwrap_or_default().to_string(),
            7 => self.transfer.as_ref().map(
                |t| format!("{:WIDTH$}", t.piece_bitfield, WIDTH = width.unwrap())
            ).unwrap_or(String::new()),
            8 => format!(
                "{:.2}",
                self.transfer
                    .as_ref()
                    .map(|t| t.downloaded as f64 * 100. / t.size as f64)
                    .unwrap_or(0.)
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
    let mut out = stdout();
    execute!(
        out,
        EnterAlternateScreen,
        Hide,
    )?;
    terminal::enable_raw_mode()?;
    'main: loop {
        interval.tick().await;
        while event::poll(Duration::ZERO)? {
            let Event::Key(key) = event::read()? else {
                continue;
            };

            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break 'main,
                KeyCode::Char('q') => break 'main,
                KeyCode::Up => table.up(),
                KeyCode::Down => table.down(),
                KeyCode::Tab => table.toggle(),
                _ => (),
            }
        }

        let rows: Vec<Progress> = torrent_tasks.iter()
            .map(|tt| tt.rx.borrow().clone())
            .collect();
        let (width, height) = terminal::size()?;
        let mut frame = table.render(&rows, width.into()).to_string();
        for _ in 0..(height as usize - frame.lines().count() - 1) {
            frame.extend(std::iter::repeat_n(' ', width.into()));
            frame.push('\r');
            frame.push('\n');
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

    execute!(
        out,
        Show,
        LeaveAlternateScreen,
    )?;
    terminal::disable_raw_mode()?;
    Ok(())
}
