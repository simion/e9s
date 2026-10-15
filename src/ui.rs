use crate::app::{completions, filter_re, App, Mode, COMMANDS, KEYS, QUICK};
use crate::logic::Health;
use crate::views::View;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

#[derive(Default, Clone, Copy)]
pub struct Chrome {
    pub headless: bool,
    pub logoless: bool,
    pub crumbsless: bool,
}

// k9s default skin.
const ORANGE: Color = Color::Indexed(214);
const AQUA: Color = Color::Indexed(51);
const DODGER: Color = Color::Indexed(33);
const FUCHSIA: Color = Color::Indexed(201);
const SKY: Color = Color::Indexed(117);
const GRAY: Color = Color::Indexed(245);

fn color(h: Health) -> Style {
    match h {
        Health::Ok => Style::new().fg(Color::Indexed(83)),
        Health::Warn => Style::new().fg(Color::Indexed(220)),
        Health::Bad => Style::new().fg(Color::Indexed(202)),
        Health::Dim => Style::new().fg(Color::Indexed(242)),
        Health::None => Style::new().fg(Color::Indexed(153)),
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let top = app.top();
    let mut notes: Vec<(Health, String)> = top.table.notes.iter().map(|n| (n.health, n.text.clone())).collect();
    if let Some(e) = &top.error {
        let stale = if top.table.rows.is_empty() { "" } else { " · rows below are from the LAST SUCCESSFUL refresh" };
        notes.insert(0, (Health::Bad, format!("REFRESH FAILED: {e}{stale}")));
    }
    // Notes wrap rather than run off the edge.
    let width = f.area().width.saturating_sub(1) as usize;
    let notes: Vec<(Health, String)> = notes.into_iter().flat_map(|(h, t)| wrap(&format!(" {t}"), width).into_iter().map(move |l| (h, l))).collect();
    let note_h = notes.len().min(12) as u16;
    let prompt_h = if matches!(app.mode, Mode::Command(_) | Mode::Filter(_) | Mode::Pattern(_)) { 3 } else { 0 };
    let [head, prompt_area, note_area, body, foot] = Layout::vertical([
        Constraint::Length(if app.chrome.headless || app.top().full { 0 } else { HEADER_H }),
        Constraint::Length(prompt_h),
        Constraint::Length(note_h),
        Constraint::Min(3),
        Constraint::Length(if (app.chrome.crumbsless || app.top().full) && app.flash.is_none() && !app.top().loading { 0 } else { 1 }),
    ])
    .areas(f.area());
    if head.height > 0 {
        header(f, app, head);
    }
    prompt(f, app, prompt_area);
    let lines: Vec<Line> = notes.into_iter().take(12).map(|(h, t)| Line::styled(t, note_style(h))).collect();
    f.render_widget(Paragraph::new(lines), note_area);
    let body = if app.top().table.panel.is_empty() {
        body
    } else {
        let [b, p] = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
        let ev: Vec<Line> = app.top().table.panel.iter().map(|(h, t)| Line::styled(t.clone(), color(*h))).collect();
        let block = Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(SKY))
            .title(Line::from(vec![Span::styled(" Events", Style::new().fg(AQUA).bold()), Span::styled("(newest first) ", Style::new().fg(FUCHSIA))]).centered());
        f.render_widget(Paragraph::new(ev).wrap(Wrap { trim: false }).block(block), p);
        b
    };
    app.page = body.height.saturating_sub(3) as usize;
    if app.top().view.is_text() {
        text_body(f, app, body)
    } else if matches!(app.top().view, View::Metrics { .. }) && !app.top().table.rows.is_empty() {
        metrics_body(f, app, body)
    } else {
        table_body(f, app, body)
    }
    footer(f, app, foot);
    suggestions(f, app, prompt_area);
    if let Mode::Help(off) = app.mode {
        let max = help(f, &app.top().view, off);
        if off > max {
            app.mode = Mode::Help(max);
        }
    }
    if let Mode::Calls(off) = app.mode {
        let max = calls(f, app.top().id, off);
        if off > max {
            app.mode = Mode::Calls(max);
        }
    }
}

/// Braille spinner driven by the clock; the main loop redraws every 100ms.
fn spinner() -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    FRAMES[(ms / 100 % 10) as usize]
}

/// One fixed-width slot: the spinner while busy, a blank otherwise, so nothing around it moves.
fn busy_slot(busy: bool) -> String {
    if busy { spinner().to_string() } else { " ".into() }
}

fn note_style(h: Health) -> Style {
    match h {
        Health::Bad => Style::new().fg(Color::Indexed(202)).bold(),
        Health::Warn => Style::new().fg(Color::Indexed(220)),
        Health::Ok => Style::new().fg(Color::Indexed(83)),
        _ => Style::new().fg(GRAY),
    }
}

/// Hard-wrap to `w` columns. Cells wrap instead of being cut, so no message loses a word.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = vec![];
    for part in text.split('\n') {
        let chars: Vec<char> = part.chars().collect();
        if chars.is_empty() {
            out.push(String::new());
        }
        out.extend(chars.chunks(w).map(|c| c.iter().collect::<String>()));
    }
    out
}

const HEADER_H: u16 = 7;

const LOGO: [&str; 6] = [
    r"  ____   ___   ____  ",
    r" / ___| / _ \ / ___| ",
    r" \___ \| (_) |\___ \ ",
    r"  ___) |\__, | ___) |",
    r" |____/   /_/ |____/ ",
    r"",
];

