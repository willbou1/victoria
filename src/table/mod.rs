pub mod state;

use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};

use state::*;

fn visible_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut width = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() {
                let b = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&b) {
                    break;
                }
            }
        } else {
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

#[derive(Debug, Clone, Copy)]
pub enum Sort {
    None,
    Increasing,
    Decreasing,
}

impl Default for Sort {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Copy)]
enum Junction {
    Top,
    Center,
    Bottom,
    SubTop,
    SubBottom,
    SubTotal,
    SubLast,
}

pub struct Column<R: Row> {
    pub header: &'static str,
    pub alignment: Alignment,
    pub max_width: Option<usize>,
    pub flex: Option<usize>,
    pub total: Option<fn(&[&R]) -> String>,
    pub value: fn(&R, width: Option<usize>) -> String,
}

pub struct TextSubSection<R: Row> {
    pub header: &'static str,
    pub key: char,
    pub content: fn(&R, width: usize) -> String,
}

pub struct TableSubSection<R: Row, S: Row + 'static> {
    pub header: &'static str,
    pub key: char,
    pub content: fn(&R, width: usize) -> Option<(Table<S>, Vec<&S>)>,
}

pub trait SubSection<R: Row> {
    fn header(&self) -> &'static str;
    fn key(&self) -> char;
    fn content(&self, row: &R, state: Option<&mut TableState>, width: usize) -> String;
    fn new_state(&self) -> Option<TableState>;
}

impl<R: Row> SubSection<R> for TextSubSection<R> {
    fn header(&self) -> &'static str {
        self.header
    }

    fn key(&self) -> char {
        self.key
    }

    fn content(&self, row: &R, _state: Option<&mut TableState>, width: usize) -> String {
        (self.content)(row, width)
    }

    fn new_state(&self) -> Option<TableState> {
        None
    }
}

impl<R: Row, S: Row> SubSection<R> for TableSubSection<R, S> {
    fn header(&self) -> &'static str {
        self.header
    }

    fn key(&self) -> char {
        self.key
    }

    fn content(&self, row: &R, state: Option<&mut TableState>, width: usize) -> String {
        (self.content)(row, width).map(
            |(mut table, rows)| table.render(rows, state.unwrap(), width).to_string()
        ).unwrap_or_default()
    }

    fn new_state(&self) -> Option<TableState> {
        Some(TableState::new::<S>(false))
    }
}

impl<R: Row> Column<R> {
    pub const DEFAULT: Self = Self {
        header: "",
        alignment: Alignment::Left,
        max_width: None,
        flex: None,
        total: None,
        value: |_, _| String::new(),
    };
}

pub type RowHash = u64;
pub trait Row {
    fn id(&self) -> RowHash;
    fn columns() -> &'static [Column<Self>] where Self: Sized;
    fn sub_sections() -> &'static [&'static dyn SubSection<Self>]
    where Self: Sized,
    {
        &[]
    }
}

pub struct Table<R: Row + 'static>  {
    columns: &'static [Column<R>],
    padding: usize,

    focused: bool,
    flex_total: usize,
    render: String,
    widths: Vec<usize>,
}

impl<R: Row> Table<R> {
    pub fn new(padding: usize) -> Self {
        Self {
            focused: true,
            columns: R::columns(),
            padding,
            render: String::new(),
            widths: Vec::new(),
            flex_total: R::columns().iter()
                .map(|c| c.flex.unwrap_or(0))
                .sum(),
        }
    }

    fn render_color(&mut self, color: Color) {
        self.render.push_str(&SetForegroundColor(color).to_string());
    }

    fn reset_color(&mut self) {
        self.render.push_str(&ResetColor.to_string());
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
        self.render_color(color);
        self.render.push_str(&aligned_cell);
        self.reset_color();
    }

    fn render_sub_sections(&mut self, state: &mut RowState) -> usize {
        let start = self.render.len();
        self.render_color(
            if self.focused {Color::Green} else {Color::White}
        );
        self.render.push('┤');
        self.reset_color();
        self.render.push(' ');
        for (s, section) in R::sub_sections().iter().enumerate() {
            self.render_color(
                if state.show_sections[s] {Color::Green} else {Color::White}
            );
            self.render.push('[');
            self.render.push(section.key());
            self.render.push(']');
            self.render.push(' ');
            self.render.push_str(section.header());
            self.render.push(' ');
            self.reset_color();
        }
        self.render_color(
            if self.focused {Color::Green} else {Color::White}
        );
        self.render.push('├');
        self.reset_color();
        visible_width(&self.render[start..self.render.len()])
    }

    fn junction_chars(junction: Junction) -> (char, char, char) {
        use Junction::*;
        match junction {
            Top => ('╭', '┬', '╮'),
            Center => ('├', '┼', '┤'),
            Bottom => ('╰', '┴', '╯'),
            SubTop => ('├', '┴', '┤'),
            SubBottom => ('├', '┬', '┤'),
            SubTotal => ('├', '┬', '┤'),
            SubLast => ('╰', '─', '╯'),
        }
    }

