use std::{collections::HashMap, marker::PhantomData};

use super::*;

pub enum TableEvent {
    Up, Down, First, Last,
    Mark, Unmark,
    ToggleTotal,
    ToggleSub,
    ToggleSection(char),
    FocusSection(char),
    UnfocusSection,
}

pub struct Focus<'a, K>
where
    K: Copy + 'static
{
    pub row: RowHash,
    pub section: Option<(K, &'a mut TableState<K>)>,
}

#[derive(Debug, Clone)]
pub struct TableState<K>
where
    K: Copy + 'static,
{
    pub(crate) prev_focused: Option<bool>,
    pub(crate) focused_section: Option<usize>,
    pub(crate) selected_row: Option<RowHash>,
    pub(crate) row_states: HashMap<RowHash, RowState<K>>,
    pub(crate) column_sorts: Vec<Sort>,
    pub(crate) show_total: bool,
    pub(crate) sub_keys: Vec<char>,
    pub(crate) sub_section_keys: Vec<Option<K>>,
    pub(crate) default_row_state: RowState<K>,
    pub(crate) ordered_row_hashes: Vec<RowHash>,
}

impl<K> TableState<K>
where
    K:  Copy + 'static,
{
    pub fn new<R>(is_root: bool) -> Self
    where
        R: Row<SectionKey = K> + 'static
    {
        Self {
            show_total: true,
            prev_focused: if is_root {None} else {Some(false)},
            focused_section: None,
            selected_row: None,
            row_states: HashMap::new(),
            column_sorts: std::iter::repeat_n(
                Sort::default(),
                R::columns().len()
            ).collect(),
            default_row_state: RowState::<K>::new::<R>(),
            ordered_row_hashes: Vec::new(),
            sub_keys: R::sub_sections().iter()
                .map(|s| s.key()).collect(),
            sub_section_keys: R::sub_sections().iter()
                .map(|s| s.section_key()).collect(),
        }
    }

    pub(crate) fn reconcile(&mut self, ordered_row_hashes: Vec<RowHash>) {
        if let Some(hash) = self.selected_row && !ordered_row_hashes.contains(&hash) {
            self.selected_row = ordered_row_hashes.first().map(|f| *f);
        } else if let None = self.selected_row {
            self.selected_row = ordered_row_hashes.first().map(|f| *f);
        }
        self.row_states.retain(|k, _| ordered_row_hashes.contains(k));
        for hash in &ordered_row_hashes {
            self.row_states.entry(*hash).or_insert(self.default_row_state.clone());
        }
        self.ordered_row_hashes = ordered_row_hashes;
    }

    pub fn follow_focus(&mut self) -> Option<Focus<'_, K>> {
        let row = self.selected_row?;
        let section = self.focused_section.and_then(|index| {
            Some((
                self.sub_section_keys[index]?,
                self.row_states.get_mut(&row)?
                    .sub_table_states[index]
                    .as_mut()?,
            ))
        });
        Some(Focus {
            row,
            section,
        })
    }

    pub fn handle_event(&mut self, event: TableEvent) {
        self.handle_event_internal(event, None);
    }

    fn handle_event_internal(
        &mut self,
        event: TableEvent,
        parent_focused_section: Option<&mut Option<usize>>
    ) {
        use TableEvent::*;
        // Dispatch to focused table
        if let Some(index) = self.focused_section {
            let state = self.selected_row.as_ref()
                .and_then(|hash| self.row_states.get_mut(hash))
                .and_then(|row| row.sub_table_states[index].as_mut());

            if let Some(state) = state {
                state.handle_event_internal(event, Some(&mut self.focused_section));
                return;
            }

            self.focused_section = None;
        }

        match event {
            Up => self.nav(-1),
            Down => self.nav(1),
            First => self.goto(0),
            Last => self.goto(self.ordered_row_hashes.len().saturating_add(1)),
            Mark => self.mark(),
            ToggleTotal => self.toggle_total(),
            Unmark => self.unmark(),
            ToggleSub => self.toggle_sub(),
            ToggleSection(key) => self.toggle_section(key),
            FocusSection(key) => self.focus_section(key),
            UnfocusSection => if let Some(parent_focused_section) = parent_focused_section {
                *parent_focused_section = None;
                self.prev_focused = Some(false);
            },
        }
    }

    fn selected_row_mut(&mut self) -> Option<&mut RowState<K>> {
        if let Some(hash) = &self.selected_row {
            return self.row_states.get_mut(hash);
        }
        None
    }

    fn for_selected_or_marked<F>(&mut self, f: F)
    where
        F: Fn(&mut RowState<K>)
    {
        let marked_rows: Vec<_> = self.row_states.iter_mut()
            .map(|(_, v)| v).filter(|v| v.marked)
            .collect();
        if marked_rows.is_empty() {
            if let Some(row) = self.selected_row_mut() {
                f(row);
            }
        } else {
            for row in marked_rows {
                f(row);
            }
        }
    }

    fn mark(&mut self) {
        if let Some(row) = self.selected_row_mut() {
            row.toggle_mark();
        }
    }

    fn unmark(&mut self) {
        for state in self.row_states.values_mut() {
            state.marked = false;
        }
    }

    fn focus_section(&mut self, key: char) {
        let Some(index) = self.sub_keys.iter().position(|&k| k == key) else {
            return;
        };
        if self.selected_row.is_some() {
            {
                let row = self.selected_row_mut().unwrap();
                row.show_section(index);

                if let Some(state) = &mut row.sub_table_states[index] {
                    state.prev_focused = Some(true);
                }
            }

            self.focused_section = Some(index);
        }
    }

    fn toggle_sub(&mut self) {
        self.for_selected_or_marked(|row| {
            row.toggle_sub();
        });
    }

    pub fn toggle_total(&mut self) {
        self.show_total = !self.show_total;
    }

    fn toggle_section(&mut self, key: char) {
        let Some(index) = self.sub_keys.iter().position(|&k| k == key) else {
            return;
        };
        self.for_selected_or_marked(|row| {
            row.toggle_section(index);
        });
    }

    fn goto(&mut self, index: usize) {
        let last = self.ordered_row_hashes.len().saturating_sub(1);
        let index = index.min(last);
        self.selected_row = self.ordered_row_hashes.get(index).copied();
    }

    fn nav(&mut self, movement: isize) {
        let Some(selected) = self.selected_row else {
            return;
        };

        let Some(index) = self.ordered_row_hashes.iter()
            .position(|hash| *hash == selected)
        else {
            return;
        };

        let index = (index as isize + movement).max(0);
        self.goto(index as usize);
    }
}

