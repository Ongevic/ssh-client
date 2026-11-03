//! Startup host picker: lists concrete aliases from ~/.ssh/config with the
//! resolved `user@host:port`, supports a `/` filter, and offers a manual
//! connection form.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::sshconfig::{ResolvedHost, SshConfig, local_username};

pub struct PickerEntry {
    pub alias: String,
    pub resolved: ResolvedHost,
}

#[derive(Default)]
pub struct Picker {
    pub entries: Vec<PickerEntry>,
    pub filter: String,
    pub filtering: bool,
    pub selected: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PickerAction {
    None,
    Connect(String),
    Manual,
    Reload,
    Quit,
}

impl Picker {
    pub fn from_config(config: &SshConfig) -> Self {
        let user = local_username();
        let entries = config
            .concrete_aliases()
            .into_iter()
            .map(|alias| PickerEntry {
                resolved: config.resolve(&alias, &user),
                alias,
            })
            .collect();
        Self {
            entries,
            ..Default::default()
        }
    }

    pub fn visible(&self) -> Vec<&PickerEntry> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                needle.is_empty()
                    || e.alias.to_lowercase().contains(&needle)
                    || e.resolved.hostname.to_lowercase().contains(&needle)
            })
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.selected = 0;
                }
                KeyCode::Enter => {
                    self.filtering = false;
                    return self.connect_selected();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.selected = 0;
                }
                KeyCode::Up => self.move_by(-1),
                KeyCode::Down => self.move_by(1),
                KeyCode::Char(c) if !ctrl => {
                    self.filter.push(c);
                    self.selected = 0;
                }
                _ => {}
            }
            return PickerAction::None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => PickerAction::Quit,
            KeyCode::Char('c') if ctrl => PickerAction::Quit,
            KeyCode::Char('/') => {
                self.filtering = true;
                PickerAction::None
            }
            KeyCode::Char('m') | KeyCode::Char('M') => PickerAction::Manual,
            KeyCode::Char('r') => PickerAction::Reload,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_by(-1);
                PickerAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_by(1);
                PickerAction::None
            }
            KeyCode::Enter => self.connect_selected(),
            _ => PickerAction::None,
        }
    }

    fn connect_selected(&self) -> PickerAction {
        match self.visible().get(self.selected) {
            Some(entry) => PickerAction::Connect(entry.alias.clone()),
            None => PickerAction::None,
        }
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected =
            (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, status: &str) {
        let block = Block::default()
            .title("sshgrab  hosts from ~/.ssh/config")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(4),
            ])
            .split(inner);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Enter connects like `ssh <alias>`.  / filter   m manual host   r reload config   Esc/q quit"),
            ])
            .wrap(Wrap { trim: true }),
            chunks[0],
        );

        let visible = self.visible();
        let width = visible.iter().map(|e| e.alias.len()).max().unwrap_or(4).max(4);
        let items: Vec<ListItem> = visible
            .iter()
            .map(|e| {
                let mut spans = vec![
                    Span::styled(
                        format!("{:<width$}", e.alias),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::raw(e.resolved.summary()),
                ];
                if let Some(jump) = &e.resolved.proxy_jump {
                    spans.push(Span::styled(
                        format!("  via {jump}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .title(format!("Hosts ({})", visible.len()))
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");
        let mut state = ListState::default();
        if !visible.is_empty() {
            state.select(Some(self.selected.min(visible.len() - 1)));
        }
        frame.render_stateful_widget(list, chunks[1], &mut state);

        let filter_title = if self.filtering {
            "Filter (typing; Enter connects, Esc clears)"
        } else {
            "Filter (`/`)"
        };
        frame.render_widget(
            Paragraph::new(self.filter.as_str()).block(
                Block::default()
                    .title(filter_title)
                    .borders(Borders::ALL)
                    .border_style(if self.filtering {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    }),
            ),
            chunks[2],
        );
        frame.render_widget(
            Paragraph::new(status)
                .wrap(Wrap { trim: true })
                .block(Block::default().title("Status").borders(Borders::ALL)),
            chunks[3],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshconfig::ParseContext;
    use crossterm::event::KeyEventKind;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn picker() -> Picker {
        let ctx = ParseContext::from_home(PathBuf::from("/home/t"));
        let cfg = SshConfig::parse_str("Host zed alpha\nHost hpc\n HostName h.example\nHost *\n", &ctx);
        Picker::from_config(&cfg)
    }

    #[test]
    fn hpc_is_first_and_enter_connects_it() {
        let mut p = picker();
        assert_eq!(p.entries[0].alias, "hpc");
        assert_eq!(p.handle_key(key(KeyCode::Enter)), PickerAction::Connect("hpc".into()));
    }

    #[test]
    fn filter_narrows_list() {
        let mut p = picker();
        p.handle_key(key(KeyCode::Char('/')));
        p.handle_key(key(KeyCode::Char('z')));
        assert_eq!(p.visible().len(), 1);
        assert_eq!(p.handle_key(key(KeyCode::Enter)), PickerAction::Connect("zed".into()));
        assert_eq!(p.handle_key(key(KeyCode::Char('m'))), PickerAction::Manual);
    }
}
