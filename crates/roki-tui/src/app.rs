//! Top-level orchestration: terminal setup/restore, input forwarding, render
//! loop, Update reducer.

use std::io;
use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::cli::Cli;
use crate::client::ApiClient;
use crate::config::{ResolvedConfig, resolve};
use crate::input::{Action, classify};
use crate::model::{AppModel, PollSource, RefreshState, Update, View};
use crate::palette::{Palette, detect};
use crate::poll::{PollHandles, spawn as spawn_polls};
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

/// RAII handle that owns the terminal mode + alt-screen lifetime. `Drop` always
/// restores the terminal even when `drive()` returns via `?`, panics, or is
/// cancelled. The panic hook (installed once) restores the same way so the
/// user's terminal is never left in raw alt-screen state.
struct TerminalGuard {
    armed: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        Ok(Self { armed: true })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn install_panic_hook() {
    static SET: Once = Once::new();
    SET.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            prev(info);
        }));
    });
}

async fn run_inner(cfg: ResolvedConfig, palette: Palette, _headless: bool) -> Result<()> {
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    drive(cfg, palette, &mut terminal).await
}

async fn drive(
    cfg: ResolvedConfig,
    palette: Palette,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let client = Arc::new(ApiClient::new(&cfg.api_url)?);
    let (tx, mut rx) = mpsc::channel::<Update>(64);
    let handles = spawn_polls(client.clone(), cfg.polling.clone(), tx.clone());
    spawn_input_forwarder(tx.clone());
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

fn spawn_input_forwarder(tx: mpsc::Sender<Update>) {
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        loop {
            match stream.next().await {
                Some(Ok(Event::Key(k))) => {
                    if tx.send(Update::Input(k)).await.is_err() {
                        return;
                    }
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "input_stream_error");
                    let _ = tx
                        .send(Update::PollError {
                            source: PollSource::Input,
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
                None => return,
            }
        }
    });
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
        Update::Events {
            page,
            requested_since,
        } => model.events.merge_page(page, requested_since),
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
        Action::Focus(View::TicketDetail) => focus_ticket_detail(model, handles),
        Action::Focus(v) => model.focus_view(v),
        Action::Up => move_up(model),
        Action::Down => move_down(model),
        Action::Enter => enter_pressed(model, handles),
        Action::Refresh => trigger_refresh(model, client, tx),
        Action::ToggleAck => {
            if model.focus == View::Escalations {
                model.escalations.toggle_ack();
            }
        }
        Action::PrintLogCmd => print_log_command(model),
        Action::None => {}
    }
    true
}

fn focus_ticket_detail(model: &mut AppModel, handles: &PollHandles) {
    if let Some(id) = model.selected_ticket_id().map(str::to_string) {
        model.ticket_detail.focus_ticket(id.clone());
        if handles.focus_tx.send(Some(id)).is_err() {
            model.status.set("focus channel closed: detail poll dead");
        }
    }
    model.focus_view(View::TicketDetail);
}

fn move_up(model: &mut AppModel) {
    match model.focus {
        View::Tickets => model.tickets.move_up(),
        View::TicketDetail => model.ticket_detail.move_cycle_up(),
        View::Escalations => model.escalations.move_up(),
        _ => {}
    }
}

fn move_down(model: &mut AppModel) {
    match model.focus {
        View::Tickets => model.tickets.move_down(),
        View::TicketDetail => model.ticket_detail.move_cycle_down(),
        View::Escalations => model.escalations.move_down(),
        _ => {}
    }
}

fn enter_pressed(model: &mut AppModel, handles: &PollHandles) {
    if model.focus != View::Tickets {
        return;
    }
    let Some(id) = model.selected_ticket_id().map(str::to_string) else {
        return;
    };
    model.ticket_detail.focus_ticket(id.clone());
    if handles.focus_tx.send(Some(id)).is_err() {
        model.status.set("focus channel closed: detail poll dead");
    }
    model.focus_view(View::TicketDetail);
}

fn trigger_refresh(model: &mut AppModel, client: Arc<ApiClient>, tx: mpsc::Sender<Update>) {
    match model.refresh {
        RefreshState::Idle => start_refresh(model, client, tx),
        RefreshState::InFlight => {
            model.status.set("refresh: already in flight");
        }
        RefreshState::DebouncedUntil(t) => resume_debounced(model, t, client, tx),
    }
}

fn start_refresh(model: &mut AppModel, client: Arc<ApiClient>, tx: mpsc::Sender<Update>) {
    model.refresh = RefreshState::InFlight;
    tokio::spawn(async move {
        let update = match client.post_refresh().await {
            Ok(ack) => Update::RefreshAck(ack),
            Err(e) => Update::PollError {
                source: PollSource::Refresh,
                message: e.to_string(),
            },
        };
        if let Err(send_err) = tx.send(update).await {
            // The receive loop has exited; nothing can reset RefreshState now,
            // but the process is already shutting down. Log so a future
            // regression that triggers this without shutdown is visible.
            tracing::warn!(error = %send_err, "refresh_send_failed_after_rx_close");
        }
    });
}