#[derive(Debug, Clone)]
pub struct RowState<K>
where
    K: Copy + 'static,
{
    pub(crate) marked: bool,
    pub(crate) show_sub: bool,
    pub(crate) show_sections: Vec<bool>,
    pub(crate) sub_table_states: Vec<Option<TableState<K>>>,
    _marker: PhantomData<K>,
}

impl<K> RowState<K>
where
    K: Copy + 'static,
{
    fn new<R>() -> Self
    where
        R: Row<SectionKey = K> + 'static
    {
        Self {
            marked: false,
            show_sub: false,
            show_sections: std::iter::repeat_n(
                false,
                R::sub_sections().len(),
            ).collect(),
            sub_table_states: R::sub_sections().iter()
                .map(|s| s.new_state()).collect(),

            _marker: PhantomData,
        }
    }

    fn toggle_mark(&mut self) {
        self.marked = !self.marked;
    }

    fn toggle_sub(&mut self) {
        self.show_sub = !self.show_sub;
    }

    fn toggle_section(&mut self, index: usize) {
        if self.show_sub {
            self.show_sections[index] = !self.show_sections[index];
            if self.show_sections.iter().all(|s| !*s) {
                self.show_sub = false;
            }
        } else {
            self.show_sub = true;
            self.show_sections[index] = true;
        }
    }

    fn show_section(&mut self, index: usize) {
        self.show_sub = true;
        self.show_sections[index] = true;
    }
}

