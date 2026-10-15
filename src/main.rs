mod app;
mod aws;
mod describe;
mod learn;
mod logic;
mod sdk;
mod ui;
mod views;

use app::{App, Msg};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::sync::mpsc;
use std::time::Duration;

const USAGE: &str = "\
e9s: a k9s-style terminal UI for AWS (ECS, CloudWatch Logs, load balancers, autoscaling...)

USAGE
  e9s [flags]

WHERE
  -p, --profile NAME     AWS profile (default: AWS_PROFILE, the only profile, or a picker)
  -a, --account ID       the profile whose sso_account_id is ID
  -r, --region REGION    region (default: a scan of every region showing where resources are)
      --cluster NAME     open this ECS cluster's services
  -s, --service NAME     open this service's tasks (needs --cluster)

WHAT
  -c, --command CMD      open a view, like typing :CMD inside, e.g. -c rollout, -c tasks, -c 'logs /ecs/app'

HOW
      --refresh SECONDS  auto-refresh interval, 0 turns it off (default 10)
      --headless         hide the header
      --logoless         hide the logo
      --crumbsless       hide the breadcrumbs
      --readonly         accepted for k9s habits; e9s is always read-only
  -h, --help             this help

Press c inside to copy the command that reopens the view you are on, and ? for every key.";

fn fail(msg: &str) -> ! {
    eprintln!("e9s: {msg}\ntry --help");
    std::process::exit(2);
}

fn main() -> std::io::Result<()> {
    let mut start = app::Start { profile: std::env::var("AWS_PROFILE").ok().filter(|p| !p.is_empty()), ..Default::default() };
    let (mut account, mut script, mut chrome) = (None, None, ui::Chrome::default());
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().unwrap_or_else(|| fail(&format!("{name} needs a value")));
        match a.as_str() {
            "--profile" | "-p" => start.profile = Some(value("--profile")),
            "--account" | "-a" => account = Some(value("--account")),
            "--region" | "-r" => start.region = Some(value("--region")),
            "--cluster" => start.cluster = Some(value("--cluster")),
            "--service" | "-s" => start.service = Some(value("--service")),
            "--command" | "-c" => start.command = Some(value("--command")),
            "--refresh" => {
                let v = value("--refresh");
                start.refresh = Some(v.parse().unwrap_or_else(|_| fail(&format!("--refresh takes seconds, got {v}"))));
            }
            "--headless" => chrome.headless = true,
            "--logoless" => chrome.logoless = true,
            "--crumbsless" => chrome.crumbsless = true,
            "--script" => script = Some(value("--script")),
            // k9s flag; e9s is always read-only, so it changes nothing.
            "--readonly" => {}
            "--write" => fail("e9s is read-only and has no --write mode"),
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => fail(&format!("unknown argument {other}")),
        }
    }
    if let Some(acct) = account {
        match aws::profile_for_account(&acct) {
            Ok(p) => start.profile = Some(p),
            Err(e) => fail(&e),
        }
    }
    if start.service.is_some() && start.cluster.is_none() {
        fail("--service needs --cluster");
    }
    if let Some(cmd) = &start.command {
        let word = cmd.split_whitespace().next().unwrap_or("");
        if app::canonical(word).is_none() {
            fail(&format!("unknown command `{word}`; commands are the : commands listed in ? inside"));
        }
    }
    let (tx, rx) = mpsc::channel::<Msg>();
    let mut app = App::new(start, tx);
    app.chrome = chrome;
    if let Some(s) = script {
        return run_script(&mut app, &rx, &s);
    }
    let mut terminal = ratatui::init();
    let res = (|| -> std::io::Result<()> {
        while !app.quit {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        app.on_key(k);
                    }
                }
            }
            if let Some(l) = app.login.take() {
                // The CLI's own login, on the real terminal: it opens the browser and waits.
                ratatui::restore();
                let result = run_login(&l);
                terminal = ratatui::init();
                // Not terminal.clear(): it asks the terminal for the cursor position, and a terminal that
                // does not answer in time (a pipe, a slow ssh link) would end e9s right after the login.
                ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::terminal::Clear(ratatui::crossterm::terminal::ClearType::All))?;
                app.logged_in(&l, result);
            }
            while let Ok(m) = rx.try_recv() {
                app.on_msg(m);
            }
            app.tick();
        }
        Ok(())
    })();
    app.shutdown();
    ratatui::restore();
    res
}