/// The keys that do something in this view, k9s-style.
fn actions(view: &View) -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&str, &str)> = match view {
        View::Profiles | View::Regions | View::Aliases => vec![("<enter>", "Select")],
        View::Clusters => vec![("<enter>", "Services")],
        View::Services { .. } => vec![("<enter>", "Tasks"), ("<R>", "Rollout"), ("<e>", "Events"), ("<F>", "Failures"), ("<l>", "Logs"), ("<m>", "Metrics")],
        View::Failures { .. } => vec![("<enter>", "Containers/Events"), ("<l>", "Logs"), ("<R>", "Rollout"), ("<e>", "Events")],
        View::Tasks { .. } => vec![("<enter>", "Containers"), ("<l>", "Logs"), ("<R>", "Rollout"), ("<e>", "Events"), ("<F>", "Failures")],
        View::Rollout { .. } => vec![("<enter>", "Containers"), ("<l>", "Logs"), ("<e>", "Events"), ("<m>", "Metrics")],
        View::Containers { .. } => vec![("<l>", "Logs")],
        View::Events { .. } => vec![("<R>", "Rollout"), ("<l>", "Logs"), ("<m>", "Metrics")],
        View::Logs { src, .. } => {
            let mut v = vec![("<0>", "Tail"), ("<1>", "Head"), ("<2-6>", "1m-1h"), ("<s>", "Autoscroll"), ("<f>", "Fullscreen"), ("</>", "Filter"), ("<p>", "Pattern")];
            if matches!(src, crate::views::LogSrc::Service { .. }) {
                v.push(("<a>", "All containers"));
            }
            v.extend([("<m>", "Mark"), ("<C>", "Clear"), ("<c>", "Copy"), ("<w>", "Wrap"), ("<t>", "Time"), ("<ctrl-s>", "Save")]);
            v
        }
        View::Detail { .. } => vec![("</>", "Search"), ("<n N>", "Next/Prev"), ("<w>", "Wrap"), ("<ctrl-s>", "Save")],
        View::TaskDefFamilies => vec![("<enter>", "Revisions")],
        View::TaskDefRevisions { .. } => vec![("<enter>", "Containers")],
        View::LogGroups => vec![("<enter>", "Streams"), ("<l>", "Logs")],
        View::LogStreams { .. } => vec![("<enter>", "Logs")],
        View::LoadBalancers => vec![("<enter>", "Target groups"), ("<m>", "Metrics")],
        View::TargetGroups { .. } => vec![("<enter>", "Targets"), ("<m>", "Metrics")],
        View::Targets { .. } => vec![("<m>", "Metrics")],
        View::ScalableTargets { .. } => vec![("<enter>", "Activities"), ("<p>", "Policies")],
        View::EcrRepos => vec![("<enter>", "Images")],
        View::Lambda => vec![("<l>", "Logs")],
        _ => vec![],
    };
    if !matches!(view, View::Detail { .. }) {
        v.extend([("<d>", "Describe"), ("<y>", "YAML")]);
    }
    v.extend([("<r>", "Refresh"), ("<esc>", "Back"), ("<?>", "Help")]);
    v
}

