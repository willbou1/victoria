pub trait Theme: Copy {
    const SELECTED: Self;
    const MARKED: Self;
    const TOTAL: Self;
    const HEADER: Self;
    const NORMAL: Self;
    fn apply(&self) -> String;
    fn cleanup(&self) -> String;
}
