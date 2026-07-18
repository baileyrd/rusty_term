// Glassmorphism + Neon UI components for Aether
use ratatui::prelude::*;

pub fn render_glass(frame: &mut Frame, area: Rect) {
    // TODO: Implement glass panels, neon text, etc.
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(block, area);
}
