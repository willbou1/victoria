use crossterm::style::{Color, ResetColor, SetForegroundColor};
use whodatrow;

#[derive(Debug, Clone, Copy)]
pub enum Theme {
    Base(u8),
}

impl whodatrow::theme::Theme for Theme {
    fn apply(&self) -> String {
        match self {
            Self::Base(base) => SetForegroundColor(Color::AnsiValue(*base)).to_string(),
        }
    }

    fn cleanup(&self) -> String {
        ResetColor.to_string()
    }
}

impl whodatrow::theme::TableTheme for Theme {
    const SELECTED: Self = Self::Base(0x2);
    const MARKED: Self = Self::Base(0x0b);
    const TOTAL: Self = Self::Base(0x04);
    const HEADER: Self = Self::Base(0x0d);
    const NORMAL: Self = Self::Base(0x07);
}

impl whodatrow::theme::HistogramTheme for Theme {
    fn shade(level: f64) -> Self {
        if level < 0.25 {
            Self::Base(0x0b)
        } else if level < 0.5 {
            Self::Base(0x0a)
        } else if level < 0.75 {
            Self::Base(0x0d)
        } else {
            Self::Base(0x09)
        }
    }
}