fn header(f: &mut Frame, app: &App, area: Rect) {
    let ctx = &app.aws.ctx;
    let top = app.top();
    let key = |k: &str| Span::styled(format!("{k:<10}"), Style::new().fg(Color::White).bold());
    let val = |v: String| Span::styled(v, Style::new().fg(ORANGE));
    let who = match (&app.arn, &app.ident_err) {
        _ if app.sso_expired => Span::styled(
            if ctx.profile.is_empty() { "SSO expired: aws sso login".to_string() } else { format!("SSO expired: aws sso login --profile {}", ctx.profile) },
            Style::new().fg(Color::White).bg(Color::Indexed(160)).bold(),
        ),
        (Some(arn), _) => {
            // The role, not the whole ARN: assumed-role/<role>/<session>.
            let who = arn.split_once(":assumed-role/").or_else(|| arn.split_once(":user/")).map(|x| x.1).unwrap_or(arn);
            let role = who.split('/').next().unwrap_or(who).to_string();
            if app.read_only_role() { val(role) } else { Span::styled(format!("{role} (may write; tool only reads)"), Style::new().fg(Color::Indexed(220)).bold()) }
        }
        (None, Some(e)) => Span::styled(e.clone(), Style::new().fg(Color::Indexed(202))),
        (None, None) if ctx.profile.is_empty() && matches!(top.view, View::Profiles) => val("-".into()),
        (None, None) => val(spinner().to_string()),
    };
    let refresh = if top.tail.is_some() {
        "live tail".to_string()
    } else {
        match app.refresh_in() {
            Some(s) => format!("{s:>2}s / {}s", app.interval),
            None if app.interval == 0 => "off".into(),
            None => "manual".into(),
        }
    };
    let info = vec![
        Line::from(vec![key("Profile:"), val(if ctx.profile.is_empty() { "(default chain)".into() } else { ctx.profile.clone() })]),
        Line::from(vec![key("Region:"), val(ctx.region.clone())]),
        Line::from(vec![key("Account:"), val(app.account.clone().unwrap_or_else(|| "-".into()))]),
        Line::from(vec![key("Role:"), who]),
        // Spinner slot first, always one column, so the countdown never shifts.
        Line::from(vec![
            key("Refresh:"),
            Span::styled(format!("{} ", busy_slot(top.loading || top.tail.is_some())), Style::new().fg(ORANGE).bold()),
            val(refresh),
        ]),
        Line::from(vec![key("Rev:"), val(format!("v{}", env!("CARGO_PKG_VERSION")))]),
    ];
    let info_w = info.iter().map(|l| l.width()).max().unwrap_or(0) as u16 + 4;
    let logo_w = if app.chrome.logoless { 0 } else { LOGO.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16 + 1 };
    let [a, b, c] = Layout::horizontal([Constraint::Length(info_w.max(34)), Constraint::Fill(1), Constraint::Length(logo_w)]).areas(area);
    f.render_widget(Paragraph::new(info), a);

    // Key hints: quick views (fuchsia) then this view's actions (dodger blue), in columns of HEADER_H rows.
    let quick: Vec<(String, String, Color)> = QUICK.iter().map(|(k, c)| (format!("<{k}>"), c.to_string(), FUCHSIA)).collect();
    let acts: Vec<(String, String, Color)> = actions(&top.view).into_iter().map(|(k, d)| (k.to_string(), d.to_string(), DODGER)).collect();
    let rows = HEADER_H as usize - 1;
    // Quick views and actions get their own columns, so their keys line up.
    let cols: Vec<&[(String, String, Color)]> = quick.chunks(rows).chain(acts.chunks(rows)).collect();
    let widths: Vec<(usize, usize)> = cols
        .iter()
        .map(|col| (col.iter().map(|x| x.0.chars().count()).max().unwrap_or(0), col.iter().map(|x| x.1.chars().count()).max().unwrap_or(0)))
        .collect();
    let mut lines = vec![];
    for r in 0..rows {
        let mut spans = vec![];
        let mut used = 0usize;
        for (col, (kw, dw)) in cols.iter().zip(&widths) {
            if used + kw + dw + 4 > b.width as usize {
                break;
            }
            let (k, d, c) = col.get(r).cloned().unwrap_or_default();
            spans.push(Span::styled(format!("{k:<kw$} "), Style::new().fg(c).bold()));
            spans.push(Span::styled(format!("{d:<dw$}   "), Style::new().fg(GRAY)));
            used += kw + dw + 4;
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), b);
    if logo_w > 0 && c.width >= logo_w {
        f.render_widget(Paragraph::new(LOGO.iter().map(|l| Line::styled(*l, Style::new().fg(ORANGE).bold())).collect::<Vec<_>>()), c);
    }
}

/// `services(my-cluster)` styled the k9s way: name aqua, argument fuchsia.
fn view_title(t: &str) -> Vec<Span<'static>> {
    match t.split_once('(') {
        Some((name, rest)) => vec![
            Span::styled(name.to_string(), Style::new().fg(AQUA).bold()),
            Span::styled("(", Style::new().fg(AQUA)),
            Span::styled(rest.trim_end_matches(')').to_string(), Style::new().fg(FUCHSIA).bold()),
            Span::styled(")", Style::new().fg(AQUA)),
        ],
        None => vec![Span::styled(t.to_string(), Style::new().fg(AQUA).bold())],
    }
}

fn title(app: &App) -> Line<'static> {
    let top = app.top();
    let vis = top.visible().len();
    let count = if vis == top.table.rows.len() { vis.to_string() } else { format!("{vis}/{}", top.table.rows.len()) };
    let mut spans = vec![Span::styled(format!(" {} ", busy_slot(top.loading || top.tail.is_some())), Style::new().fg(ORANGE).bold())];
    spans.extend(view_title(&top.view.title()));
    spans.push(Span::styled("[", Style::new().fg(AQUA)));
    spans.push(Span::styled(count, Style::new().fg(FUCHSIA).bold()));
    spans.push(Span::styled("]", Style::new().fg(AQUA)));
    if !top.filter.is_empty() {
        spans.push(Span::styled(format!(" </{}>", top.filter), Style::new().fg(FUCHSIA)));
    }
    if top.tail.is_some() {
        spans.push(Span::styled(" (live)", Style::new().fg(Color::Indexed(83)).bold()));
    }
    spans.push(Span::raw("   "));
    Line::from(spans)
}

