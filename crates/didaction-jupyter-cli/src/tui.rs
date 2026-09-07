use crate::config::{RuntimeKind, Settings};
use crossterm::{event, execute, terminal};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::{io, time::Duration};

pub fn run(settings: &mut Settings) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = event_loop(&mut terminal, settings);
    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), terminal::LeaveAlternateScreen)?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    settings: &mut Settings,
) -> io::Result<()> {
    let ids: Vec<String> = settings.kernels.keys().cloned().collect();
    let mut state = ListState::default().with_selected(Some(0));
    loop {
        terminal.draw(|frame| {
            let items = ids.iter().map(|id| {
                let profile = &settings.kernels[id];
                let selected = if profile.enabled { "[x]" } else { "[ ]" };
                let runtime = match profile.runtime {
                    RuntimeKind::Server => "server",
                    RuntimeKind::Browser => "browser",
                };
                let default = if settings.default_kernel == *id {
                    " (default)"
                } else {
                    ""
                };
                ListItem::new(format!(
                    "{selected} {} · {runtime}{default}",
                    profile.display_name
                ))
            });
            let list = List::new(items)
                .block(
                    Block::default()
                        .title(" Kernel profiles ")
                        .borders(Borders::ALL),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ");
            frame.render_stateful_widget(list, frame.area(), &mut state);
            let help =
                Paragraph::new("↑/↓ select · Space enable · d default · Enter save · q cancel")
                    .style(Style::default().fg(Color::DarkGray));
            let area = frame.area();
            frame.render_widget(
                help,
                ratatui::layout::Rect::new(2, area.height - 2, area.width - 4, 1),
            );
        })?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        if let event::Event::Key(key) = event::read()? {
            let index = state.selected().unwrap_or(0);
            match key.code {
                event::KeyCode::Up => state.select(Some(index.saturating_sub(1))),
                event::KeyCode::Down => state.select(Some((index + 1).min(ids.len() - 1))),
                event::KeyCode::Char(' ') => {
                    let profile = settings.kernels.get_mut(&ids[index]).unwrap();
                    profile.enabled = !profile.enabled;
                }
                event::KeyCode::Char('d') => {
                    settings.default_kernel = ids[index].clone();
                    settings.kernels.get_mut(&ids[index]).unwrap().enabled = true;
                }
                event::KeyCode::Enter => return Ok(()),
                event::KeyCode::Char('q') | event::KeyCode::Esc => {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                _ => {}
            }
        }
    }
}
