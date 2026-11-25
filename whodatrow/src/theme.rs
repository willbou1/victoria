pub trait Theme: Copy {
    fn apply(&self) -> String;
    fn cleanup(&self) -> String;
}

pub trait TableTheme: Theme {
    const SELECTED: Self;
    const MARKED: Self;
    const TOTAL: Self;
    const HEADER: Self;
    const NORMAL: Self;
}

pub trait HistogramTheme: Theme {
    fn shade(level: f64) -> Self;
}