    fn render_h_sparator(&mut self, junction: Junction, state: Option<&mut RowState>) {
        let (left, junction_char, right) = Self::junction_chars(junction);
        self.render_color(
            if self.focused {Color::Green} else {Color::White}
        );
        self.render.push(left);
        let mut sections_len = self.padding;
        self.render_n('─', self.padding);
        self.reset_color();
        if let Junction::SubTop = junction {
            sections_len += self.render_sub_sections(state.unwrap());
        }
        self.render_color(
            if self.focused {Color::Green} else {Color::White}
        );
        let mut end = 0;
        for (c, &width) in self.widths.clone().iter().enumerate() {
            end += self.padding * 2 + width;
            if end > sections_len {
                self.render_n('─', (end - sections_len).min(self.padding * 2 + width));
            }
            end += 1;
            if c + 1 < self.widths.len() && end > sections_len {
                self.render.push(junction_char);
            }
        }
        self.render.push(right);
        self.reset_color();
        self.render_newline();
    }

    fn render_v_separator(&mut self, marked: bool) {
        self.render_color(
            if marked && self.focused {Color::Magenta}
            else if self.focused {Color::Green}
            else {Color::White}
        );
        self.render.push('│');
        self.reset_color();
    }
    
    fn render_sub(&mut self, width: usize, row: &R, state: &mut RowState) {
        for (i, opened) in state.show_sections.iter().enumerate() {
            if !*opened {
                continue;
            }

            let sub = R::sub_sections()[i].content(
                row,
                state.sub_table_states[i].as_mut(),
                width - 2 * (self.padding + 1),
            );

            for line in sub.lines() {
                self.render_v_separator(state.marked);
                self.render_padding();
                self.render.push_str(line);
                self.render_n(
                    ' ',
                    width - visible_width(line) - self.padding - 2,
                );
                self.render_v_separator(state.marked);
                self.render_newline();
            }
        }
    }

    fn render_cells(&mut self, cells: &[String], color: Color, marked: bool) {
        self.render_v_separator(marked);
        for c in 0..self.columns.len() {
            self.render_padding();
            self.render_cell(c, &cells[c], color);
            self.render_padding();
            self.render_v_separator(marked);
        }
        self.render_newline();
    }

    fn chop_string(&self, width: usize, cell: &str) -> String {
        if cell.chars().count() > width {
            cell.chars().take(width - 1).chain(['…']).collect()
        } else {
            cell.to_string()
        }
    }

    fn show_total(state: &TableState) -> bool {
        state.show_total && R::columns().iter().filter(|c| c.total.is_some()).count() > 0
    }

    pub fn render<'a, I>(&mut self, rows: I, state: &mut TableState, width: usize) -> &str
    where
        I: IntoIterator<Item = &'a R>,
    {
        let rows: Vec<_> = rows.into_iter().collect();
        state.reconcile(rows.iter().map(|r| r.id()).collect());
        self.render.clear();
        self.focused = state.prev_focused.map_or(true, |pf| pf) && state.focused_section.is_none();
        
        let mut filled_columns: Vec<Vec<_>> = Vec::new();
        for col in self.columns {
            let mut filled_column = vec![col.header.to_string()];
            filled_column.extend(
                rows.iter().map(|r| {
                    if col.flex.is_none() {
                        self.chop_string(col.max_width.unwrap_or(usize::MAX), &(col.value)(r, None))
                    } else {
                        String::new()
                    }
                })
            );
            if Self::show_total(state) {
                filled_column.push(
                    col.total.map(|t| t(&rows)).unwrap_or(String::new())
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
                            *cell = self.chop_string(
                                self.widths[c],
                                &(col.value)(row, Some(self.widths[c]))
                            );
                        });
                }
                remaining -= column_width;
                remaining_flex -= flex;
            }
        }

        self.render_h_sparator(Junction::Top, None);
        self.render_cells(
            &self.columns.iter().map(|c| c.header.to_string()).collect::<Vec<_>>(),
            Color::Red,
            false,
        );
        self.render_h_sparator(Junction::Center, None);

        for (r, row) in rows.iter().enumerate() {
            self.render_cells(
                &filled_columns.iter().map(|c| c[r + 1].clone()).collect::<Vec<_>>(),
                if let Some(hash) = state.selected_row && hash == row.id() && self.focused {
                    Color::Green
                } else {
                    Color::White
                },
                state.row_states.get(&row.id()).map_or(false, |r| r.marked)
            );
            if state.row_states.get(&row.id()).map_or(false, |r| r.show_sub) {
                self.render_h_sparator(Junction::SubTop, state.row_states.get_mut(&row.id()));
                self.render_sub(
                    width,
                    row,
                    state.row_states.get_mut(&row.id()).unwrap()
                );
                if r != rows.len() - 1 {
                    self.render_h_sparator(Junction::SubBottom, None);
                }
            }
        }

        let bottom_sub = rows.last().map_or(
            false,
            |row| state.row_states.get(&row.id()).map_or(false, |r| r.show_sub)
        );

        if Self::show_total(state) {
            if bottom_sub {
                    self.render_h_sparator(Junction::SubTotal, None);
            } else {
                self.render_h_sparator(Junction::Center, None);
            }
            self.render_cells(
                &filled_columns.iter().map(|c| c[rows.len() + 1].clone()).collect::<Vec<_>>(),
                Color::Yellow,
                false,
            );
            self.render_h_sparator(Junction::Bottom, None);
        } else if bottom_sub {
            self.render_h_sparator(Junction::SubLast, None);
        } else {
            self.render_h_sparator(Junction::Bottom, None);
        }
        &self.render
    }
}