fn resume_debounced(
    model: &mut AppModel,
    until: std::time::Instant,
    client: Arc<ApiClient>,
    tx: mpsc::Sender<Update>,
) {
    let now = std::time::Instant::now();
    let remaining = until.saturating_duration_since(now).as_secs();
    if remaining == 0 {
        model.refresh = RefreshState::Idle;
        start_refresh(model, client, tx);
        return;
    }
    model
        .status
        .set(format!("refresh: debounced ({remaining}s)"));
}

fn print_log_command(model: &mut AppModel) {
    if model.focus != View::TicketDetail {
        return;
    }
    let (Some(ticket), Some(c)) = (
        model.ticket_detail.ticket_id.clone(),
        model.ticket_detail.selected_cycle(),
    ) else {
        return;
    };
    let state = c.last_state_id.clone().unwrap_or_default();
    let n = c.total_visits.max(1);
    model.status.set(format!(
        "roki log --ticket {ticket} --cycle {} --iter {n} --state {state} --stream stdout",
        c.cycle_id
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Action;
    use crate::model::AppModel;
    use crate::palette::Palette;
    use tokio::sync::watch;

    fn fresh_model() -> AppModel {
        AppModel::new(Palette::IndexedAnsi16)
    }

    fn dummy_handles() -> (PollHandles, watch::Receiver<Option<String>>) {
        let (focus_tx, focus_rx) = watch::channel(None);
        (PollHandles { focus_tx }, focus_rx)
    }

    #[tokio::test]
    async fn refresh_inflight_rejects_a_second_refresh() {
        let (handles, _rx) = dummy_handles();
        let (tx, _mpsc_rx) = mpsc::channel::<Update>(8);
        let client = Arc::new(ApiClient::new("http://127.0.0.1:1").unwrap());
        let mut model = fresh_model();
        model.refresh = RefreshState::InFlight;
        let kept = apply_action(&mut model, Action::Refresh, &handles, client, tx);
        assert!(kept);
        assert!(model.status.text().contains("already in flight"));
        assert!(matches!(model.refresh, RefreshState::InFlight));
    }

    #[tokio::test]
    async fn refresh_debounced_shows_remaining_seconds() {
        let (handles, _rx) = dummy_handles();
        let (tx, _mpsc_rx) = mpsc::channel::<Update>(8);
        let client = Arc::new(ApiClient::new("http://127.0.0.1:1").unwrap());
        let mut model = fresh_model();
        model.refresh =
            RefreshState::DebouncedUntil(std::time::Instant::now() + Duration::from_secs(3));
        apply_action(&mut model, Action::Refresh, &handles, client, tx);
        assert!(
            model.status.text().contains("debounced"),
            "got: {}",
            model.status.text()
        );
        assert!(matches!(model.refresh, RefreshState::DebouncedUntil(_)));
    }

    #[tokio::test]
    async fn refresh_debounce_expired_re_enters_idle_and_starts() {
        let (handles, _rx) = dummy_handles();
        let (tx, _mpsc_rx) = mpsc::channel::<Update>(8);
        let client = Arc::new(ApiClient::new("http://127.0.0.1:1").unwrap());
        let mut model = fresh_model();
        // already-elapsed debounce: any past Instant produces remaining=0
        model.refresh =
            RefreshState::DebouncedUntil(std::time::Instant::now() - Duration::from_secs(1));
        apply_action(&mut model, Action::Refresh, &handles, client, tx);
        assert!(matches!(model.refresh, RefreshState::InFlight));
    }

    #[test]
    fn pollerror_refresh_resets_state_to_idle() {
        let (handles, _rx) = dummy_handles();
        let (tx, _mpsc_rx) = mpsc::channel::<Update>(8);
        let client = Arc::new(ApiClient::new("http://127.0.0.1:1").unwrap());
        let mut model = fresh_model();
        model.refresh = RefreshState::InFlight;
        let kept = apply(
            &mut model,
            Update::PollError {
                source: PollSource::Refresh,
                message: "boom".into(),
            },
            &handles,
            client,
            tx,
        );
        assert!(kept);
        assert!(matches!(model.refresh, RefreshState::Idle));
        assert!(model.status.text().contains("boom"));
    }

    #[test]
    fn focus_failure_when_detail_poll_already_dead_surfaces_status() {
        let (focus_tx, focus_rx) = watch::channel(None);
        drop(focus_rx);
        let handles = PollHandles { focus_tx };
        let (tx, _mpsc_rx) = mpsc::channel::<Update>(8);
        let client = Arc::new(ApiClient::new("http://127.0.0.1:1").unwrap());
        let mut model = fresh_model();
        model.tickets.rows = vec![roki_api_types::TicketSummary {
            ticket_id: "ENG-1".into(),
            repo: "github.com/x/y".into(),
            status: "open".into(),
            labels: vec![],
            assignee: "u".into(),
            in_flight_cycle_id: None,
            last_event_at: time::OffsetDateTime::from_unix_timestamp(0).unwrap(),
        }];
        apply_action(
            &mut model,
            Action::Focus(View::TicketDetail),
            &handles,
            client,
            tx,
        );
        assert!(
            model.status.text().contains("focus channel closed"),
            "got: {}",
            model.status.text()
        );
    }
}
