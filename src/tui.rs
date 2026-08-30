//! `effiqueue top` — a live view of a running instance.
//!
//! This is a **client**, not part of the daemon. It polls the Prometheus
//! endpoint the daemon already exposes and renders it; the control loop has no
//! idea it exists and pays nothing when it is not running. That also means it
//! works against a remote instance, which is the normal case for a tool that
//! lives on long-running VMs — and that a daemon under systemd or Docker, with
//! no TTY, never has to care about terminal state.
//!
//! What it shows is the project's actual thesis: `workers_needed` against
//! `workers_capacity`, with the Feasibility Gap between them, and where `mu`
//! is currently coming from.

use crate::http;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Sparkline, Tabs};
use ratatui::{Frame, Terminal};
use std::collections::BTreeMap;
use std::io;
use std::time::{Duration, Instant};

/// Samples kept per program for the sparklines.
const HISTORY: usize = 240;

/// One program's metrics, as scraped.
#[derive(Debug, Clone, Default)]
pub struct Program {
    pub name: String,
    pub workers: f64,
    pub backlog: f64,
    pub workers_needed: f64,
    pub workers_capacity: f64,
    pub feasibility_gap: f64,
    pub mu: f64,
    pub lambda: f64,
    pub mu_source: f64,
    pub probing: f64,
    pub worker_rss_bytes: f64,
    pub pool_rss_bytes: f64,
    pub ram_budget_bytes: f64,
    pub best_drain_seconds: f64,
    pub slo_drain_seconds: f64,
    pub estimator_spread: f64,
    pub spawn_backoff_seconds: f64,
    pub scale_up_total: f64,
    pub scale_down_total: f64,
    pub probe_total: f64,
    pub backlog_history: Vec<u64>,
    pub worker_history: Vec<u64>,
}

impl Program {
    fn source_label(&self) -> (&'static str, Color) {
        match self.mu_source as u64 {
            1 => ("broker rates", Color::Green),
            2 => ("regression", Color::Cyan),
            _ => ("not measured", Color::DarkGray),
        }
    }
}

/// Parse the Prometheus text exposition into `program -> metric -> value`.
///
/// Only the flat `name{program="x"} value` shape this crate emits is handled;
/// comments and any other label sets are skipped.
pub fn parse_metrics(body: &str) -> BTreeMap<String, BTreeMap<String, f64>> {
    let mut out: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((series, value)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<f64>() else {
            continue;
        };
        let Some((name, rest)) = series.split_once('{') else {
            continue;
        };
        let Some(labels) = rest.strip_suffix('}') else {
            continue;
        };
        let Some(program) = labels
            .strip_prefix("program=\"")
            .and_then(|l| l.strip_suffix('"'))
        else {
            continue;
        };
        out.entry(unescape(program))
            .or_default()
            .insert(name.to_string(), value);
    }
    out
}

