//! Top-level orchestration: terminal setup/restore, input forwarding, render
//! loop, Update reducer.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::cli::Cli;
use crate::client::ApiClient;
use crate::config::{resolve, ResolvedConfig};
use crate::input::{classify, Action};
use crate::model::{AppModel, PollSource, RefreshState, Update, View};
use crate::palette::{detect, Palette};
use crate::poll::{spawn as spawn_polls, PollHandles};
use crate::startup_log;
use crate::ui;

pub struct App;

impl App {
    pub async fn run() -> Result<()> {
        let cli = Cli::parse_args();
        let cfg = resolve(cli)?;
        let palette = detect();
        startup_log::emit(io::stderr(), &cfg.api_url, &cfg.polling, palette)?;
        run_inner(cfg, palette, /*headless=*/ false).await
    }

    /// Headless entry for integration tests. Drives the model loop without a
    /// real terminal. Returns the model after `max_updates` Update messages
    /// were processed or `timeout` elapsed.
    pub async fn run_for_test(
        api_url: &str,
        max_updates: usize,
        timeout: Duration,
    ) -> Result<AppModel> {
        let palette = detect();
        let cfg = ResolvedConfig {
            api_url: api_url.into(),
            polling: crate::config::PollingSection {
                tickets_seconds: 1,
                events_seconds: 1,
                escalations_seconds: 1,
            },
        };
        run_inner_headless(cfg, palette, max_updates, timeout).await
    }
}

async fn run_inner(cfg: ResolvedConfig, palette: Palette, _headless: bool) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = drive(cfg, palette, &mut terminal).await;
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    result
}

async fn drive(
    cfg: ResolvedConfig,
    palette: Palette,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let client = Arc::new(ApiClient::new(&cfg.api_url)?);
    let (tx, mut rx) = mpsc::channel::<Update>(64);
    let handles = spawn_polls(client.clone(), cfg.polling.clone(), tx.clone());
    let mut input_stream = EventStream::new();
    let input_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(Ok(ev)) = input_stream.next().await {
            if let Event::Key(k) = ev {
                if input_tx.send(Update::Input(k)).await.is_err() {
                    break;
                }
            }
        }
    });
    let mut model = AppModel::new(palette);
    loop {
        terminal.draw(|f| ui::draw(f, &model))?;
        let Some(update) = rx.recv().await else { break };
        if matches!(update, Update::Quit) {
            break;
        }
        if !apply(&mut model, update, &handles, client.clone(), tx.clone()) {
            break;
        }
    }
    Ok(())
}

async fn run_inner_headless(
    cfg: ResolvedConfig,
    palette: Palette,
    max_updates: usize,
    timeout: Duration,
) -> Result<AppModel> {
    let client = Arc::new(ApiClient::new(&cfg.api_url)?);
    let (tx, mut rx) = mpsc::channel::<Update>(64);
    let handles = spawn_polls(client.clone(), cfg.polling.clone(), tx.clone());
    let mut model = AppModel::new(palette);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = 0usize;
    while seen < max_updates {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(update)) => {
                if matches!(update, Update::Quit) {
                    break;
                }
                if !apply(&mut model, update, &handles, client.clone(), tx.clone()) {
                    break;
                }
                seen += 1;
            }
            _ => break,
        }
    }
    Ok(model)
}

fn apply(
    model: &mut AppModel,
    update: Update,
    handles: &PollHandles,
    client: Arc<ApiClient>,
    tx: mpsc::Sender<Update>,
) -> bool {
    match update {
        Update::Tickets(rows) => model.tickets.apply(rows),
        Update::TicketDetail(d) => model.apply_ticket_detail(d),
        Update::Cycles(rows) => model.apply_cycles(rows),
        Update::Tail { visit_n, body } => model.apply_tail(visit_n, body),
        Update::Events { page, requested_since } => model.events.merge_page(page, requested_since),
        Update::Escalations(rows) => model.escalations.apply(rows),
        Update::RefreshAck(ack) => model.apply_refresh_ack(ack),
        Update::PollError { source, message } => {
            model.status.set(format!("{:?}: {message}", source));
            if matches!(source, PollSource::Refresh) {
                model.refresh = RefreshState::Idle;
            }
        }
        Update::Input(ev) => {
            let action = classify(ev);
            return apply_action(model, action, handles, client, tx);
        }
        Update::Quit => return false,
    }
    true
}

fn apply_action(
    model: &mut AppModel,
    action: Action,
    handles: &PollHandles,
    client: Arc<ApiClient>,
    tx: mpsc::Sender<Update>,
) -> bool {
    match action {
        Action::Quit => return false,
        Action::Focus(View::TicketDetail) => {
            if let Some(id) = model.selected_ticket_id().map(str::to_string) {
                model.ticket_detail.focus_ticket(id.clone());
                let _ = handles.focus_tx.send(Some(id));
            }
            model.focus_view(View::TicketDetail);
        }
        Action::Focus(v) => model.focus_view(v),
        Action::Up => match model.focus {
            View::Tickets => model.tickets.move_up(),
            View::TicketDetail => model.ticket_detail.move_cycle_up(),
            View::Escalations => model.escalations.move_up(),
            _ => {}
        },
        Action::Down => match model.focus {
            View::Tickets => model.tickets.move_down(),
            View::TicketDetail => model.ticket_detail.move_cycle_down(),
            View::Escalations => model.escalations.move_down(),
            _ => {}
        },
        Action::Enter => {
            if model.focus == View::Tickets {
                if let Some(id) = model.selected_ticket_id().map(str::to_string) {
                    model.ticket_detail.focus_ticket(id.clone());
                    let _ = handles.focus_tx.send(Some(id));
                    model.focus_view(View::TicketDetail);
                }
            }
        }
        Action::Refresh => {
            match model.refresh {
                RefreshState::Idle => {
                    model.refresh = RefreshState::InFlight;
                    let c = client.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        match c.post_refresh().await {
                            Ok(ack) => {
                                let _ = tx.send(Update::RefreshAck(ack)).await;
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(Update::PollError {
                                        source: PollSource::Refresh,
                                        message: e.to_string(),
                                    })
                                    .await;
                            }
                        }
                    });
                }
                RefreshState::InFlight => {
                    model.status.set("refresh: already in flight");
                }
                RefreshState::DebouncedUntil(t) => {
                    let now = std::time::Instant::now();
                    let remaining = t.saturating_duration_since(now).as_secs();
                    if remaining == 0 {
                        model.refresh = RefreshState::Idle;
                        return apply_action(model, Action::Refresh, handles, client, tx);
                    }
                    model.status.set(format!("refresh: debounced ({remaining}s)"));
                }
            }
        }
        Action::ToggleAck => {
            if model.focus == View::Escalations {
                model.escalations.toggle_ack();
            }
        }
        Action::PrintLogCmd => {
            if model.focus == View::TicketDetail {
                if let (Some(ticket), Some(c)) =
                    (model.ticket_detail.ticket_id.clone(), model.ticket_detail.selected_cycle())
                {
                    let state = c.last_state_id.clone().unwrap_or_default();
                    let n = c.total_visits.max(1);
                    model.status.set(format!(
                        "roki log --ticket {ticket} --cycle {} --iter {n} --state {state} --stream stdout",
                        c.cycle_id
                    ));
                }
            }
        }
        Action::None => {}
    }
    true
}