/// `aws sso login` or `aws login`, the one thing e9s still asks the AWS CLI to do.
fn run_login(l: &app::Login) -> Result<(), String> {
    println!("e9s: aws {}  (e9s comes back when it finishes)\n", l.args.join(" "));
    match std::process::Command::new("aws").args(&l.args).status() {
        Ok(st) if st.success() => Ok(()),
        Ok(st) => Err(format!("exited with {st}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("the aws CLI is not installed; logging in needs it".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Headless run for checks without a tty: drives the real App and renderer on an in-memory screen.
/// Tokens: `@wait` (until the view loads), `@sleep` (2s), `@snap` (print the screen), `@size=WxH` (default 200x60),
/// `<Enter>` `<Esc>` `<Up>` `<Down>` `<Tab>` `<Space>` `<C-x>`, anything else is typed as keys.
fn run_script(app: &mut App, rx: &mpsc::Receiver<Msg>, script: &str) -> std::io::Result<()> {
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut term = ratatui::Terminal::new(TestBackend::new(200, 60)).unwrap();
    let pump = |app: &mut App, d: Duration| {
        let end = std::time::Instant::now() + d;
        while std::time::Instant::now() < end {
            while let Ok(m) = rx.try_recv() {
                app.on_msg(m);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    for tok in script.split_whitespace() {
        let key = |c: KeyCode, m: KeyModifiers| KeyEvent::new(c, m);
        match tok {
            "@wait" => {
                let t0 = std::time::Instant::now();
                pump(app, Duration::from_millis(50));
                for _ in 0..1200 {
                    if !app.top().loading {
                        break;
                    }
                    pump(app, Duration::from_millis(50));
                }
                eprintln!("@wait {}: {} ms", app.top().view.title(), t0.elapsed().as_millis());
                pump(app, Duration::from_millis(200));
            }
            "@sleep" => pump(app, Duration::from_secs(2)),
            "@cmd" => println!("launch: {:?}", app.launch_command()),
            t if t.starts_with("@size=") => match t[6..].split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))) {
                Some((w, h)) => term.backend_mut().resize(w, h),
                None => eprintln!("{t}: expected @size=<width>x<height>"),
            },
            t if t.starts_with("@shot=") => {
                let _ = term.draw(|f| ui::draw(f, app));
                let path = format!("{}.html", &t[6..]);
                std::fs::write(&path, buffer_html(term.backend().buffer()))?;
                println!("shot: {path}");
            }
            "@snap" => {
                let _ = term.draw(|f| ui::draw(f, app));
                let buf = term.backend().buffer();
                for y in 0..buf.area.height {
                    let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
                    println!("{}", line.trim_end());
                }
                println!("{}", "=".repeat(80));
            }
            "<Enter>" => app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            "<Esc>" => app.on_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            "<Up>" => app.on_key(key(KeyCode::Up, KeyModifiers::NONE)),
            "<Down>" => app.on_key(key(KeyCode::Down, KeyModifiers::NONE)),
            "<Space>" => app.on_key(key(KeyCode::Char(' '), KeyModifiers::NONE)),
            "<Tab>" => app.on_key(key(KeyCode::Tab, KeyModifiers::NONE)),
            t if t.starts_with("<C-") && t.len() == 5 => app.on_key(key(KeyCode::Char(t.as_bytes()[3] as char), KeyModifiers::CONTROL)),
            t => t.chars().for_each(|c| app.on_key(key(KeyCode::Char(c), KeyModifiers::NONE))),
        }
        // No browser here: report the login that would run.
        if let Some(l) = app.login.take() {
            println!("login: aws {}", l.args.join(" "));
        }
        let _ = term.draw(|f| ui::draw(f, app));
    }
    app.shutdown();
    Ok(())
}

/// The screen as HTML with the exact colours, for screenshots (`@shot=<path>` in --script).
fn buffer_html(buf: &ratatui::buffer::Buffer) -> String {
    use ratatui::style::{Color, Modifier};
    fn rgb(c: Color, default: &str) -> String {
        const BASE: [(u8, u8, u8); 16] = [
            (0, 0, 0), (205, 49, 49), (13, 188, 121), (229, 229, 16), (36, 114, 200), (188, 63, 188), (17, 168, 205), (229, 229, 229),
            (102, 102, 102), (241, 76, 76), (35, 209, 139), (245, 245, 67), (59, 142, 234), (214, 112, 214), (41, 184, 219), (255, 255, 255),
        ];
        let idx = |i: u8| -> (u8, u8, u8) {
            match i {
                0..=15 => BASE[i as usize],
                16..=231 => {
                    let i = i - 16;
                    let v = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
                    (v(i / 36), v(i / 6 % 6), v(i % 6))
                }
                _ => {
                    let g = 8 + (i - 232) * 10;
                    (g, g, g)
                }
            }
        };
        let (r, g, b) = match c {
            Color::Reset => return default.to_string(),
            Color::Black => idx(0), Color::Red => idx(1), Color::Green => idx(2), Color::Yellow => idx(3),
            Color::Blue => idx(4), Color::Magenta => idx(5), Color::Cyan => idx(6), Color::Gray => idx(7),
            Color::DarkGray => idx(8), Color::LightRed => idx(9), Color::LightGreen => idx(10), Color::LightYellow => idx(11),
            Color::LightBlue => idx(12), Color::LightMagenta => idx(13), Color::LightCyan => idx(14), Color::White => idx(15),
            Color::Indexed(i) => idx(i),
            Color::Rgb(r, g, b) => (r, g, b),
        };
        format!("#{r:02x}{g:02x}{b:02x}")
    }
    let mut out = String::from(
        "<!doctype html><meta charset=utf-8><style>body{margin:0;background:#1e1e1e}pre{margin:12px;font:13px/1.18 Menlo,monospace;color:#d4d4d4}i{font-style:normal;display:inline-block;width:1ch;overflow:hidden;vertical-align:top}</style><pre>",
    );
    for y in 0..buf.area.height {
        let mut x = 0;
        while x < buf.area.width {
            let c = &buf[(x, y)];
            let style = (c.fg, c.bg, c.modifier.contains(Modifier::BOLD));
            let mut text = String::new();
            while x < buf.area.width && (buf[(x, y)].fg, buf[(x, y)].bg, buf[(x, y)].modifier.contains(Modifier::BOLD)) == style {
                text.push_str(buf[(x, y)].symbol());
                x += 1;
            }
            // Non-ASCII glyphs (braille, box drawing) may come from a fallback font; pin each to one cell.
            let esc: String = text
                .chars()
                .map(|ch| match ch {
                    '&' => "&amp;".to_string(),
                    '<' => "&lt;".to_string(),
                    '>' => "&gt;".to_string(),
                    c if c.is_ascii() => c.to_string(),
                    c => format!("<i>{c}</i>"),
                })
                .collect();
            out.push_str(&format!(
                "<span style=\"color:{};background:{};font-weight:{}\">{esc}</span>",
                rgb(style.0, "#d4d4d4"),
                rgb(style.1, "transparent"),
                if style.2 { "bold" } else { "normal" }
            ));
        }
        out.push('\n');
    }
    out.push_str("</pre>");
    out
}