/// Metrics as line charts: one panel per metric, one line per statistic, in a grid.
fn metrics_body(f: &mut Frame, app: &App, area: Rect) {
    use ratatui::symbols::Marker;
    use ratatui::widgets::{Axis, Chart, Dataset, GraphType};
    let block = Block::new().borders(Borders::ALL).border_style(Style::new().fg(SKY)).title(title(app).centered());
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Group rows by metric label, keeping first-seen order: Average and Maximum share a panel.
    let mut groups: Vec<(String, Vec<(String, Vec<f64>)>)> = vec![];
    for r in &app.top().table.rows {
        let label = r.json["label"].as_str().unwrap_or("").to_string();
        let stat = r.json["stat"].as_str().unwrap_or("").to_string();
        let vals: Vec<f64> = r.json["values"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
        match groups.iter_mut().find(|g| g.0 == label) {
            Some(g) => g.1.push((stat, vals)),
            None => groups.push((label, vec![(stat, vals)])),
        }
    }
    let cols = if inner.width >= 120 && groups.len() > 1 { 2 } else { 1 };
    let nrows = groups.len().div_ceil(cols);
    let row_areas = Layout::vertical(vec![Constraint::Ratio(1, nrows as u32); nrows]).split(inner);
    const LINE_COLORS: [Color; 3] = [AQUA, ORANGE, FUCHSIA];
    for (gi, (label, series)) in groups.iter().enumerate() {
        let cell = Layout::horizontal(vec![Constraint::Ratio(1, cols as u32); cols]).split(row_areas[gi / cols])[gi % cols];
        let latest: Vec<Span> = series
            .iter()
            .enumerate()
            .flat_map(|(i, (stat, vals))| {
                let v = vals.last().map(|x| fmt_metric(*x)).unwrap_or_else(|| "no data".into());
                [Span::styled(format!(" {} ", stat.to_lowercase()), Style::new().fg(GRAY)), Span::styled(v, Style::new().fg(LINE_COLORS[i % 3]).bold())]
            })
            .collect();
        let panel = Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::Indexed(240)))
            .title(Line::styled(format!(" {label} "), Style::new().fg(Color::White).bold()))
            .title(Line::from(latest).right_aligned());
        let points: Vec<Vec<(f64, f64)>> = series.iter().map(|(_, v)| v.iter().enumerate().map(|(i, y)| (i as f64, *y)).collect()).collect();
        let len = series.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
        if len == 0 {
            f.render_widget(Paragraph::new("  no datapoints in the last 3 hours").fg(GRAY).block(panel), cell);
            continue;
        }
        let hi = series.iter().flat_map(|(_, v)| v.iter().copied()).fold(f64::MIN, f64::max);
        let (y_lo, y_hi) = if label.contains('%') { (0.0, 100f64.max(hi)) } else { (0.0, if hi > 0.0 { hi * 1.15 } else { 1.0 }) };
        let datasets: Vec<Dataset> = series
            .iter()
            .zip(&points)
            .enumerate()
            .map(|(i, ((stat, _), pts))| {
                Dataset::default().name(stat.to_lowercase()).marker(Marker::Braille).graph_type(GraphType::Line).style(Style::new().fg(LINE_COLORS[i % 3])).data(pts)
            })
            .collect();
        let span_min = (len.saturating_sub(1) * 5) as i64;
        let x_labels = vec![
            Span::styled(format!("-{}", crate::logic::fmt_dur(span_min * 60)), Style::new().fg(GRAY)),
            Span::styled(format!("-{}", crate::logic::fmt_dur(span_min * 30)), Style::new().fg(GRAY)),
            Span::styled("now", Style::new().fg(GRAY)),
        ];
        let y_labels = vec![
            Span::styled(fmt_metric(y_lo), Style::new().fg(GRAY)),
            Span::styled(fmt_metric((y_lo + y_hi) / 2.0), Style::new().fg(GRAY)),
            Span::styled(fmt_metric(y_hi), Style::new().fg(GRAY)),
        ];
        let chart = Chart::new(datasets)
            .block(panel)
            .x_axis(Axis::default().bounds([0.0, (len.max(2) - 1) as f64]).labels(x_labels).style(Style::new().fg(Color::Indexed(240))))
            .y_axis(Axis::default().bounds([y_lo, y_hi]).labels(y_labels).style(Style::new().fg(Color::Indexed(240))))
            .legend_position(None);
        f.render_widget(chart, cell);
    }
}

fn fmt_metric(v: f64) -> String {
    if v.abs() >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if v.abs() >= 1000.0 {
        format!("{:.1}k", v / 1000.0)
    } else if (v - v.round()).abs() < 1e-9 {
        format!("{}", v as i64)
    } else if v.abs() < 1.0 {
        format!("{v:.3}")
    } else {
        format!("{v:.1}")
    }
}

