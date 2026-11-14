use crossterm::style::Stylize;

pub struct Histogram {
    height: usize,
}

impl Histogram {
    pub fn new(height: usize) -> Self {
        Self { height }
    }

    pub fn render(&self, mut data: Vec<usize>, width: usize) -> String {
        const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let mut rendered = String::new();

        let max = *data.iter().max().unwrap_or(&0);
        let scale_width = max.to_string().len() + 3;

        let plot_width = width.saturating_sub(scale_width);

        if !data.is_empty() && plot_width < data.len() {
            let chunk_size = data.len().div_ceil(plot_width.max(1));
            data = data
                .chunks(chunk_size)
                .map(|data| data.iter().sum())
                .collect();
        }

        if data.is_empty() || plot_width == 0 {
            //return;
        }

        let bar_width = plot_width / data.len();
        let taken = bar_width * data.len();
        let remaining = plot_width - taken;

        let max = *data.iter().max().unwrap_or(&0);
        let max_width = max.to_string().len();
        let scale_width = max_width + 3;
        let num_div = self.height * 8;

        for l in 0..self.height {
            if l == 0 {
                rendered.push_str(&format!("{max:>max_width$} ┤ "));
            } else if l == self.height - 1 {
                rendered.push_str(&format!("{:>max_width$} ┤ ", 0));
            } else {
                rendered.push_str(&format!("{:>max_width$} │ ", ""));
            }

            let d_lower = (self.height - 1 - l) * 8;

            for v in &data {
                let d = (num_div * *v).checked_div(max).unwrap_or(0);
                let d_sub = (d as i32 - d_lower as i32).clamp(0, 8);
                let block = BLOCKS[d_sub as usize];
                let colored = if *v == 0 {
                    block.dark_grey()
                } else if *v * 4 < max {
                    block.blue()
                } else if *v * 2 < max {
                    block.green()
                } else if *v * 4 < max * 3 {
                    block.yellow()
                } else {
                    block.red()
                }
                .to_string();
                rendered.extend(std::iter::repeat_n(colored, bar_width));
            }

            rendered.extend(std::iter::repeat_n(' ', remaining));
            rendered.push_str("\r\n");
        }

        rendered.extend(std::iter::repeat_n(' ', scale_width - 3));
        rendered.push_str(" ╰");
        rendered.extend(std::iter::repeat_n('─', taken.saturating_sub(2)));

        rendered
    }
}
