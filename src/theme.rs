use crossterm::style::{Color, ResetColor, SetForegroundColor};
use whodatrow;

#[derive(Debug, Clone, Copy)]
pub enum Theme {
    Base(u8),
}

impl whodatrow::theme::Theme for Theme {
    const SELECTED: Self = Self::Base(0x2);
    const MARKED: Self = Self::Base(0x0b);
    const TOTAL: Self = Self::Base(0x04);
    const HEADER: Self = Self::Base(0x0d);
    const NORMAL: Self = Self::Base(0x07);

    fn apply(&self) -> String {
        match self {
            Self::Base(base) => SetForegroundColor(Color::AnsiValue(*base)).to_string(),
        }
    }

    fn cleanup(&self) -> String {
        ResetColor.to_string()
    }
}