fn table_body(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::new().borders(Borders::ALL).border_style(Style::new().fg(SKY)).title(title(app).centered());
    let inner = block.inner(area);
    let top = app.top();
    let vis = top.visible();
    let rows = &top.table.rows;
    let ncol = top.table.headers.len();
    let detail = top.table.detail.filter(|&d| d < ncol);
    let cols: Vec<usize> = (0..ncol).filter(|&c| Some(c) != detail).collect();
    let cell = |i: usize, c: usize| rows[i].cells.get(c).map(String::as_str).unwrap_or("");
    let avail = inner.width as usize;
    let mut widths: Vec<usize> = cols.iter().map(|&c| top.table.headers[c].chars().count() + 2).collect();
    for &i in &vis {
        for (k, &c) in cols.iter().enumerate() {
            widths[k] = widths[k].max(cell(i, c).chars().count().min(avail));
        }
    }
    // The last grid column takes what is left. Without a detail line it is the explanation column,
    // so it keeps about a quarter of the width. Narrow the widest columns to fit; cells wrap, never cut.
    let n = cols.len();
    let last_min = match widths.last() {
        Some(&w) if detail.is_none() && n > 3 => (avail / 4).max(20).min(w.max(20)),
        Some(&w) => w.min(20),
        None => 0, // first frame: nothing loaded yet, no columns
    };
    while n > 1 && widths[..n - 1].iter().map(|w| w + 1).sum::<usize>() + last_min > avail {
        let (i, w) = widths[..n - 1].iter().enumerate().max_by_key(|x| *x.1).map(|(i, w)| (i, *w)).unwrap();
        if w <= 6 {
            break;
        }
        widths[i] -= 1;
    }
    let used: usize = widths[..n.saturating_sub(1)].iter().map(|w| w + 1).sum();
    if let Some(last) = widths.last_mut() {
        *last = avail.saturating_sub(used).max(4);
    }
    let constraints: Vec<Constraint> = widths.iter().map(|&w| Constraint::Length(w as u16)).collect();
    let header = Row::new(cols.iter().map(|&c| {
        let h = &top.table.headers[c];
        match top.sort {
            Some((sc, desc)) if sc == c => format!("{h}{}", if desc { "↓" } else { "↑" }),
            _ => h.clone(),
        }
    }))
    .style(Style::new().fg(Color::White).bold());
    // Each row: its grid cells, then its detail text as full-width lines below.
    let detail_w = avail.saturating_sub(4);
    let details: Vec<Vec<String>> = vis
        .iter()
        .map(|&i| match detail.map(|d| cell(i, d)) {
            Some(t) if !t.is_empty() => wrap(t, detail_w),
            _ => vec![],
        })
        .collect();
    let mut mains = vec![];
    let body: Vec<Row> = vis
        .iter()
        .zip(&details)
        .map(|(&i, det)| {
            let cells: Vec<Vec<String>> = cols.iter().enumerate().map(|(k, &c)| wrap(cell(i, c), widths[k])).collect();
            let h = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
            mains.push(h);
            Row::new(cells.into_iter().map(|l| l.join("\n"))).height((h + det.len()) as u16).style(color(rows[i].health))
        })
        .collect();
    let empty = body.is_empty();
    let table = Table::new(body, constraints).header(header).block(block).row_highlight_style(Style::new().fg(Color::Black).bg(AQUA)).column_spacing(1);
    let mut state = TableState::default().with_selected(Some(top.sel));
    f.render_stateful_widget(table, area, &mut state);
    // Draw the detail lines into the space each row left for them.
    let bottom = inner.y + inner.height;
    let mut y = inner.y + 1;
    for pos in state.offset()..vis.len() {
        if y >= bottom {
            break;
        }
        let st = if pos == top.sel { Style::new().fg(Color::Black).bg(AQUA) } else { color(rows[vis[pos]].health) };
        for (j, l) in details[pos].iter().enumerate() {
            let ly = y + (mains[pos] + j) as u16;
            if ly >= bottom {
                break;
            }
            let prefix = if j == 0 { "  ↳ " } else { "    " };
            f.render_widget(Paragraph::new(Line::styled(format!("{prefix}{l}"), st)), Rect { x: inner.x, y: ly, width: inner.width, height: 1 });
        }
        y += (mains[pos] + details[pos].len()) as u16;
    }
    if empty {
        let inner = Rect { x: area.x + 2, y: area.y + 2, width: area.width.saturating_sub(4), height: area.height.saturating_sub(3) };
        if top.loading {
            f.render_widget(Paragraph::new(inflight_lines(top.id, inner.height as usize)), inner);
        } else {
            let msg = if top.error.is_some() {
                "failed: see the error above".to_string()
            } else {
                top.table.empty.clone().unwrap_or_else(|| format!("no {} in {}", top.view.title(), app.aws.ctx.region))
            };
            f.render_widget(Paragraph::new(msg).fg(GRAY), inner);
        }
    }
}

/// A stable colour per task, so lines from one task read as a group (k9s colours pod names).
fn task_colour(id: &str) -> Color {
    const PALETTE: [u8; 8] = [51, 117, 201, 214, 83, 141, 228, 210];
    let h = id.bytes().fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
    Color::Indexed(PALETTE[h % PALETTE.len()])
}

fn log_style(msg: &str) -> Style {
    let u = msg.to_uppercase();
    if ["ERROR", "CRITICAL", "TRACEBACK", "EXCEPTION", "FATAL"].iter().any(|k| u.contains(k)) {
        Style::new().fg(Color::Red)
    } else if u.contains("WARN") {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new()
    }
}