/// Reverse the label escaping applied on the way out.
fn unescape(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn merge(programs: &mut Vec<Program>, scraped: BTreeMap<String, BTreeMap<String, f64>>) {
    for (name, m) in scraped {
        let get = |k: &str| m.get(k).copied().unwrap_or(0.0);
        let existing = programs.iter_mut().find(|p| p.name == name);
        let p = match existing {
            Some(p) => p,
            None => {
                programs.push(Program {
                    name: name.clone(),
                    ..Default::default()
                });
                programs.last_mut().expect("just pushed")
            }
        };
        p.workers = get("effiqueue_workers");
        p.backlog = get("effiqueue_backlog");
        p.workers_needed = get("effiqueue_workers_needed");
        p.workers_capacity = get("effiqueue_workers_capacity");
        p.feasibility_gap = get("effiqueue_feasibility_gap");
        p.mu = get("effiqueue_mu");
        p.lambda = get("effiqueue_lambda");
        p.mu_source = get("effiqueue_mu_source");
        p.probing = get("effiqueue_probing");
        p.worker_rss_bytes = get("effiqueue_worker_rss_bytes");
        p.pool_rss_bytes = get("effiqueue_pool_rss_bytes");
        p.ram_budget_bytes = get("effiqueue_ram_budget_bytes");
        p.best_drain_seconds = get("effiqueue_best_drain_seconds");
        p.slo_drain_seconds = get("effiqueue_slo_drain_seconds");
        p.estimator_spread = get("effiqueue_estimator_spread");
        p.spawn_backoff_seconds = get("effiqueue_spawn_backoff_seconds");
        p.scale_up_total = get("effiqueue_scale_up_total");
        p.scale_down_total = get("effiqueue_scale_down_total");
        p.probe_total = get("effiqueue_probe_total");

        push_history(&mut p.backlog_history, p.backlog.max(0.0) as u64);
        push_history(&mut p.worker_history, p.workers.max(0.0) as u64);
    }
}

fn push_history(history: &mut Vec<u64>, value: u64) {
    history.push(value);
    if history.len() > HISTORY {
        let excess = history.len() - HISTORY;
        history.drain(0..excess);
    }
}

fn human_bytes(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1.0 {
        return "—".into();
    }
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}

fn human_seconds(seconds: f64) -> String {
    if seconds < 0.0 {
        return "never".into();
    }
    // Stay in seconds well past a minute: the SLO is rendered with this, and an
    // operator who configured `120s` should see `120s` rather than a translation
    // of it.
    if seconds < 180.0 {
        return format!("{seconds:.0}s");
    }
    format!("{:.1}m", seconds / 60.0)
}

/// Viewer state.
struct App {
    endpoint: String,
    host: String,
    port: u16,
    path: String,
    interval: Duration,
    programs: Vec<Program>,
    selected: usize,
    paused: bool,
    last_ok: Option<Instant>,
    error: Option<String>,
}

impl App {
    async fn refresh(&mut self) {
        match http::get(
            &self.host,
            self.port,
            &self.path,
            None,
            Duration::from_secs(3),
        )
        .await
        {
            Ok(body) => {
                merge(&mut self.programs, parse_metrics(&body));
                if self.programs.is_empty() {
                    self.error =
                        Some("endpoint responded but exposed no effiqueue series".to_string());
                } else {
                    self.error = None;
                    self.last_ok = Some(Instant::now());
                    self.selected = self.selected.min(self.programs.len() - 1);
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
}

/// Entry point for `effiqueue top`.
pub async fn run(url: &str, interval: Duration) -> anyhow::Result<()> {
    let (host, port, path) = split_url(url)?;
    let mut app = App {
        endpoint: url.to_string(),
        host,
        port,
        path,
        interval,
        programs: Vec::new(),
        selected: 0,
        paused: false,
        last_ok: None,
        error: None,
    };
    app.refresh().await;

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let result = event_loop(&mut term, &mut app).await;

    // Restore the terminal even if the loop failed; a panicking TUI that leaves
    // the terminal in raw mode is worse than the original error.
    terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen)?;
    term.show_cursor()?;
    result
}

type Tui = Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>;

async fn event_loop(term: &mut Tui, app: &mut App) -> anyhow::Result<()> {
    let mut next_poll = Instant::now() + app.interval;
    loop {
        term.draw(|f| draw(f, app))?;

        // Poll for input on a short tick so the UI stays responsive between
        // scrapes, which are seconds apart.
        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c')
                            if key
                                .modifiers
                                .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            return Ok(())
                        }
                        KeyCode::Char('p') | KeyCode::Char(' ') => app.paused = !app.paused,
                        KeyCode::Char('r') => next_poll = Instant::now(),
                        KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => {
                            if !app.programs.is_empty() {
                                app.selected = (app.selected + 1) % app.programs.len();
                            }
                        }
                        KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') => {
                            if !app.programs.is_empty() {
                                app.selected = app
                                    .selected
                                    .checked_sub(1)
                                    .unwrap_or(app.programs.len() - 1);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if !app.paused && Instant::now() >= next_poll {
            app.refresh().await;
            next_poll = Instant::now() + app.interval;
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);

    if let Some(p) = app.programs.get(app.selected) {
        draw_program(f, chunks[1], p);
    } else {
        let msg = app
            .error
            .clone()
            .unwrap_or_else(|| "waiting for the first scrape...".into());
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(Color::Red))
                .block(Block::default().borders(Borders::ALL).title(" effiQueue ")),
            chunks[1],
        );
    }

    let footer = "q quit   ←/→ program   p pause   r refresh now";
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line> = if app.programs.is_empty() {
        vec![Line::from("(no programs)")]
    } else {
        app.programs
            .iter()
            .map(|p| Line::from(p.name.clone()))
            .collect()
    };
    let status = match (&app.error, app.paused) {
        (Some(e), _) => Span::styled(
            format!(" ✗ {e} "),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        (None, true) => Span::styled(" ‖ paused ", Style::default().fg(Color::Yellow)),
        (None, false) => Span::styled(" ● live ", Style::default().fg(Color::Green)),
    };
    let title = Line::from(vec![
        Span::styled(
            " effiQueue ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(app.endpoint.clone(), Style::default().fg(Color::DarkGray)),
        status,
    ]);
    f.render_widget(
        Tabs::new(titles)
            .select(app.selected)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_program(f: &mut Frame, area: Rect, p: &Program) {
    let gap = p.feasibility_gap > 0.0;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if gap { 4 } else { 0 }), // feasibility gap banner
            Constraint::Length(6),                       // capacity bars
            Constraint::Length(7),                       // estimator + memory
            Constraint::Min(0),                          // sparklines
        ])
        .split(area);

    if gap {
        draw_gap(f, rows[0], p);
    }
    draw_capacity(f, rows[1], p);
    draw_stats(f, rows[2], p);
    draw_history(f, rows[3], p);
}

/// The signature readout. If this is on screen, it is the thing to read.
fn draw_gap(f: &mut Frame, area: Rect, p: &Program) {
    let short_bytes = human_bytes(p.feasibility_gap * p.worker_rss_bytes);
    let text = vec![
        Line::from(Span::styled(
            format!(
                "SLO {} unreachable on this host",
                human_seconds(p.slo_drain_seconds)
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "need {} workers, this machine safely fits {} — short by {} (~{})   best achievable drain: {}",
            p.workers_needed.max(0.0),
            p.workers_capacity,
            p.feasibility_gap,
            short_bytes,
            human_seconds(p.best_drain_seconds),
        )),
    ];
    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(" feasibility gap "),
        ),
        area,
    );
}

fn draw_capacity(f: &mut Frame, area: Rect, p: &Program) {
    let block = Block::default().borders(Borders::ALL).title(" capacity ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Everything is scaled against the same ceiling so the three bars are
    // directly comparable — that comparison is the whole point of the view.
    let ceiling = p
        .workers_capacity
        .max(p.workers)
        .max(p.workers_needed.max(0.0))
        .max(1.0);
    let bars = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let needed_known = p.workers_needed >= 0.0;
    let rows: [(&str, f64, Color, String); 3] = [
        (
            "running ",
            p.workers,
            Color::Cyan,
            format!("{}", p.workers as u64),
        ),
        (
            "needed  ",
            p.workers_needed.max(0.0),
            if needed_known {
                Color::Yellow
            } else {
                Color::DarkGray
            },
            if needed_known {
                format!("{}", p.workers_needed as u64)
            } else {
                "unknown".into()
            },
        ),
        (
            "capacity",
            p.workers_capacity,
            Color::Green,
            format!("{}", p.workers_capacity as u64),
        ),
    ];
    for (i, (label, value, color, text)) in rows.iter().enumerate() {
        f.render_widget(
            Gauge::default()
                .gauge_style(Style::default().fg(*color))
                .ratio((value / ceiling).clamp(0.0, 1.0))
                .label(format!("{label}  {text}")),
            bars[i],
        );
    }
}

fn draw_stats(f: &mut Frame, area: Rect, p: &Program) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let (source, source_color) = p.source_label();
    let mu_line = if p.mu > 0.0 {
        format!("{:.2} msg/s/worker", p.mu)
    } else {
        "—".to_string()
    };
    let mut estimator = vec![
        Line::from(vec![
            Span::raw("µ         "),
            Span::styled(mu_line, Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(format!("λ         {:.2} msg/s", p.lambda)),
        Line::from(vec![
            Span::raw("source    "),
            Span::styled(source, Style::default().fg(source_color)),
        ]),
    ];
    if p.mu_source as u64 == 2 || p.mu <= 0.0 {
        estimator.push(Line::from(format!("spread    {:.1}", p.estimator_spread)));
    }
    if p.probing > 0.0 {
        estimator.push(Line::from(Span::styled(
            "probing — perturbing to identify µ",
            Style::default().fg(Color::Magenta),
        )));
    }
    f.render_widget(
        Paragraph::new(estimator)
            .block(Block::default().borders(Borders::ALL).title(" estimator ")),
        cols[0],
    );

    let mut memory = vec![
        Line::from(format!("per worker  {}", human_bytes(p.worker_rss_bytes))),
        Line::from(format!("pool        {}", human_bytes(p.pool_rss_bytes))),
        Line::from(format!("budget      {}", human_bytes(p.ram_budget_bytes))),
        Line::from(format!(
            "scaled      ↑{} ↓{}  probes {}",
            p.scale_up_total as u64, p.scale_down_total as u64, p.probe_total as u64
        )),
    ];
    if p.spawn_backoff_seconds > 0.0 {
        memory.push(Line::from(Span::styled(
            format!(
                "crash-loop back-off: {} left",
                human_seconds(p.spawn_backoff_seconds)
            ),
            Style::default().fg(Color::Red),
        )));
    }
    f.render_widget(
        Paragraph::new(memory).block(Block::default().borders(Borders::ALL).title(" memory ")),
        cols[1],
    );
}

fn draw_history(f: &mut Frame, area: Rect, p: &Program) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    f.render_widget(
        Sparkline::default()
            .data(&p.backlog_history)
            .style(Style::default().fg(Color::Blue))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" backlog  {} ", p.backlog as u64)),
            ),
        rows[0],
    );
    f.render_widget(
        Sparkline::default()
            .data(&p.worker_history)
            .style(Style::default().fg(Color::Cyan))
            .block(Block::default().borders(Borders::ALL).title(" workers ")),
        rows[1],
    );
}

/// Split `http://host:port/path` into its parts.
fn split_url(url: &str) -> anyhow::Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("expected an http:// URL, got '{url}'"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/metrics"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse()
                .map_err(|_| anyhow::anyhow!("invalid port in '{url}'"))?,
        ),
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        anyhow::bail!("missing host in '{url}'");
    }
    Ok((host, port, path.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# HELP effiqueue_workers Number of live workers.
# TYPE effiqueue_workers gauge
effiqueue_workers{program="index"} 4
effiqueue_workers{program="mailer"} 1
# TYPE effiqueue_workers_needed gauge
effiqueue_workers_needed{program="index"} 78
effiqueue_workers_needed{program="mailer"} -1
# TYPE effiqueue_workers_capacity gauge
effiqueue_workers_capacity{program="index"} 24
# TYPE effiqueue_mu gauge
effiqueue_mu{program="index"} 8.25
effiqueue_mu_source{program="index"} 2
"#;

    #[test]
    fn parses_the_exposition_format_per_program() {
        let m = parse_metrics(SAMPLE);
        assert_eq!(m.len(), 2);
        assert_eq!(m["index"]["effiqueue_workers"], 4.0);
        assert_eq!(m["index"]["effiqueue_workers_needed"], 78.0);
        assert_eq!(m["index"]["effiqueue_mu"], 8.25);
        assert_eq!(m["mailer"]["effiqueue_workers"], 1.0);
        // The documented "not measured" sentinel must survive as a negative.
        assert_eq!(m["mailer"]["effiqueue_workers_needed"], -1.0);
    }

    #[test]
    fn ignores_comments_and_unparseable_lines() {
        let m = parse_metrics("# HELP x\ngarbage\nfoo{program=\"a\"} notanumber\n");
        assert!(m.is_empty());
    }

    #[test]
    fn round_trips_labels_escaped_on_the_way_out() {
        // metrics.rs escapes backslashes and quotes; this is the other half.
        let m = parse_metrics("effiqueue_workers{program=\"we\\\"ird\\\\name\"} 2\n");
        assert_eq!(m.keys().next().unwrap(), r#"we"ird\name"#);
    }

    #[test]
    fn merge_accumulates_history_and_bounds_it() {
        let mut progs = Vec::new();
        for i in 0..(HISTORY + 25) {
            let body = format!("effiqueue_backlog{{program=\"a\"}} {i}\n");
            merge(&mut progs, parse_metrics(&body));
        }
        assert_eq!(progs.len(), 1);
        assert_eq!(progs[0].backlog_history.len(), HISTORY);
        // Oldest samples are dropped, newest retained.
        assert_eq!(
            *progs[0].backlog_history.last().unwrap(),
            (HISTORY + 24) as u64
        );
    }

    #[test]
    fn merge_updates_a_program_in_place() {
        let mut progs = Vec::new();
        merge(
            &mut progs,
            parse_metrics("effiqueue_workers{program=\"a\"} 1\n"),
        );
        merge(
            &mut progs,
            parse_metrics("effiqueue_workers{program=\"a\"} 5\n"),
        );
        assert_eq!(progs.len(), 1, "must not duplicate the program");
        assert_eq!(progs[0].workers, 5.0);
        assert_eq!(progs[0].worker_history.len(), 2);
    }

    #[test]
    fn splits_metrics_urls() {
        assert_eq!(
            split_url("http://127.0.0.1:9101/metrics").unwrap(),
            ("127.0.0.1".to_string(), 9101, "/metrics".to_string())
        );
        // A bare authority defaults to the conventional path.
        assert_eq!(
            split_url("http://box:9101").unwrap(),
            ("box".to_string(), 9101, "/metrics".to_string())
        );
        assert!(split_url("https://box:9101").is_err());
        assert!(split_url("box:9101").is_err());
        assert!(split_url("http://box:notaport").is_err());
    }

    #[test]
    fn formats_bytes_and_durations_for_humans() {
        assert_eq!(human_bytes(0.0), "—");
        assert_eq!(human_bytes(512.0), "512.0B");
        assert_eq!(human_bytes(1536.0), "1.5KB");
        assert_eq!(human_bytes(12.0 * 1024.0 * 1024.0 * 1024.0), "12.0GB");
        assert_eq!(human_seconds(-1.0), "never");
        assert_eq!(human_seconds(42.0), "42s");
        // A configured SLO reads back as configured, not translated.
        assert_eq!(human_seconds(120.0), "120s");
        assert_eq!(human_seconds(300.0), "5.0m");
    }

    /// Render the whole view into an in-memory terminal and return it as text.
    fn render(app: &App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with(programs: Vec<Program>) -> App {
        App {
            endpoint: "http://127.0.0.1:9101/metrics".into(),
            host: "127.0.0.1".into(),
            port: 9101,
            path: "/metrics".into(),
            interval: Duration::from_secs(2),
            programs,
            selected: 0,
            paused: false,
            last_ok: None,
            error: None,
        }
    }

    #[test]
    fn renders_the_feasibility_gap_prominently() {
        // The README's worked example. If the gap is on screen it is the thing
        // to read, so assert the numbers an operator would act on are present.
        let p = Program {
            name: "index".into(),
            workers: 10.0,
            backlog: 50_000.0,
            workers_needed: 78.0,
            workers_capacity: 24.0,
            feasibility_gap: 54.0,
            mu: 8.0,
            lambda: 200.0,
            mu_source: 1.0,
            worker_rss_bytes: 512.0 * 1024.0 * 1024.0,
            best_drain_seconds: -1.0,
            slo_drain_seconds: 120.0,
            backlog_history: vec![10, 20, 30],
            worker_history: vec![1, 2, 3],
            ..Default::default()
        };
        let out = render(&app_with(vec![p]), 110, 34);
        assert!(out.contains("feasibility gap"), "no gap panel:\n{out}");
        assert!(out.contains("SLO 120s unreachable"), "no verdict:\n{out}");
        assert!(out.contains("need 78"), "missing workers_needed:\n{out}");
        assert!(out.contains("fits 24"), "missing capacity:\n{out}");
        assert!(out.contains("short by 54"), "missing the gap:\n{out}");
        assert!(out.contains("27.0GB"), "missing the RAM shortfall:\n{out}");
        assert!(
            out.contains("never"),
            "an unreachable drain must say so:\n{out}"
        );
        assert!(out.contains("broker rates"), "missing mu source:\n{out}");
    }

    #[test]
    fn renders_without_a_gap_and_marks_unknown_mu() {
        let p = Program {
            name: "mailer".into(),
            workers: 2.0,
            backlog: 5.0,
            workers_needed: -1.0, // the documented "not measured" sentinel
            workers_capacity: 8.0,
            feasibility_gap: 0.0,
            mu_source: 0.0,
            backlog_history: vec![5, 4, 5],
            worker_history: vec![2, 2, 2],
            ..Default::default()
        };
        let out = render(&app_with(vec![p]), 110, 30);
        assert!(
            !out.contains("feasibility gap"),
            "gap shown when none:\n{out}"
        );
        assert!(
            out.contains("unknown"),
            "-1 must not render as a number:\n{out}"
        );
        assert!(out.contains("not measured"), "missing mu source:\n{out}");
    }

    #[test]
    fn renders_an_error_state_when_the_endpoint_is_down() {
        let mut app = app_with(Vec::new());
        app.error = Some("connection refused".into());
        let out = render(&app, 80, 20);
        assert!(
            out.contains("connection refused"),
            "error not shown:\n{out}"
        );
    }

    #[test]
    fn renders_every_program_as_a_tab() {
        let app = app_with(vec![
            Program {
                name: "index".into(),
                ..Default::default()
            },
            Program {
                name: "mailer".into(),
                ..Default::default()
            },
        ]);
        let out = render(&app, 100, 26);
        assert!(
            out.contains("index") && out.contains("mailer"),
            "tabs missing:\n{out}"
        );
    }

    #[test]
    fn survives_a_cramped_terminal() {
        // Layout must not panic when there is not enough room for the panels.
        let p = Program {
            name: "x".into(),
            feasibility_gap: 3.0,
            backlog_history: vec![1, 2],
            worker_history: vec![1],
            ..Default::default()
        };
        for (w, h) in [(20, 6), (40, 10), (5, 3), (200, 60)] {
            let _ = render(&app_with(vec![p.clone()]), w, h);
        }
    }

    #[test]
    fn the_exporter_and_this_parser_agree() {
        // The contract test that matters: render through the real exporter and
        // read it back through the real parser. A series renamed on one side
        // and not the other silently blanks a panel, and no fixture-based test
        // would notice.
        use crate::metrics::{Metrics, ProgramSnapshot};
        use std::sync::Arc;

        let snap = ProgramSnapshot {
            name: Arc::from("index"),
            workers: 10,
            backlog: 50_000,
            workers_needed: 78,
            workers_capacity: 24,
            feasibility_gap: 54,
            mu: 8.25,
            lambda: 200.5,
            mu_source: 2,
            probing: 1,
            scale_up_total: 12,
            scale_down_total: 3,
            probe_total: 2,
            worker_rss_bytes: 512 * 1024 * 1024,
            pool_rss_bytes: 5 * 1024 * 1024 * 1024,
            ram_budget_bytes: 12 * 1024 * 1024 * 1024,
            best_drain_seconds: -1.0,
            estimator_spread: 10.0,
            spawn_backoff_seconds: 0.0,
            slo_drain_seconds: 120.0,
        };
        let m = Metrics::default();
        m.set(vec![snap]);

        let mut progs = Vec::new();
        merge(&mut progs, parse_metrics(&m.render()));
        assert_eq!(progs.len(), 1);
        let p = &progs[0];

        assert_eq!(p.name, "index");
        assert_eq!(p.workers, 10.0);
        assert_eq!(p.backlog, 50_000.0);
        assert_eq!(p.workers_needed, 78.0);
        assert_eq!(p.workers_capacity, 24.0);
        assert_eq!(p.feasibility_gap, 54.0);
        assert_eq!(p.mu, 8.25);
        assert_eq!(p.lambda, 200.5);
        assert_eq!(p.mu_source, 2.0);
        assert_eq!(p.probing, 1.0);
        assert_eq!(p.worker_rss_bytes, (512 * 1024 * 1024) as f64);
        assert_eq!(p.pool_rss_bytes, (5u64 * 1024 * 1024 * 1024) as f64);
        assert_eq!(p.ram_budget_bytes, (12u64 * 1024 * 1024 * 1024) as f64);
        assert_eq!(p.best_drain_seconds, -1.0);
        assert_eq!(p.slo_drain_seconds, 120.0);
        assert_eq!(p.estimator_spread, 10.0);
        assert_eq!(p.spawn_backoff_seconds, 0.0);
        assert_eq!(p.scale_up_total, 12.0);
        assert_eq!(p.scale_down_total, 3.0);
        assert_eq!(p.probe_total, 2.0);

        // And it must survive being drawn.
        let out = render(&app_with(progs), 110, 34);
        assert!(out.contains("short by 54"), "{out}");
    }

    #[test]
    fn labels_the_estimator_source() {
        let mut p = Program::default();
        assert_eq!(p.source_label().0, "not measured");
        p.mu_source = 1.0;
        assert_eq!(p.source_label().0, "broker rates");
        p.mu_source = 2.0;
        assert_eq!(p.source_label().0, "regression");
    }
}
