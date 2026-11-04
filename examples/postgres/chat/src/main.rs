use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::text::Line;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use std::sync::Arc;
use std::{error::Error, io};
use tokio::{sync::Mutex, time::Duration};
use unicode_width::UnicodeWidthStr;

struct ChatApp {
    input: String,
    messages: Arc<Mutex<Vec<String>>>,
    pool: PgPool,
}

impl ChatApp {
    fn new(pool: PgPool) -> Self {
        ChatApp {
            input: String::new(),
            messages: Arc::new(Mutex::new(Vec::new())),
            pool,
        }
    }

    async fn run<B: Backend>(
        mut self,
        terminal: &mut Terminal<B>,
        mut listener: PgListener,
    ) -> Result<(), Box<dyn Error>>
    where
        <B as Backend>::Error: 'static,
    {
        // setup listener task
        let messages = self.messages.clone();
        tokio::spawn(async move {
            while let Ok(msg) = listener.recv().await {
                messages.lock().await.push(msg.payload().to_string());
            }
        });

        loop {
            let messages: Vec<ListItem> = self
                .messages
                .lock()
                .await
                .iter()
                .map(|m| {
                    let content = vec![Line::from(Span::raw(m.to_owned()))];
                    ListItem::new(content)
                })
                .collect();

            terminal.draw(|f| self.ui(f, messages))?;

            if !event::poll(Duration::from_millis(20))? {
                continue;
            }

            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Enter => {
                        notify(&self.pool, &self.input).await?;
                        self.input.clear();
                    }
                    KeyCode::Char(c) => {
                        self.input.push(c);
                    }
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::Esc => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }

    fn ui(&mut self, frame: &mut Frame, messages: Vec<ListItem>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints(
                [
                    Constraint::Length(1),
                    Constraint::Length(3),
                    Constraint::Min(1),
                ]
                .as_ref(),
            )
            .split(frame.size());

        let text = Text::from(Line::from(vec![
            Span::raw("Press "),
            Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" to send the message, "),
            Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" to quit"),
        ]));
        let help_message = Paragraph::new(text);
        frame.render_widget(help_message, chunks[0]);

        let input = Paragraph::new(self.input.as_str())
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().borders(Borders::ALL).title("Input"));
        frame.render_widget(input, chunks[1]);
        frame.set_cursor(
            // Put cursor past the end of the input text
            chunks[1].x + self.input.width() as u16 + 1,
            // Move one line down, from the border to the input line
            chunks[1].y + 1,
        );

        let messages =
            List::new(messages).block(Block::default().borders(Borders::ALL).title("Messages"));
        frame.render_widget(messages, chunks[2]);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // setup postgres
    let conn_url =
        std::env::var("DATABASE_URL").expect("Env var DATABASE_URL is required for this example.");
    let pool = PgPool::connect(&conn_url).await?;

    let mut listener = PgListener::connect(&conn_url).await?;
    listener.listen("chan0").await?;

    // setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // create app and run it
    let app = ChatApp::new(pool);
    let res = app.run(&mut terminal, listener).await;

    // restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{err:?}")
    }

    Ok(())
}

async fn notify(pool: &PgPool, s: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
SELECT pg_notify(chan, payload)
FROM (VALUES ('chan0', $1)) v(chan, payload)
"#,
    )
    .bind(s)
    .execute(pool)
    .await?;

    Ok(())
}
