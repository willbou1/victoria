use crossterm::style::{Color, ResetColor, SetForegroundColor, SetBackgroundColor};

fn visible_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut width = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;

            // Skip CSI parameters until the final byte.
            while i < bytes.len() {
                let b = bytes[i];
                i += 1;

                if (0x40..=0x7e).contains(&b) {
                    break;
                }
            }
        } else {
            // Decode one UTF-8 character.
            let ch = s[i..].chars().next().unwrap();
            width += 1;
            i += ch.len_utf8();
        }
    }

    width
}

pub enum Alignment {
    Left,
    Right,
}

enum Junction {
    Top,
    Center,
    Bottom,
    SubTop,
    SubBottom,
    SubLast,
}

pub struct Column<R: Row> {
    pub header: &'static str,
    pub alignment: Alignment,
    pub max_width: Option<usize>,
    pub flex: Option<usize>,
    pub total: Option<fn(&[R]) -> String>,
}

pub trait Row {
    fn columns() -> &'static [Column<Self>] where Self: Sized;
    fn display_column(&self, index: usize, width: Option<usize>) -> String;
    fn display_sub(&self, width: usize) -> String;
}

pub struct Table<R: Row + 'static> {
    columns: &'static [Column<R>],
    show_total: bool,
    padding: usize,

    focused: bool,
    flex_total: usize,
    render: String,
    widths: Vec<usize>,
    selected: usize,
    show_subs: Vec<bool>,
}

impl<R: Row> Table<R> {
    pub fn new(padding: usize, show_total: bool, focused: bool) -> Self {
        Self {
            focused,
            selected: 0,
            show_total,
            columns: R::columns(),
            padding,
            render: String::new(),
            widths: Vec::new(),
            flex_total: R::columns().iter()
                .map(|c| c.flex.unwrap_or(0))
                .sum(),
            show_subs: Vec::new(),
        }
    }

    pub fn up(&mut self) {
        let num_rows = self.show_subs.len();
        self.selected = (self.selected + num_rows - 1) % num_rows;
    }

    pub fn down(&mut self) {
        self.selected = (self.selected + 1) % self.show_subs.len();
    }

    pub fn toggle(&mut self) {
        if self.show_subs.len() > 0 {
            self.show_subs[self.selected] = 
                !self.show_subs[self.selected];
        }
    }

    fn render_newline(&mut self) {
        self.render.push('\r');
        self.render.push('\n');
    } 

    fn render_n(&mut self, c: char, width: usize) {
        self.render.extend(std::iter::repeat_n(c, width));
    }

    fn render_padding(&mut self) {
        self.render_n(' ', self.padding);
    }

    fn render_cell(&mut self, c: usize, cell: &str, color: Color) {
        use Alignment::*;
        let width = self.widths[c];
        let aligned_cell = match self.columns[c].alignment {
            Left => format!("{cell:<width$}"),
            Right => format!("{cell:>width$}"),
        };
        self.render.push_str(&SetForegroundColor(color).to_string());
        self.render.push_str(&aligned_cell);
        self.render.push_str(&ResetColor.to_string());
    }

    fn junction_chars(junction: Junction) -> (char, char, char) {
        use Junction::*;
        match junction {
            Top => ('╭', '┬', '╮'),
            Center => ('├', '┼', '┤'),
            Bottom => ('╰', '┴', '╯'),
            SubTop => ('├', '┴', '┤'),
            SubBottom => ('├', '┬', '┤'),
            SubLast => ('├', '┬', '┤'),
        }
    }

    fn render_sparator(&mut self, junction: Junction) {
        let (left, junction, right) = Self::junction_chars(junction);
        self.render.push(left);
        for (c, &width) in self.widths.clone().iter().enumerate() {
            self.render_n('─', self.padding * 2 + width);
            if c + 1 < self.widths.len() {
                self.render.push(junction);
            }
        }
        self.render.push(right);
        self.render_newline();
    }

