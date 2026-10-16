//! Drawing: one panel per server (or one line per server in the compact view), plus a
//! per-user summary across all of them. Everything is laid out as lines of styled spans
//! so heights are known up front, which is what scrolling and auto-compact need.

use std::collections::BTreeMap;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};
use serde_json::Value;

use crate::state::View;

// ---------------------------------------------------------------- values

pub fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

pub fn text<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub fn gib(mib: Option<f64>) -> String {
    mib.map_or("-".into(), |m| format!("{:.1}G", m / 1024.0))
}

pub fn fmt_age(sec: f64) -> String {
    let s = sec.max(0.0) as u64;
    match s {
        s if s >= 86400 => format!("{}d{}h", s / 86400, s % 86400 / 3600),
        s if s >= 3600 => format!("{}h{}m", s / 3600, s % 3600 / 60),
        s => format!("{}m", s / 60),
    }
}

pub fn short_gpu_name(name: &str) -> String {
    name.replace("NVIDIA ", "")
        .replace("GeForce ", "")
        .replace("Tesla ", "")
}

/// A display server (Xorg, gnome-shell) parked on the GPU: graphics-only and a few MiB.
pub fn is_display_proc(p: &Value) -> bool {
    text(p, "type") == Some("G") && num(p, "mem").unwrap_or(0.0) < 64.0
}

pub fn busy_procs(g: &Value) -> Vec<&Value> {
    arr(g, "procs")
        .iter()
        .filter(|p| !is_display_proc(p))
        .collect()
}

/// No jobs, under 1 GiB used, under 5% load. Unknown memory is never called idle.
pub fn gpu_is_idle(g: &Value) -> bool {
    if g["invalid"].as_bool().unwrap_or(false) || !busy_procs(g).is_empty() {
        return false;
    }
    match num(g, "mem_used") {
        Some(m) => m < 1024.0 && num(g, "util").unwrap_or(0.0) < 5.0,
        None => false,
    }
}

/// user, or user@container (short id) for dockerized jobs
pub fn proc_owner(p: &Value) -> String {
    let user = text(p, "user").unwrap_or("?");
    match text(p, "container") {
        Some(c) => format!("{}@{}", user, &c[..c.len().min(6)]),
        None => user.to_string(),
    }
}

/// Seconds since a GPU process started, if the agent sent its start time.
pub fn proc_age(p: &Value, d: &Value) -> Option<f64> {
    Some(num(d, "uptime")? - num(p, "start")? / num(d, "clk_tck")?)
}

pub fn user_ram_kb(u: &Value) -> f64 {
    num(u, "mem_kb").or_else(|| num(u, "rss_kb")).unwrap_or(0.0)
}

// ---------------------------------------------------------------- style

const USER_COLORS: [Color; 14] = [
    Color::Cyan,
    Color::Magenta,
    Color::LightBlue,
    Color::LightGreen,
    Color::Indexed(214), // orange1
    Color::Indexed(170), // orchid
    Color::Indexed(45),  // turquoise2
    Color::Indexed(220), // gold1
    Color::Indexed(209), // salmon1
    Color::Indexed(47),  // spring_green2
    Color::Indexed(39),  // deep_sky_blue1
    Color::Indexed(205), // hot_pink
    Color::Indexed(228), // khaki1
    Color::Indexed(219), // plum1
];
const GREY: Color = Color::Indexed(244);
const DARK: Color = Color::Indexed(240);