fn text_body(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::new().borders(Borders::ALL).border_style(Style::new().fg(SKY)).title(title(app).centered());
    let mut inner = block.inner(area);
    f.render_widget(block, area);
    let frame = app.stack.last_mut().unwrap();
    let is_logs = matches!(frame.view, View::Logs { .. });
    // k9s's log indicator: the toggles' state, centred above the lines.
    if is_logs && inner.height > 1 {
        let toggle = |name: &str, on: bool, key: &str| -> Vec<Span<'static>> {
            vec![
                Span::styled(format!("{name}:"), Style::new().fg(Color::White).bold()),
                if on { Span::styled("On", Style::new().fg(Color::Indexed(83)).bold()) } else { Span::styled("Off", Style::new().fg(GRAY)) },
                Span::styled(format!(" <{key}>     "), Style::new().fg(DODGER)),
            ]
        };
        let View::Logs { src, opts, .. } = &frame.view else { unreachable!() };
        let mut spans = vec![
            Span::styled("Since:", Style::new().fg(Color::White).bold()),
            Span::styled(opts.since.label(), Style::new().fg(ORANGE).bold()),
            Span::styled(" <0-6>     ", Style::new().fg(DODGER)),
        ];
        if matches!(src, crate::views::LogSrc::Service { .. }) {
            spans.extend(toggle("AllContainers", opts.all, "a"));
        }
        spans.extend(toggle("Autoscroll", frame.tail.is_some(), "s"));
        spans.extend(toggle("FullScreen", frame.full, "f"));
        spans.extend(toggle("Wrap", frame.wrap, "w"));
        spans.extend(toggle("Timestamps", frame.show_time, "t"));
        f.render_widget(Paragraph::new(Line::from(spans)).centered(), Rect { height: 1, ..inner });
        inner.y += 1;
        inner.height -= 1;
    }
    let (h, w) = (inner.height as usize, inner.width.max(1) as usize);
    let vis = frame.visible();
    // A filter's matches are highlighted in logs, as in k9s (not for an inverted filter).
    let hl = filter_re(&frame.filter).filter(|(_, inv)| !inv).map(|(r, _)| r);
    // Each row as styled characters: time, the task tag in its own colour, the message.
    let styled = |i: usize, selected: bool| -> Vec<(char, Style)> {
        let c = &frame.table.rows[i].cells;
        let mut out: Vec<(char, Style)> = vec![];
        fn push(out: &mut Vec<(char, Style)>, t: &str, st: Style) {
            out.extend(t.chars().map(|ch| (ch, st)));
        }
        // Paint the filter's matches in `text`, which starts at `at` in `out`.
        let mark = |out: &mut Vec<(char, Style)>, at: usize, text: &str| {
            let Some(re) = &hl else { return };
            let starts: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
            for m in re.find_iter(text) {
                for (k, &b) in starts.iter().enumerate() {
                    if b >= m.start() && b < m.end() {
                        out[at + k].1 = Style::new().fg(Color::Black).bg(ORANGE);
                    }
                }
            }
        };
        if !is_logs {
            push(&mut out, &c[0], Style::new());
            mark(&mut out, 0, &c[0]);
            return out;
        }
        if frame.table.rows[i].json.as_str() == Some(crate::app::MARK) {
            push(&mut out, &"─".repeat(w), Style::new().fg(ORANGE).bold());
            return out;
        }
        let base = log_style(&c[2]);
        if frame.show_time {
            push(&mut out, &format!("{} ", c[0]), Style::new().fg(GRAY));
        }
        if !c[1].is_empty() {
            push(&mut out, &format!("{} ", c[1]), Style::new().fg(task_colour(&c[1])));
        }
        let msg_at = out.len();
        push(&mut out, &c[2], base);
        mark(&mut out, msg_at, &c[2]);
        if selected {
            for x in out.iter_mut().filter(|x| x.1.bg != Some(ORANGE)) {
                x.1 = x.1.bg(Color::Indexed(238));
            }
        }
        out
    };
    // Split into screen lines (wrap, embedded newlines), then merge runs of one style into spans.
    let split = |chars: Vec<(char, Style)>| -> Vec<Line<'static>> {
        let mut rows: Vec<Vec<(char, Style)>> = vec![vec![]];
        for (ch, st) in chars {
            match ch {
                '\n' => rows.push(vec![]),
                '\t' => rows.last_mut().unwrap().extend(std::iter::repeat_n((' ', st), 4)),
                _ => rows.last_mut().unwrap().push((ch, st)),
            }
        }
        let rows: Vec<Vec<(char, Style)>> = if frame.wrap {
            rows.into_iter().flat_map(|r| if r.is_empty() { vec![r] } else { r.chunks(w).map(<[_]>::to_vec).collect() }).collect()
        } else {
            rows
        };
        rows.into_iter()
            .map(|r| {
                let mut spans: Vec<Span> = vec![];
                let mut cur = String::new();
                let mut st = r.first().map(|x| x.1).unwrap_or_default();
                for (ch, s2) in r {
                    if s2 != st && !cur.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut cur), st));
                    }
                    st = s2;
                    cur.push(ch);
                }
                spans.push(Span::styled(cur, st));
                Line::from(spans)
            })
            .collect()
    };
    if vis.is_empty() {
        if frame.loading {
            f.render_widget(Paragraph::new(inflight_lines(frame.id, inner.height as usize)), inner);
        } else {
            let msg = if frame.tail.is_some() { "waiting for events".to_string() } else { frame.table.empty.clone().unwrap_or_else(|| "no lines".into()) };
            f.render_widget(Paragraph::new(msg).fg(GRAY), inner);
        }
        return;
    }
    let sel = frame.sel.min(vis.len() - 1);
    // Keep the previous offset unless the cursor's line no longer fits below it: then the
    // lowest offset whose lines up to the cursor fit, counted back from the cursor once.
    let height = |i: usize| split(styled(vis[i], false)).len();
    let mut offset = frame.offset.min(sel);
    let (mut lowest, mut used) = (sel, height(sel));
    while lowest > offset && used + height(lowest - 1) <= h {
        lowest -= 1;
        used += height(lowest);
    }
    if lowest > offset {
        offset = lowest;
    }
    // Never leave space below the last line while earlier lines could fill it (after a filter).
    let (mut fill, mut below) = (vis.len() - 1, height(vis.len() - 1));
    while fill > 0 && below + height(fill - 1) <= h {
        fill -= 1;
        below += height(fill);
    }
    offset = offset.min(fill);
    let mut lines: Vec<Line> = vec![];
    for (pos, &i) in vis.iter().enumerate().skip(offset) {
        if lines.len() >= h {
            break;
        }
        // Describe pages have no cursor (as in k9s); logs keep a bar to show where you are.
        lines.extend(split(styled(i, pos == sel && is_logs)));
    }
    lines.truncate(h);
    frame.offset = offset;
    f.render_widget(Paragraph::new(lines), inner);
}