    fn render_sub(&mut self, width: usize, sub: &str) {
        for line in sub.lines() {
            self.render.push('│');
            self.render_padding();
            self.render.extend(line.chars());
            self.render_n(' ', width - visible_width(line) - self.padding - 2);
            self.render.push('│');
            self.render_newline();
        }
    }

    fn render_cells(&mut self, cells: &[String], color: Color) {
        self.render.push('│');
        for (c, col) in self.columns.iter().enumerate() {
            self.render_padding();
            self.render_cell(c, &cells[c], color);
            self.render_padding();
            self.render.push('│');
        }
        self.render_newline();
    }

    fn chop_string(&self, c: usize, cell: &str) -> String {
        if let Some(max_width) = self.columns[c].max_width {
            cell.chars().take(max_width).chain(['…']).collect()
        } else {
            cell.to_string()
        }
    }

    pub fn render(&mut self, rows: &[R], width: usize) -> &str {
        self.render.clear();
        self.show_subs.resize(rows.len(), false);
        if self.selected > rows.len() - 1 {
            self.selected = rows.len() - 1;
        }
        
        let mut filled_columns: Vec<Vec<_>> = Vec::new();
        for (c, col) in self.columns.iter().enumerate() {
            let mut filled_column = vec![col.header.to_string()];
            filled_column.extend(
                rows.iter().map(|r| {
                    if col.flex.is_none() {
                        self.chop_string(c, &r.display_column(c, None))
                    } else {
                        String::new()
                    }
                })
            );
            if self.show_total {
                filled_column.push(
                    col.total.map(|t| t(rows)).unwrap_or(String::new())
                );
            }
            filled_columns.push(filled_column);
        }

        let natural_widths: Vec<usize> = filled_columns
            .iter().map(|fc| {
                fc.iter().map(|cell| cell.chars().count()).max().unwrap_or(0)
            })
            .collect();

        let taken_width: usize = natural_widths.iter()
            .zip(self.columns.iter())
            .filter(|(_, col)| col.flex.is_none())
            .map(|(width, _)| *width)
            .sum();

        let separator_width = (2 * self.padding + 1) * self.columns.len() + 1;

        self.widths = natural_widths;

        let mut remaining = width.saturating_sub(taken_width + separator_width);
        let mut remaining_flex = self.flex_total;

        for (c, col) in self.columns.iter().enumerate() {
            if let Some(flex) = col.flex {
                let column_width =
                    ((remaining as u128 * flex as u128) / remaining_flex as u128) as usize;

                self.widths[c] = column_width;
                if col.flex.is_some() {
                    filled_columns[c][1..]
                        .iter_mut()
                        .zip(rows.iter())
                        .for_each(|(cell, row)| {
                            *cell = row.display_column(c, Some(self.widths[c]));
                        });
                }
                remaining -= column_width;
                remaining_flex -= flex;
            }
        }

        self.render_sparator(Junction::Top);
        self.render_cells(
            &self.columns.iter().map(|c| c.header.to_string()).collect::<Vec<_>>(),
            Color::Red,
        );
        self.render_sparator(Junction::Center);

        for (r, row) in rows.iter().enumerate() {
            self.render_cells(
                &filled_columns.iter().map(|c| c[r + 1].clone()).collect::<Vec<_>>(),
                if self.selected == r && self.focused {
                    Color::Green
                } else {
                    Color::White
                },
            );
            if self.show_subs[r] {
                self.render_sparator(Junction::SubTop);
                self.render_sub(width, &row.display_sub(width));
                if r != rows.len() - 1 {
                    self.render_sparator(Junction::SubBottom);
                }
            }
        }

        if self.show_total {
            if self.show_subs[rows.len() - 1] {
                self.render_sparator(Junction::SubLast);
            } else {
                self.render_sparator(Junction::Center);
            }
            self.render_cells(
                &filled_columns.iter().map(|c| c[rows.len() + 1].clone()).collect::<Vec<_>>(),
                Color::Yellow,
            );
        }
        self.render_sparator(Junction::Bottom);
        &self.render
    }
}