fn dim() -> Style {
    Style::new().fg(GREY)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub fn user_style(user: &str, me: &str) -> Style {
    let mut crc = flate2::Crc::new();
    crc.update(user.as_bytes());
    let s = Style::new().fg(USER_COLORS[crc.sum() as usize % USER_COLORS.len()]);
    if user == me {
        s.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        s
    }
}

fn level(pct: Option<f64>, warn: f64, crit: f64) -> Style {
    match pct {
        None => dim(),
        Some(p) if p >= crit => Style::new().fg(Color::Red),
        Some(p) if p >= warn => Style::new().fg(Color::Yellow),
        Some(_) => Style::new().fg(Color::Green),
    }
}

fn bar(pct: Option<f64>, width: usize) -> Vec<Span<'static>> {
    match pct {
        None => vec![Span::styled("─".repeat(width), dim())],
        Some(p) => {
            let filled = ((p.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
            vec![
                Span::styled("█".repeat(filled), level(pct, 50.0, 85.0)),
                Span::styled("░".repeat(width - filled), Style::new().fg(DARK)),
            ]
        }
    }
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.width()).sum()
}

/// Fit spans into `w` columns: cut with "…" or pad with spaces (right-aligned if `right`).
fn fit(mut spans: Vec<Span<'static>>, w: usize, right: bool) -> Vec<Span<'static>> {
    let total = width_of(&spans);
    if total > w {
        let mut out = Vec::new();
        let mut used = 0;
        for s in spans {
            let sw = s.width();
            if used + sw < w {
                used += sw;
                out.push(s);
                continue;
            }
            let mut cut = String::new();
            for ch in s.content.chars() {
                let cw = unicode_width(ch);
                if used + cw + 1 > w {
                    break;
                }
                used += cw;
                cut.push(ch);
            }
            cut.push('…');
            used += 1;
            out.push(Span::styled(cut, s.style));
            break;
        }
        if used < w {
            out.push(Span::raw(" ".repeat(w - used)));
        }
        return out;
    }
    let pad = Span::raw(" ".repeat(w - total));
    if right {
        spans.insert(0, pad);
    } else {
        spans.push(pad);
    }
    spans
}

fn unicode_width(c: char) -> usize {
    Span::raw(c.to_string()).width()
}

/// A simple table: columns with fixed widths (one may flex), two-space gaps,
/// header + rule like rich's SIMPLE_HEAD.
struct Table {
    cols: Vec<(String, usize, bool)>, // title, width, right-aligned
    rows: Vec<Vec<Vec<Span<'static>>>>,
}

impl Table {
    fn new(cols: &[(&str, usize, bool)]) -> Self {
        Table {
            cols: cols
                .iter()
                .map(|(t, w, r)| (t.to_string(), *w, *r))
                .collect(),
            rows: vec![],
        }
    }

    fn row(&mut self, cells: Vec<Vec<Span<'static>>>) {
        self.rows.push(cells);
    }

    /// flex column gets whatever is left of `width`
    fn lines(mut self, width: usize, flex: usize) -> Vec<Line<'static>> {
        let fixed: usize = self
            .cols
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != flex)
            .map(|(_, c)| c.1)
            .sum();
        let gaps = 2 * (self.cols.len() - 1);
        self.cols[flex].1 = width.saturating_sub(fixed + gaps).max(4);
        let mut out = Vec::with_capacity(self.rows.len() + 2);
        let head: Vec<Span> = self
            .cols
            .iter()
            .enumerate()
            .flat_map(|(i, (t, w, r))| {
                let mut v = fit(vec![Span::styled(t.clone(), bold())], *w, *r);
                if i + 1 < self.cols.len() {
                    v.push(Span::raw("  "));
                }
                v
            })
            .collect();
        out.push(Line::from(head));
        out.push(Line::from(Span::styled(
            "─".repeat(width),
            Style::new().fg(DARK),
        )));
        for row in self.rows {
            let mut spans = Vec::new();
            for (i, cell) in row.into_iter().enumerate() {
                let (_, w, r) = &self.cols[i];
                spans.extend(fit(cell, *w, *r));
                if i + 1 < self.cols.len() {
                    spans.push(Span::raw("  "));
                }
            }
            out.push(Line::from(spans));
        }
        out
    }
}

fn s(t: impl Into<String>) -> Vec<Span<'static>> {
    vec![Span::raw(t.into())]
}

fn st(t: impl Into<String>, style: Style) -> Vec<Span<'static>> {
    vec![Span::styled(t.into(), style)]
}

// ---------------------------------------------------------------- context

pub struct Ctx {
    pub live: bool,
    pub interval: f64,
    pub procs: bool,
    pub me: String,
    pub now: Instant,
}

fn retry_note(v: &View, ctx: &Ctx) -> String {
    if !ctx.live || v.error.is_none() {
        return String::new();
    }
    if v.blocked {
        return "  · press r to retry".into();
    }
    match v.retry_at {
        Some(t) if t > ctx.now && (t - ctx.now).as_secs() >= 1 => {
            format!("  · retry in {}s", (t - ctx.now).as_secs())
        }
        _ => String::new(),
    }
}

fn users_text(g: &Value, me: &str) -> Vec<Span<'static>> {
    let mut agg: Vec<(String, String, f64)> = Vec::new(); // (user, label, mem)
    for p in busy_procs(g) {
        let label = proc_owner(p);
        let mem = num(p, "mem").unwrap_or(0.0);
        match agg.iter_mut().find(|a| a.1 == label) {
            Some(a) => a.2 += mem,
            None => agg.push((text(p, "user").unwrap_or("?").to_string(), label, mem)),
        }
    }
    agg.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    for (i, (user, label, mem)) in agg.into_iter().enumerate() {
        if i > 0 {
            out.push(Span::raw("  "));
        }
        out.push(Span::styled(label, user_style(&user, me)));
        out.push(Span::styled(format!(" {}", gib(Some(mem))), dim()));
    }
    out
}

// ---------------------------------------------------------------- server panel

pub struct Panel {
    pub lines: Vec<Line<'static>>,
    pub title: Line<'static>,
    pub subtitle: Line<'static>,
    pub border: Style,
}

impl Panel {
    pub fn height(&self) -> u16 {
        self.lines.len() as u16 + 2
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut title = self.title.clone();
        title.spans.insert(0, Span::styled("─", self.border));
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.border)
            .padding(ratatui::widgets::Padding::horizontal(1))
            .title(title)
            .title_bottom(self.subtitle.clone().right_aligned());
        Paragraph::new(self.lines.clone())
            .block(block)
            .render(area, buf);
    }
}