/// k9s prompt box, with the completion greyed after the cursor.
fn prompt(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let (icon, text, hint) = match &app.mode {
        Mode::Command(s) => {
            let rest = completions(s).first().filter(|c| c.len() > s.len() && !s.is_empty()).map(|c| c[s.len()..].to_string()).unwrap_or_default();
            (":", s.clone(), rest)
        }
        Mode::Filter(s) => ("/", s.clone(), String::new()),
        Mode::Pattern(s) => ("pattern: ", s.clone(), String::new()),
        _ => return,
    };
    let line = Line::from(vec![
        Span::styled(icon, Style::new().fg(AQUA).bold()),
        Span::styled(text, Style::new().fg(Color::White)),
        Span::styled("█", Style::new().fg(AQUA)),
        Span::styled(hint, Style::new().fg(GRAY)),
    ]);
    f.render_widget(Paragraph::new(line).block(Block::new().borders(Borders::ALL).border_style(Style::new().fg(AQUA))), area);
}

/// Command mode: the suggestions for what was typed, under the prompt, Up/Down's pick highlighted (k9s).
fn suggestions(f: &mut Frame, app: &App, prompt: Rect) {
    let Mode::Command(s) = &app.mode else { return };
    let (prefix, pick) = app.pick.clone().map_or((s.clone(), None), |(p, i)| (p, Some(i)));
    if prefix.trim().is_empty() || prompt.height == 0 {
        return;
    }
    let list = completions(&prefix);
    let room = f.area().height.saturating_sub(prompt.y + prompt.height + 2) as usize;
    let start = pick.map_or(0, |i| (i + 1).saturating_sub(room.min(8)));
    let shown: Vec<Line> = list
        .iter()
        .enumerate()
        .skip(start)
        .take(room.min(8))
        .map(|(i, a)| {
            let what = COMMANDS.iter().find(|(x, _)| x.contains(a)).map(|(_, d)| *d).unwrap_or("");
            let st = if Some(i) == pick { Style::new().fg(Color::Black).bg(AQUA) } else { Style::new().fg(Color::White) };
            Line::from(vec![Span::styled(format!(" {a:<14}"), st), Span::styled(format!(" {what}"), Style::new().fg(GRAY))])
        })
        .collect();
    if shown.is_empty() {
        return;
    }
    let w = shown.iter().map(|l| l.width()).max().unwrap_or(0) as u16 + 2;
    let r = Rect { x: prompt.x + 1, y: prompt.y + prompt.height, width: w.min(prompt.width.saturating_sub(1)), height: shown.len() as u16 + 2 };
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(shown).block(Block::new().borders(Borders::ALL).border_style(Style::new().fg(GRAY))), r);
}

/// One call of the command log: a spinner while it runs, then a tick or a cross, and its time.
fn call_line((cmd, t, ok, _): &(String, std::time::Duration, Option<bool>, std::time::Instant)) -> Line<'static> {
    let (mark, st) = match ok {
        None => (spinner(), Style::new().fg(ORANGE).bold()),
        Some(true) => ('✓', Style::new().fg(Color::Indexed(83))),
        Some(false) => ('✗', Style::new().fg(Color::Indexed(202))),
    };
    Line::from(vec![
        Span::styled(format!("{mark} "), st),
        Span::styled(cmd.clone(), Style::new().fg(if ok.is_none() { Color::White } else { GRAY })),
        Span::styled(format!("  {:.1}s", t.as_secs_f32()), Style::new().fg(GRAY)),
    ])
}

/// While a screen loads: its running calls, then the ones already finished below them.
fn inflight_lines(screen: u64, max: usize) -> Vec<Line<'static>> {
    let calls = crate::aws::screen_log(screen);
    let mut lines: Vec<Line> = calls.iter().take(max.saturating_sub(1).max(1)).map(call_line).collect();
    if calls.len() > lines.len() {
        lines.push(Line::styled(format!("  … and {} more (H lists them all)", calls.len() - lines.len()), Style::new().fg(GRAY)));
    }
    if lines.is_empty() {
        lines.push(Line::styled(format!("{} working", spinner()), Style::new().fg(GRAY)));
    }
    lines
}

/// `H`: every AWS CLI call this screen made, running first, then newest first, with when it started.
fn calls(f: &mut Frame, screen: u64, off: u16) -> u16 {
    let r = f.area();
    let log = crate::aws::screen_log(screen);
    let mut lines: Vec<Line> = log
        .iter()
        .map(|c| {
            let mut l = call_line(c);
            l.spans.insert(0, Span::styled(format!(" {:>6} ago  ", fmt_ago(c.3.elapsed())), Style::new().fg(GRAY)));
            l
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::styled(" no AWS calls on this screen yet", Style::new().fg(GRAY)));
    }
    let max = (lines.len() as u16).saturating_sub(r.height.saturating_sub(2));
    let off = off.min(max);
    let more = if max == 0 { String::new() } else { format!(" {}/{} ", off, max) };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(SKY))
        .title(Line::styled(format!(" Commands ({}) ", log.len()), Style::new().fg(AQUA).bold()).centered())
        .title_bottom(Line::from(vec![
            Span::styled(" ↑↓ j k ", Style::new().fg(DODGER).bold()),
            Span::styled("scroll ", Style::new().fg(GRAY)),
            Span::styled(" esc ", Style::new().fg(DODGER).bold()),
            Span::styled("close ", Style::new().fg(GRAY)),
            Span::styled(more, Style::new().fg(FUCHSIA)),
        ]).centered());
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).scroll((off, 0)).block(block), r);
    max
}

