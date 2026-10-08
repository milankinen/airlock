//! Monitor tab.
//!
//! Shows the network activity, CPU usage and memory usage of the sandbox.

pub mod cpu;
mod histogram;
pub mod memory;
pub mod network;

pub use cpu::CpuState;
pub use memory::MemoryState;
pub use network::NetworkTab;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use self::cpu::CpuWidget;
use self::memory::MemoryWidget;
use self::network::NetworkWidget;

/// Width reserved for the right-hand CPU/Memory column.
const RIGHT_COL_WIDTH: u16 = 32;

/// Height of the header strip at the top: border, 1 content row, border.
const HEADER_HEIGHT: u16 = 3;

/// Maximum number of rows of the memory box in the right column.
const MEMORY_MAX_HEIGHT: u16 = 10;

/// State of the Monitor tab.
pub struct MonitorTab {
    /// State of the network panel.
    pub network: NetworkTab,
    /// State of the CPU panel.
    pub cpu: CpuState,
    /// State of the memory panel.
    pub memory: MemoryState,
    /// Project path. The tab header shows it.
    pub project_path: String,
    /// Version of the airlock CLI. The tab header shows it.
    pub version: String,
}

impl MonitorTab {
    /// Create an empty Monitor tab state.
    /// Args:
    ///  - `project_path`: Project path to show in the header
    ///  - `version`: Airlock CLI version to show in the header.
    pub fn new(project_path: String, version: String) -> Self {
        Self {
            network: NetworkTab::new(),
            cpu: CpuState::new(),
            memory: MemoryState::new(),
            project_path,
            version,
        }
    }

    /// Apply the latest guest stats snapshot to the CPU and memory panels.
    pub fn apply_stats(&mut self, snapshot: crate::StatsSnapshot) {
        self.cpu
            .set_snapshot(snapshot.per_core, Some(snapshot.load_avg));
        self.memory
            .set_usage(snapshot.total_bytes, snapshot.used_bytes);
    }
}

/// Widget that draws the full Monitor tab body.
///
/// Layout:
///
/// ```text
/// ┌ header strip (title + project path) ───────────────────┐
/// ├──────────────────────────────────────┬─────────────────┤
/// │                                      │ ┌ cpu ────────┐ │
/// │            network panel             │ └─────────────┘ │
/// │           (wide, takes rest)         │ ┌ memory ─────┐ │
/// │                                      │ └─────────────┘ │
/// └──────────────────────────────────────┴─────────────────┘
/// ```
///
/// In a narrow terminal, the network panel uses all of the body.
pub struct MonitorWidget<'a> {
    tab: &'a MonitorTab,
    policy: crate::Policy,
    bindings: &'a crate::keys::KeyBindings,
}

impl<'a> MonitorWidget<'a> {
    /// Create a Monitor tab widget.
    /// Args:
    ///  - `tab`: State of the Monitor tab
    ///  - `policy`: Current network policy, for the policy title
    ///  - `bindings`: Key bindings, for the shortcut hints.
    pub fn new(
        tab: &'a MonitorTab,
        policy: crate::Policy,
        bindings: &'a crate::keys::KeyBindings,
    ) -> Self {
        Self {
            tab,
            policy,
            bindings,
        }
    }
}

impl Widget for MonitorWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < HEADER_HEIGHT + 4 {
            return;
        }

        let [header_area, body_area] =
            Layout::vertical([Constraint::Length(HEADER_HEIGHT), Constraint::Min(1)]).areas(area);

        render_header(header_area, &self.tab.project_path, &self.tab.version, buf);

        if body_area.width > RIGHT_COL_WIDTH + 10 {
            let [left, right] =
                Layout::horizontal([Constraint::Min(10), Constraint::Length(RIGHT_COL_WIDTH)])
                    .areas(body_area);

            NetworkWidget::new(&self.tab.network, self.policy, self.bindings).render(left, buf);
            render_right_column(right, &self.tab.cpu, &self.tab.memory, buf);
        } else {
            NetworkWidget::new(&self.tab.network, self.policy, self.bindings)
                .render(body_area, buf);
        }
    }
}

fn render_header(area: Rect, project_path: &str, version: &str, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 {
        return;
    }

    // Title on the left. Project path on the same row, aligned to the right.
    let title_row = Rect::new(inner.x, inner.y, inner.width, 1);
    Paragraph::new(Line::from(vec![
        Span::styled(
            " airlock sandbox monitor",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({version})"),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .render(title_row, buf);

    Paragraph::new(
        Line::from(Span::styled(
            format!("{project_path} "),
            Style::default().fg(Color::DarkGray),
        ))
        .alignment(Alignment::Right),
    )
    .render(title_row, buf);
}

fn render_right_column(area: Rect, cpu: &CpuState, memory: &MemoryState, buf: &mut Buffer) {
    if area.height < 4 {
        return;
    }

    // The CPU box size comes from its content. The memory box has a fixed
    // height. They do not expand to fill the column. The space below them
    // stays empty, so the two panels are always at the top of the column.
    let mem_desired = MEMORY_MAX_HEIGHT;
    let cpu_desired = cpu.desired_height();
    let total = cpu_desired.saturating_add(mem_desired);

    let (cpu_h, mem_h) = if total <= area.height {
        (cpu_desired, mem_desired)
    } else if cpu_desired < area.height {
        // CPU fits. Memory gets the remaining rows.
        (cpu_desired, area.height - cpu_desired)
    } else {
        // Very short terminal. Memory gets its minimum (0), and CPU gets
        // all rows.
        (area.height, 0)
    };

    let [cpu_area, mem_area, _spare] = Layout::vertical([
        Constraint::Length(cpu_h),
        Constraint::Length(mem_h),
        Constraint::Min(0),
    ])
    .areas(area);

    CpuWidget::new(cpu).render(cpu_area, buf);
    if mem_area.height > 0 {
        MemoryWidget::new(memory).render(mem_area, buf);
    }
}