/// `width` is the panel's outer width; narrow panels drop Power, then Fan, then the bars,
/// and keep the Users column to the end.
pub fn server_panel(v: &View, ctx: &Ctx, width: u16) -> Panel {
    let inner = width.saturating_sub(4) as usize;
    let mut title = vec![
        Span::styled(format!(" {}", v.name), bold()),
        Span::styled(format!("  {}", v.host), dim()),
    ];
    let Some(d) = &v.data else {
        let msg = v.error.clone().unwrap_or_else(|| "connecting...".into());
        let style = if v.error.is_some() {
            Style::new().fg(Color::Red)
        } else {
            dim()
        };
        title.push(Span::raw(" "));
        return Panel {
            lines: vec![Line::from(vec![
                Span::styled(msg, style),
                Span::styled(retry_note(v, ctx), dim()),
            ])],
            title: Line::from(title),
            subtitle: Line::default(),
            border: if v.error.is_some() {
                Style::new().fg(Color::Red)
            } else {
                Style::new().fg(GREY)
            },
        };
    };
    if let Some(drv) = text(d, "driver") {
        title.push(Span::styled(
            format!("  driver {} · CUDA {}", drv, text(d, "cuda").unwrap_or("?")),
            dim(),
        ));
    }
    title.push(Span::raw(" "));
    let stale = v
        .updated
        .is_some_and(|t| ctx.now.duration_since(t).as_secs_f64() > (3.0 * ctx.interval).max(15.0));
    let (cpu, mem) = (&d["cpu"], &d["mem"]);
    let total = num(mem, "total_kb").unwrap_or(0.0) / 1048576.0;
    let used = total - num(mem, "avail_kb").unwrap_or(0.0) / 1048576.0;
    let usage = num(cpu, "usage");

    let mut head = vec![Span::styled("CPU ", bold())];
    head.extend(bar(usage, 8));
    head.push(Span::raw(format!(
        " {:>3.0}% of {}c",
        usage.unwrap_or(0.0),
        num(cpu, "count").unwrap_or(0.0)
    )));
    head.push(Span::styled("   load ", bold()));
    let load: Vec<String> = arr(cpu, "load")
        .iter()
        .map(|x| format!("{:.1}", x.as_f64().unwrap_or(0.0)))
        .collect();
    head.push(Span::raw(load.join(" / ")));
    head.push(Span::styled("   RAM ", bold()));
    head.extend(bar(
        if total > 0.0 {
            Some(100.0 * used / total)
        } else {
            None
        },
        8,
    ));
    head.push(Span::raw(format!(" {:.0}/{:.0}G", used, total)));
    if let Some(up) = num(d, "uptime") {
        head.push(Span::styled(format!("   up {}", fmt_age(up)), dim()));
    }
    let mut cpu_line = vec![Span::styled("CPU users  ", bold())];
    let top: Vec<&Value> = arr(d, "users")
        .iter()
        .filter(|u| num(u, "cpu").unwrap_or(0.0) >= 50.0)
        .take(6)
        .collect();
    for u in &top {
        let name = text(u, "user").unwrap_or("?").to_string();
        cpu_line.push(Span::styled(name.clone(), user_style(&name, &ctx.me)));
        cpu_line.push(Span::styled(
            format!(
                " {:.1}c {:.0}G   ",
                num(u, "cpu").unwrap_or(0.0) / 100.0,
                user_ram_kb(u) / 1048576.0
            ),
            dim(),
        ));
    }
    if top.is_empty() {
        cpu_line.push(Span::styled("-", dim()));
    }
    let mut lines = vec![Line::from(head), Line::from(cpu_line)];

    let gpus = arr(d, "gpus");
    if text(d, "nvml") == Some("absent") {
        lines.push(Line::from(Span::styled("CPU only (no NVIDIA GPU)", dim())));
    } else if gpus.is_empty() {
        let msg = text(d, "gpu_error")
            .map(|e| format!("GPU: {}", e))
            .unwrap_or_else(|| "no GPUs".into());
        lines.push(Line::from(Span::styled(
            msg,
            Style::new().fg(Color::Yellow),
        )));
    } else {
        lines.push(Line::default());
        lines.extend(gpu_table(d, gpus, inner, ctx));
        if ctx.procs {
            let pl = proc_table(d, gpus, inner, ctx);
            if pl.len() > 2 {
                lines.push(Line::default());
                lines.extend(pl);
            }
        }
    }

    let n_idle = gpus.iter().filter(|g| gpu_is_idle(g)).count();
    let mut sub = Vec::new();
    if !gpus.is_empty() {
        let style = if n_idle > 0 {
            Style::new().fg(Color::Green)
        } else {
            dim()
        };
        sub.push(Span::styled(
            format!(" {} GPUs · {} idle", gpus.len(), n_idle),
            style,
        ));
        let lost = gpus
            .iter()
            .filter(|g| g["invalid"].as_bool().unwrap_or(false))
            .count();
        if lost > 0 {
            sub.push(Span::styled(
                format!(" · {} lost", lost),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(e) = text(d, "gpu_error") {
            sub.push(Span::styled(
                format!("  ⚠ {}", e.chars().take(50).collect::<String>()),
                Style::new().fg(Color::Yellow),
            ));
        }
    }
    if let Some(e) = &v.error {
        sub.push(Span::styled(
            format!("  ⚠ {}", e.chars().take(60).collect::<String>()),
            Style::new().fg(Color::Red),
        ));
        sub.push(Span::styled(retry_note(v, ctx), dim()));
    } else if stale {
        sub.push(Span::styled("  ⚠ stale", Style::new().fg(Color::Yellow)));
    } else if let Some(l) = v.latency {
        sub.push(Span::styled(format!("  {:.1}s", l), dim()));
    }
    sub.push(Span::raw(" "));
    Panel {
        lines,
        title: Line::from(title),
        subtitle: Line::from(sub),
        border: Style::new().fg(if v.error.is_some() || stale {
            Color::Yellow
        } else {
            GREY
        }),
    }
}

fn gpu_table(_d: &Value, gpus: &[Value], width: usize, ctx: &Ctx) -> Vec<Line<'static>> {
    let show_power = width >= 100;
    let show_fan = width >= 84
        && gpus
            .iter()
            .any(|g| !g["fan"].is_null() && g.get("fan").is_some());
    let bar_w = if width >= 84 {
        10
    } else if width >= 72 {
        6
    } else {
        0
    };
    let name_w = gpus
        .iter()
        .map(|g| gpu_label(g).chars().count())
        .max()
        .unwrap_or(3)
        .clamp(3, 18);
    let mut cols = vec![
        ("#", 2, true),
        ("GPU", name_w, false),
        (
            if show_fan { "Temp Fan" } else { "Temp" },
            if show_fan { 8 } else { 4 },
            true,
        ),
    ];
    cols.push((
        "Util",
        bar_w
            + 5
            + if gpus
                .iter()
                .any(|g| num(g, "enc").unwrap_or(0.0) + num(g, "dec").unwrap_or(0.0) > 0.0)
            {
                8
            } else {
                0
            },
        false,
    ));
    cols.push(("Memory", bar_w + 12, false));
    if show_power {
        cols.push(("Power", 9, true));
    }
    cols.push(("Users (GPU mem)", 12, false));
    let flex = cols.len() - 1;
    let mut t = Table::new(&cols);
    for g in gpus {
        let idx = g["index"].as_u64().unwrap_or(0).to_string();
        if g["invalid"].as_bool().unwrap_or(false) {
            let red = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
            let mut row = vec![
                st(idx, red),
                st(text(g, "name").unwrap_or("?").to_string(), red),
                s(""),
                s(""),
                s(""),
            ];
            if show_power {
                row.push(s(""));
            }
            row.push(st(
                "needs admin (reset/reboot)",
                Style::new().fg(Color::Red),
            ));
            t.row(row);
            continue;
        }
        let idle = gpu_is_idle(g);
        let mut util = if bar_w > 0 {
            bar(num(g, "util"), bar_w)
        } else {
            vec![]
        };
        if bar_w > 0 {
            util.push(Span::raw(" "));
        }
        util.push(Span::raw(
            num(g, "util").map_or("  - ".into(), |u| format!("{:>3.0}%", u)),
        ));
        let (enc, dec) = (num(g, "enc").unwrap_or(0.0), num(g, "dec").unwrap_or(0.0));
        if enc > 0.0 || dec > 0.0 {
            util.push(Span::styled(
                format!(" E{:.0} D{:.0}", enc, dec),
                Style::new().fg(Color::Magenta),
            ));
        }
        let (mu, mt) = (num(g, "mem_used"), num(g, "mem_total"));
        let mpct = match (mu, mt) {
            (Some(u), Some(t)) if t > 0.0 => Some(100.0 * u / t),
            _ => None,
        };
        let mut memc = if bar_w > 0 { bar(mpct, bar_w) } else { vec![] };
        if bar_w > 0 {
            memc.push(Span::raw(" "));
        }
        memc.push(Span::raw(format!("{}/{}", gib(mu), gib(mt))));
        let temp = num(g, "temp");
        let mut tc = vec![Span::styled(
            temp.map_or("-".into(), |t| format!("{:.0}°", t)),
            level(temp, 70.0, 83.0),
        )];
        if show_fan {
            tc.push(Span::styled(
                format!(
                    " {:>3}",
                    num(g, "fan").map_or("-".into(), |f| format!("{:.0}%", f))
                ),
                Style::new().fg(Color::Cyan),
            ));
        }
        let users = if !busy_procs(g).is_empty() {
            users_text(g, &ctx.me)
        } else if g["procs_supported"] == Value::Bool(false) {
            st("process info N/A", dim())
        } else if idle {
            st("idle", Style::new().fg(Color::Green))
        } else {
            st("-", dim())
        };
        let mut row = vec![
            st(
                idx,
                if idle {
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    bold()
                },
            ),
            s(gpu_label(g)),
            tc,
            util,
            memc,
        ];
        if show_power {
            row.push(s(num(g, "power").map_or("-".into(), |p| {
                format!("{:.0}/{:.0}W", p, num(g, "power_limit").unwrap_or(0.0))
            })));
        }
        row.push(users);
        t.row(row);
    }
    t.lines(width, flex)
}

pub fn gpu_label(g: &Value) -> String {
    let name = short_gpu_name(text(g, "name").unwrap_or("?"));
    match g.get("mig").and_then(Value::as_array) {
        Some(m) if !m.is_empty() => format!("{} MIG×{}", name, m.len()),
        _ => name,
    }
}

fn owner_spans(p: &Value, me: &str) -> Vec<Span<'static>> {
    let user = text(p, "user").unwrap_or("?").to_string();
    let mut v = vec![Span::styled(
        proc_owner(p),
        if text(p, "user").is_some() {
            user_style(&user, me)
        } else {
            dim()
        },
    )];
    if let Some(state) = text(p, "state") {
        let style = if matches!(state, "gone" | "zombie") {
            Style::new().fg(Color::Red)
        } else {
            dim()
        };
        v.push(Span::styled(format!(" ({})", state), style));
    }
    v
}

fn proc_table(d: &Value, gpus: &[Value], width: usize, ctx: &Ctx) -> Vec<Line<'static>> {
    let has_sm = gpus
        .iter()
        .flat_map(|g| arr(g, "procs"))
        .any(|p| p.get("sm").is_some());
    let has_age = gpus
        .iter()
        .flat_map(|g| arr(g, "procs"))
        .any(|p| proc_age(p, d).is_some());
    let mut cols = vec![
        ("GPU", 3, true),
        ("PID", 8, true),
        ("User", 14, false),
        ("GPU mem", 7, true),
    ];
    if has_sm {
        cols.push(("SM%", 4, true));
    }
    cols.extend([("CPU%", 5, true), ("RAM", 6, true)]);
    if has_age {
        cols.push(("Age", 6, true));
    }
    cols.push(("Command", 10, false));
    let flex = cols.len() - 1;
    let mut t = Table::new(&cols);
    for g in gpus {
        let mut procs = busy_procs(g);
        procs.sort_by(|a, b| {
            num(b, "mem")
                .partial_cmp(&num(a, "mem"))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for p in procs {
            let mut row = vec![
                st(g["index"].as_u64().unwrap_or(0).to_string(), dim()),
                st(p["pid"].as_u64().unwrap_or(0).to_string(), dim()),
                owner_spans(p, &ctx.me),
                st(gib(num(p, "mem")), dim()),
            ];
            if has_sm {
                row.push(st(
                    num(p, "sm").map_or("-".into(), |x| format!("{:.0}", x)),
                    dim(),
                ));
            }
            row.push(st(
                num(p, "cpu").map_or("-".into(), |x| format!("{:.0}", x)),
                dim(),
            ));
            row.push(st(
                num(p, "rss_kb").map_or("-".into(), |x| gib(Some(x / 1024.0))),
                dim(),
            ));
            if has_age {
                row.push(st(proc_age(p, d).map_or("-".into(), fmt_age), dim()));
            }
            let cmd = text(p, "cmd").unwrap_or("?");
            let (exe, args) = cmd.split_once(' ').unwrap_or((cmd, ""));
            let exe = exe.rsplit('/').next().unwrap_or(exe).to_string();
            row.push(vec![
                Span::styled(exe, bold().fg(GREY)),
                Span::styled(
                    if args.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", args)
                    },
                    dim(),
                ),
            ]);
            t.row(row);
        }
    }
    t.lines(width, flex)
}

// ---------------------------------------------------------------- users across servers

#[derive(Default, Debug)]
pub struct UserTotals {
    pub gpus: usize,
    pub gpu_mem: f64,
    pub cpu: f64,
    pub ram_kb: f64,
    pub hosts: BTreeMap<String, Vec<u64>>,
}

/// GPU + CPU usage per user (or user@container) across every server.
pub fn collect_users(views: &[View]) -> BTreeMap<String, UserTotals> {
    let mut agg: BTreeMap<String, UserTotals> = BTreeMap::new();
    for v in views {
        let Some(d) = &v.data else { continue };
        for g in arr(d, "gpus") {
            let mut per: BTreeMap<String, f64> = BTreeMap::new();
            for p in busy_procs(g) {
                *per.entry(proc_owner(p)).or_default() += num(p, "mem").unwrap_or(0.0);
            }
            for (user, mem) in per {
                let e = agg.entry(user).or_default();
                e.gpus += 1;
                e.gpu_mem += mem;
                e.hosts
                    .entry(v.name.clone())
                    .or_default()
                    .push(g["index"].as_u64().unwrap_or(0));
            }
        }
        for u in arr(d, "users") {
            let name = text(u, "user").unwrap_or("?").to_string();
            if num(u, "cpu").unwrap_or(0.0) >= 50.0 || agg.contains_key(&name) {
                let e = agg.entry(name).or_default();
                e.cpu += num(u, "cpu").unwrap_or(0.0);
                e.ram_kb += user_ram_kb(u);
            }
        }
    }
    agg
}

pub fn user_summary(views: &[View], ctx: &Ctx, width: u16) -> Panel {
    let agg = collect_users(views);
    let mut rows: Vec<(&String, &UserTotals)> = agg.iter().collect();
    rows.sort_by(|a, b| {
        b.1.gpu_mem
            .partial_cmp(&a.1.gpu_mem)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.1.cpu
                    .partial_cmp(&a.1.cpu)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    let mut t = Table::new(&[
        ("User", 14, false),
        ("GPUs", 4, true),
        ("GPU mem", 8, true),
        ("Where  server[gpu idx]", 10, false),
        ("CPU cores", 9, true),
        ("RAM", 7, true),
    ]);
    for (user, e) in rows.into_iter().take(20) {
        let (base, _) = user.split_once('@').unwrap_or((user, ""));
        let where_: String = e
            .hosts
            .iter()
            .map(|(h, idx)| {
                let mut i = idx.clone();
                i.sort();
                format!(
                    "{}[{}] ",
                    h,
                    i.iter()
                        .map(|x| x.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .collect();
        let in_ctr = user.contains('@'); // CPU/RAM of containerised jobs belong to the host-side user
        t.row(vec![
            st(user.clone(), user_style(base, &ctx.me)),
            s(if e.gpus > 0 {
                e.gpus.to_string()
            } else {
                "-".into()
            }),
            s(if e.gpus > 0 {
                gib(Some(e.gpu_mem))
            } else {
                "-".into()
            }),
            s(where_),
            s(if in_ctr {
                "-".into()
            } else {
                format!("{:.1}", e.cpu / 100.0)
            }),
            s(if in_ctr {
                "-".into()
            } else {
                format!("{:.0}G", e.ram_kb / 1048576.0)
            }),
        ]);
    }
    Panel {
        lines: t.lines(width.saturating_sub(4) as usize, 3),
        title: Line::from(Span::styled(" Users across all servers ", bold())),
        subtitle: Line::default(),
        border: Style::new().fg(GREY),
    }
}

// ---------------------------------------------------------------- compact view

const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// One character per GPU: height = memory used, colour = load (green when idle).
pub fn gpu_strip(gpus: &[Value]) -> Vec<Span<'static>> {
    gpus.iter()
        .map(|g| {
            if g["invalid"].as_bool().unwrap_or(false) {
                return Span::styled(
                    "✗",
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                );
            }
            let frac = match (num(g, "mem_used"), num(g, "mem_total")) {
                (Some(u), Some(t)) if t > 0.0 => u / t,
                _ => 0.0,
            };
            let ch = GLYPHS[((frac * 8.0) as usize).min(7)];
            let style = if gpu_is_idle(g) {
                Style::new().fg(Color::Green)
            } else {
                level(num(g, "util"), 50.0, 85.0)
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

pub fn compact_lines(views: &[View], ctx: &Ctx, width: u16) -> Vec<Line<'static>> {
    let strip_w = views
        .iter()
        .filter_map(|v| v.data.as_ref())
        .map(|d| arr(d, "gpus").len())
        .max()
        .unwrap_or(3)
        .max(4);
    let mut t = Table::new(&[
        (
            "Server",
            views
                .iter()
                .map(|v| v.name.chars().count())
                .max()
                .unwrap_or(6)
                .max(6),
            false,
        ),
        ("GPUs", strip_w, false),
        ("Idle", 11, false),
        ("Who (GPUs)", 10, false),
        ("CPU", 4, true),
        ("RAM", 11, true),
        ("", 12, false),
    ]);
    for v in views {
        let name = st(v.name.clone(), bold());
        let Some(d) = &v.data else {
            let msg = v.error.clone().unwrap_or_else(|| "connecting...".into());
            let style = if v.error.is_some() {
                Style::new().fg(Color::Red)
            } else {
                dim()
            };
            t.row(vec![
                name,
                s(""),
                s(""),
                vec![
                    Span::styled(msg, style),
                    Span::styled(retry_note(v, ctx), dim()),
                ],
                s(""),
                s(""),
                s(""),
            ]);
            continue;
        };
        let gpus = arr(d, "gpus");
        let mut counts: Vec<(String, usize)> = Vec::new();
        for g in gpus {
            let mut owners: Vec<String> = busy_procs(g).iter().map(|p| proc_owner(p)).collect();
            owners.dedup();
            owners.sort();
            owners.dedup();
            for o in owners {
                match counts.iter_mut().find(|c| c.0 == o) {
                    Some(c) => c.1 += 1,
                    None => counts.push((o, 1)),
                }
            }
        }
        counts.sort_by_key(|c| std::cmp::Reverse(c.1));
        let mut who = Vec::new();
        for (o, n) in counts {
            let base = o.split('@').next().unwrap_or("").to_string();
            who.push(Span::styled(o, user_style(&base, &ctx.me)));
            who.push(Span::styled(
                if n > 1 {
                    format!("×{}  ", n)
                } else {
                    "  ".into()
                },
                dim(),
            ));
        }
        let idle: Vec<String> = gpus
            .iter()
            .filter(|g| gpu_is_idle(g))
            .map(|g| g["index"].as_u64().unwrap_or(0).to_string())
            .collect();
        let mem = &d["mem"];
        let total = num(mem, "total_kb").unwrap_or(0.0) / 1048576.0;
        let used = total - num(mem, "avail_kb").unwrap_or(0.0) / 1048576.0;
        let status = match &v.error {
            Some(e) => st(
                e.chars().take(40).collect::<String>(),
                Style::new().fg(Color::Red),
            ),
            None => st(format!("{:.1}s", v.latency.unwrap_or(0.0)), dim()),
        };
        t.row(vec![
            name,
            if gpus.is_empty() {
                st("cpu", dim())
            } else {
                gpu_strip(gpus)
            },
            st(idle.join(","), Style::new().fg(Color::Green)),
            who,
            s(format!("{:.0}%", num(&d["cpu"], "usage").unwrap_or(0.0))),
            s(format!("{:.0}/{:.0}G", used, total)),
            status,
        ]);
    }
    t.lines(width as usize, 3)
}

// ---------------------------------------------------------------- header and body

pub fn header(views: &[View], interval: f64, short: bool) -> Line<'static> {
    let gpus: Vec<&Value> = views
        .iter()
        .filter_map(|v| v.data.as_ref())
        .flat_map(|d| arr(d, "gpus"))
        .collect();
    let idle = gpus.iter().filter(|g| gpu_is_idle(g)).count();
    let up = views.iter().filter(|v| v.ok()).count();
    let idle_style = if idle > 0 {
        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        dim()
    };
    let now = chrono_like_now(short);
    let mut l = vec![Span::styled(
        " gpuwho ",
        Style::new()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if short {
        l.push(Span::raw(format!(
            " {}/{} up · {} GPUs · ",
            up,
            views.len(),
            gpus.len()
        )));
        l.push(Span::styled(format!("{} idle", idle), idle_style));
        l.push(Span::styled(format!(" · {}", now), dim()));
    } else {
        l.push(Span::raw(format!(
            "  {}/{} servers up  ·  GPUs {}  ·  ",
            up,
            views.len(),
            gpus.len()
        )));
        l.push(Span::styled(format!("idle {}", idle), idle_style));
        l.push(Span::styled(format!("  ·  {}", now), dim()));
        if interval > 0.0 {
            l.push(Span::styled(
                format!("  ·  every {}s", trim_float(interval)),
                dim(),
            ));
        }
    }
    Line::from(l)
}

fn trim_float(x: f64) -> String {
    let s = format!("{:.2}", x);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Local wall clock without a time crate: libc localtime_r on unix, UTC elsewhere.
fn chrono_like_now(time_only: bool) -> String {
    #[cfg(unix)]
    {
        let t = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };
        if time_only {
            return format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec);
        }
        format!(
            "{}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
    #[cfg(not(unix))]
    {
        let s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let (h, m, sec) = (s / 3600 % 24, s / 60 % 60, s % 60);
        let _ = time_only;
        format!("{:02}:{:02}:{:02} UTC", h, m, sec)
    }
}

/// Everything below the header, drawn into a buffer as tall as it needs to be.
pub fn body(views: &[View], ctx: &Ctx, width: u16, compact: bool) -> Buffer {
    let summary = user_summary(views, ctx, width);
    if compact {
        let lines = compact_lines(views, ctx, width);
        let h = lines.len() as u16 + 1 + summary.height();
        let mut buf = Buffer::empty(Rect::new(0, 0, width, h));
        Paragraph::new(lines.clone()).render(Rect::new(0, 0, width, lines.len() as u16), &mut buf);
        summary.render(
            Rect::new(0, lines.len() as u16 + 1, width, summary.height()),
            &mut buf,
        );
        return buf;
    }
    let cols = (width as usize / 115).clamp(1, views.len().max(1));
    let col_w = (width as usize - (cols - 1)) / cols;
    let panels: Vec<Panel> = views
        .iter()
        .map(|v| server_panel(v, ctx, col_w as u16))
        .collect();
    let rows: Vec<&[Panel]> = panels.chunks(cols).collect();
    let grid_h: u16 = rows
        .iter()
        .map(|r| r.iter().map(Panel::height).max().unwrap_or(0))
        .sum();
    let mut buf = Buffer::empty(Rect::new(0, 0, width, grid_h + summary.height()));
    let mut y = 0;
    for row in rows {
        let h = row.iter().map(Panel::height).max().unwrap_or(0);
        for (i, p) in row.iter().enumerate() {
            p.render(
                Rect::new((i * (col_w + 1)) as u16, y, col_w as u16, p.height()),
                &mut buf,
            );
        }
        y += h;
    }
    summary.render(Rect::new(0, y, width, summary.height()), &mut buf);
    buf
}

/// The whole dashboard as one buffer (header + body), for `gpuwho once`.
pub fn dashboard(views: &[View], ctx: &Ctx, width: u16, compact: bool) -> Buffer {
    let b = body(views, ctx, width, compact);
    let mut out = Buffer::empty(Rect::new(0, 0, width, b.area.height + 1));
    Paragraph::new(header(views, ctx.interval, false)).render(Rect::new(0, 0, width, 1), &mut out);
    copy_rows(&b, 0, &mut out, 1, b.area.height);
    out
}

/// Copy `n` rows starting at `from` of `src` into `dst` at row `to`.
pub fn copy_rows(src: &Buffer, from: u16, dst: &mut Buffer, to: u16, n: u16) {
    let w = src.area.width.min(dst.area.width);
    let (x0, bottom) = (dst.area.x, dst.area.y + dst.area.height);
    for dy in 0..n {
        if from + dy >= src.area.height || to + dy >= bottom {
            break;
        }
        for x in 0..w {
            dst[(x0 + x, to + dy)] = src[(x, from + dy)].clone();
        }
    }
}

/// A buffer as text, with ANSI colours if `color`, trailing spaces trimmed.
pub fn to_ansi(buf: &Buffer, color: bool) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        let mut cur = Style::default();
        let mut plain_len = 0;
        let mut last_visible = 0;
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            let style = cell.style();
            if color && style != cur {
                line.push_str(&sgr(style));
                cur = style;
            }
            line.push_str(cell.symbol());
            plain_len += 1;
            if cell.symbol() != " " {
                last_visible = line.len();
            }
        }
        let _ = plain_len;
        line.truncate(last_visible);
        out.push_str(&line);
        if color {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    out
}

fn sgr(s: Style) -> String {
    let mut codes = vec!["0".to_string()];
    if s.add_modifier.contains(Modifier::BOLD) {
        codes.push("1".into());
    }
    if s.add_modifier.contains(Modifier::DIM) {
        codes.push("2".into());
    }
    if s.add_modifier.contains(Modifier::UNDERLINED) {
        codes.push("4".into());
    }
    let color = |c: Color, fg: bool| -> Option<String> {
        let base = if fg { 30 } else { 40 };
        Some(match c {
            Color::Reset => return None,
            Color::Black => format!("{}", base),
            Color::Red => format!("{}", base + 1),
            Color::Green => format!("{}", base + 2),
            Color::Yellow => format!("{}", base + 3),
            Color::Blue => format!("{}", base + 4),
            Color::Magenta => format!("{}", base + 5),
            Color::Cyan => format!("{}", base + 6),
            Color::Gray => format!("{}", base + 7),
            Color::DarkGray => format!("{}", base + 60),
            Color::LightRed => format!("{}", base + 61),
            Color::LightGreen => format!("{}", base + 62),
            Color::LightYellow => format!("{}", base + 63),
            Color::LightBlue => format!("{}", base + 64),
            Color::LightMagenta => format!("{}", base + 65),
            Color::LightCyan => format!("{}", base + 66),
            Color::White => format!("{}", base + 67),
            Color::Indexed(i) => format!("{};5;{}", base + 8, i),
            Color::Rgb(r, g, b) => format!("{};2;{};{};{}", base + 8, r, g, b),
        })
    };
    if let Some(c) = s.fg.and_then(|c| color(c, true)) {
        codes.push(c);
    }
    if let Some(c) = s.bg.and_then(|c| color(c, false)) {
        codes.push(c);
    }
    format!("\x1b[{}m", codes.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster() -> Vec<View> {
        crate::demo::entries()
            .iter()
            .map(|e| {
                let mut v = View::new(e["name"].as_str().unwrap(), e["host"].as_str().unwrap());
                v.data = Some(e["data"].clone());
                v.updated = Some(Instant::now());
                v.latency = Some(0.4);
                v
            })
            .collect()
    }

    fn ctx() -> Ctx {
        Ctx {
            live: true,
            interval: 3.0,
            procs: true,
            me: "nobody".into(),
            now: Instant::now(),
        }
    }

    fn plain(buf: &Buffer) -> String {
        to_ansi(buf, false)
    }

    fn gpu<'a>(c: &'a [View], server: &str, i: usize) -> &'a Value {
        &c.iter()
            .find(|v| v.name == server)
            .unwrap()
            .data
            .as_ref()
            .unwrap()["gpus"][i]
    }

    #[test]
    fn idle_and_display_servers() {
        let c = cluster();
        assert!(gpu_is_idle(gpu(&c, "node01", 3)) && !gpu_is_idle(gpu(&c, "node01", 0)));
        assert!(gpu_is_idle(gpu(&c, "node03", 2))); // only a parked Xorg on it
        assert!(!gpu_is_idle(gpu(&c, "node03", 7))); // lost
        assert!(!gpu_is_idle(
            &serde_json::json!({"procs": [], "mem_used": null})
        ));
    }

    #[test]
    fn users_across_servers() {
        let u = collect_users(&cluster());
        assert_eq!(u["dave"].gpus, 4);
        assert_eq!(u["dave"].hosts["node02"], vec![0, 1, 2, 3]);
        assert!(u.contains_key("root@3f9c2a") && !u.contains_key("gdm"));
        assert_eq!(u["frank"].gpus, 0);
        assert_eq!((u["bob"].ram_kb / 1048576.0 * 10.0).round() / 10.0, 20.1);
    }

    #[test]
    fn dashboard_has_everything() {
        let c = cluster();
        let out = plain(&dashboard(&c, &ctx(), 240, false));
        for name in [
            "node01",
            "node02",
            "node03",
            "cpu01",
            "4/4 servers up",
            "GPUs 20",
            "idle 7",
            "CPU only",
            "needs admin",
            "VLLM::Worker_TP0",
            "Users across all servers",
        ] {
            assert!(out.contains(name), "missing {}", name);
        }
    }

    #[test]
    fn narrow_panels_keep_users() {
        let c = cluster();
        let mut x = ctx();
        x.procs = false;
        let p = server_panel(&c[0], &x, 76);
        let mut buf = Buffer::empty(Rect::new(0, 0, 76, p.height()));
        p.render(buf.area, &mut buf);
        let out = plain(&buf);
        assert!(out.contains("Users") && out.contains("alice") && !out.contains("Power"));
    }

    #[test]
    fn compact_view() {
        let out = plain(&dashboard(&cluster(), &ctx(), 120, true));
        assert!(
            out.contains("Who (GPUs)")
                && out.contains("dave×4")
                && out.contains("3,4")
                && out.contains('✗')
        );
        assert!(out.lines().count() < 30);
    }

    /// cargo test -p gpuwho --release render_speed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn render_speed() {
        let base = cluster();
        for n in [4usize, 16, 32, 64, 128] {
            let views: Vec<View> = (0..n)
                .map(|i| {
                    let mut v = base[i % 3].clone();
                    v.name = format!("node{:02}", i);
                    v
                })
                .collect();
            for compact in [false, true] {
                let t = Instant::now();
                for _ in 0..20 {
                    let _ = body(&views, &ctx(), 240, compact);
                }
                println!(
                    "{:>4} servers {:>8}: {:.2} ms/frame",
                    n,
                    if compact { "compact" } else { "full" },
                    t.elapsed().as_secs_f64() * 1000.0 / 20.0
                );
            }
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(gib(None), "-");
        assert_eq!(gib(Some(2048.0)), "2.0G");
        assert_eq!(fmt_age(90061.0), "1d1h");
        assert_eq!(fmt_age(3720.0), "1h2m");
        assert_eq!(short_gpu_name("NVIDIA GeForce RTX 4090"), "RTX 4090");
        assert_eq!(
            proc_owner(&serde_json::json!({"user": "root", "container": "3f9c2a1b4d5e"})),
            "root@3f9c2a"
        );
        let fitted = fit(vec![Span::raw("abcdefghij")], 5, false);
        assert_eq!(
            fitted
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>(),
            "abcd…"
        );
    }
}
