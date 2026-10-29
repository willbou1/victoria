use std::{
    hash::{Hash, Hasher},
    collections::hash_map::DefaultHasher,
};
use crossterm::style::Stylize;

use super::util::*;
use super::libvictoria::{
    torrent::control::*
};
use whodatrow::*;

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum ProgressSection {
    Trackers,
    Files,
    Blocks,
    Peers,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Section {
    Progress(ProgressSection),
}

impl Row for TrackerInfo {
    type SectionKey = Section;

    fn id(&self) -> RowHash {
        let mut hasher = DefaultHasher::new();
        self.url.hash(&mut hasher);
        hasher.finish()
    }

    fn columns() -> &'static [Column<Self>] {
        use Alignment::*;
        &[
            Column {
                value: |tracker, _| tracker.succeeded.as_ref().map(
                    |s| match s {
                        TrackerStatus::Announcing => "↻",
                        TrackerStatus::Succeeded => "✓",
                        TrackerStatus::Failed => "✗",
                    }.to_string()
                ).unwrap_or_default(),
                ..Column::DEFAULT
            },
            Column {
                header: "T",
                alignment: Right,
                value: |tracker, _| tracker.tier.to_string(),
                ..Column::DEFAULT
            },
            Column {
                header: "Url",
                value: |tracker, _| tracker.url.clone(),
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
    type SectionKey = Section;

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
                header: "T P D",
                value: |peer, _| [
                    peer.discovery.trackers,
                    peer.discovery.pex,
                    peer.discovery.dht,
                ].map(|ext| if ext {"■"} else {"□"}).join(" "),
                ..Column::DEFAULT
            },
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
    type SectionKey = Section;

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
                header: "Priority",
                value: |file, _| {
                    use FilePriority::*;
                    match file.priority {
                        Low => "Low",
                        Normal => "Normal",
                        Medium => "Medium",
                        High => "High",
                    }.to_string()
                },
                ..Column::DEFAULT
            },
            Column {
                header: "Size",
                alignment: Right,
                value: |file, _| pretty_size(file.size),
                ..Column::DEFAULT
            },
            Column {
                header: "Pieces",
                flex: Some(1),
                value: |file, width|
                    format!("{:WIDTH$}", file.piece_bitfield, WIDTH = width.unwrap()),
                ..Column::DEFAULT
            },
            Column {
                header: "%",
                alignment: Right,
                value: |file, _|
                    pretty_percentage(file.piece_bitfield.percentage()),
                ..Column::DEFAULT
            },
        ]
    }
}

impl Row for PieceProgress {
    type SectionKey = Section;

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
                value: |piece, _| pretty_percentage(piece.block_bitfield.percentage()),
                ..Column::DEFAULT
            },
            Column {
                header: "Num",
                alignment: Right,
                value: |piece, _| piece.block_bitfield.num_set().to_string(),
                ..Column::DEFAULT
            },
            Column {
                header: "Tot",
                alignment: Right,
                value: |piece, _| piece.block_bitfield.len().to_string(),
                ..Column::DEFAULT
            },
            Column {
                header: "X",
                alignment: Right,
                value: |piece, _| {
                    let num = piece.num_verification_failures;
                    if num > 0 {num.to_string().red().to_string()} else {num.to_string()}
                },
                ..Column::DEFAULT
            },
        ]
    }
}


impl Row for Progress {
    type SectionKey = Section;
    
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
                        |t| pretty_percentage(t.percentage())
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
                section_key: Section::Progress(ProgressSection::Files),
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
                section_key: Section::Progress(ProgressSection::Blocks),
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
                section_key: Section::Progress(ProgressSection::Peers),
                header: "Peers",
                key: 'p',
                content: |prog: &Self, _| {
                    let table = Table::new(1);
                    Some((table, prog.peers.values().collect()))
                },
            },
            &TableSubSection {
                section_key: Section::Progress(ProgressSection::Trackers),
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