fn fmt_ago(d: std::time::Duration) -> String {
    match d.as_secs() {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{}s", s / 60, s % 60),
        s => format!("{}h{}m", s / 3600, s / 60 % 60),
    }
}

/// Bottom line: breadcrumb pills, then the flash message.
fn footer(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![];
    for (i, fr) in app.stack.iter().enumerate().filter(|_| !app.chrome.crumbsless) {
        let name = fr.view.title();
        let name = name.split('(').next().unwrap_or(&name).to_string();
        let bg = if i + 1 == app.stack.len() { ORANGE } else { SKY };
        spans.push(Span::styled(format!(" <{name}> "), Style::new().fg(Color::Black).bg(bg).bold()));
        spans.push(Span::raw(" "));
    }
    let idle = app.flash.is_none() && !(app.top().loading && !app.top().table.rows.is_empty());
    if idle {
        spans.push(Span::styled(" <?> explains this screen in AWS and Kubernetes terms", Style::new().fg(GRAY)));
    }
    if let Some((m, bad, _)) = &app.flash {
        spans.push(Span::styled(format!("  {m}"), Style::new().fg(if *bad { Color::Indexed(202) } else { Color::Indexed(83) })));
    } else if app.top().loading && !app.top().table.rows.is_empty() {
        // A refresh over rows already on screen: name the call in flight.
        let calls = crate::aws::inflight();
        if let Some((cmd, t)) = calls.first() {
            let more = if calls.len() > 1 { format!("  (+{} more)", calls.len() - 1) } else { String::new() };
            spans.push(Span::styled(format!(" {} {cmd}  {:.1}s{more}", spinner(), t.as_secs_f32()), Style::new().fg(GRAY)));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Flow `items` (a heading line, then entries) into as many equal columns as fit the width, column by column.
fn flow(items: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut items = items.into_iter();
    let heading = items.next().unwrap_or_default();
    let entries: Vec<Line> = items.collect();
    let col_w = entries.iter().map(|l| l.width()).max().unwrap_or(1) + 3;
    let ncols = (width / col_w).clamp(1, entries.len().max(1));
    let rows = entries.len().div_ceil(ncols);
    let mut out = vec![heading];
    for r in 0..rows {
        let mut spans = vec![];
        for c in 0..ncols {
            if let Some(l) = entries.get(c * rows + r) {
                let pad = col_w - l.width();
                spans.extend(l.spans.clone());
                spans.push(Span::raw(" ".repeat(pad)));
            }
        }
        out.push(Line::from(spans));
    }
    out
}

/// Scrollable full-screen help: what this view is, then keys and commands flowed into columns.
/// Returns the largest useful scroll offset.
fn help(f: &mut Frame, view: &View, off: u16) -> u16 {
    let area = f.area();
    let r = Rect { x: 0, y: 0, width: area.width, height: area.height };
    let key = |k: &str, d: &str, kw: usize| Line::from(vec![Span::styled(format!("  {k:<kw$}  "), Style::new().fg(DODGER).bold()), Span::styled(d.to_string(), Style::new().fg(GRAY))]);
    let mut keys = vec![Line::styled("  KEYS", Style::new().fg(AQUA).bold())];
    keys.extend(KEYS.iter().map(|(k, d)| key(k, d, 14)));
    let mut cmds = vec![Line::styled("  COMMANDS", Style::new().fg(AQUA).bold())];
    cmds.extend(COMMANDS.iter().map(|(a, d)| key(&format!(":{}", a.join(" :")), d, 26)));
    let inner_w = r.width.saturating_sub(2);
    // What this screen is, in AWS terms and in Kubernetes terms, wrapped to the box.
    let (topic, about) = crate::learn::about(view);
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled("  ABOUT THIS VIEW: ", Style::new().fg(AQUA).bold()),
        Span::styled(topic, Style::new().fg(ORANGE).bold()),
    ])];
    for para in about {
        for l in wrap(para, inner_w.saturating_sub(4) as usize) {
            lines.push(Line::styled(format!("  {l}"), Style::new().fg(Color::White)));
        }
    }
    lines.push(Line::raw(""));
    lines.extend(flow(keys, inner_w as usize));
    lines.push(Line::raw(""));
    lines.extend(flow(cmds, inner_w as usize));
    let max = (lines.len() as u16).saturating_sub(r.height.saturating_sub(2));
    let off = off.min(max);
    let more = if max == 0 { String::new() } else { format!(" {}/{} ", off, max) };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(SKY))
        .title(Line::styled(" Help ", Style::new().fg(AQUA).bold()).centered())
        .title_bottom(Line::from(vec![
            Span::styled(" ↑↓ j k ", Style::new().fg(DODGER).bold()),
            Span::styled("scroll ", Style::new().fg(GRAY)),
            Span::styled(" esc ", Style::new().fg(DODGER).bold()),
            Span::styled("close ", Style::new().fg(GRAY)),
            Span::styled(more, Style::new().fg(FUCHSIA)),
        ]).centered());
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).scroll((off, 0)).block(block), r);
    max
}
