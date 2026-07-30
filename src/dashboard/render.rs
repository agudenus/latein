//! Server-side rendering. Plain string building — no template engine, no build step, and
//! no client-side framework: the page arrives complete and the poll only patches cells.
//!
//! Everything user- or API-derived goes through [`esc`]. Market titles come from a third
//! party, so they are escaped even though they are "just" market names.
//!
//! The markup deliberately mirrors the design handoff's structure (header bar → hero →
//! funnel → opportunity table → right rail), and every literal colour and size lives in
//! `assets/app.css` on Nocturne's variables rather than inline here.

use crate::dashboard::state::{BreakState, DashboardState, FunnelStage, OpportunityView};

/// Phosphor `warning-octagon`, regular weight, inlined on `currentColor` — the design's
/// one icon, and the reason this page loads no icon font.
const WARNING_OCTAGON: &str = r##"<svg class="icon" viewBox="0 0 256 256" width="16" height="16" fill="none" stroke="currentColor" stroke-width="16" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M164.5 32h-73a8 8 0 0 0-5.7 2.3L34.3 85.8a8 8 0 0 0-2.3 5.7v73a8 8 0 0 0 2.3 5.7l51.5 51.5a8 8 0 0 0 5.7 2.3h73a8 8 0 0 0 5.7-2.3l51.5-51.5a8 8 0 0 0 2.3-5.7v-73a8 8 0 0 0-2.3-5.7l-51.5-51.5a8 8 0 0 0-5.7-2.3Z"/><path d="M128 80v56"/><path d="M128 172h.1"/></svg>"##;

/// Escape into HTML text/attribute context.
pub fn esc(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn opt(value: &Option<String>) -> String {
    esc(value.as_deref().unwrap_or("—"))
}

/// The whole document.
pub fn page(state: &DashboardState) -> String {
    let break_key = state
        .break_state
        .as_ref()
        .map(|b| format!("{:?}", b.reason).to_lowercase())
        .unwrap_or_else(|| "none".to_string());

    let body = match &state.break_state {
        Some(brk) => break_page(state, brk),
        None => console(state),
    };

    format!(
        r#"<!doctype html>
<html lang="en" data-poll-ms="{poll}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
<meta name="robots" content="noindex, nofollow">
<title>polyarb — soak monitor</title>
<link rel="stylesheet" href="/assets/nocturne-styles.css">
<link rel="stylesheet" href="/assets/app.css">
<link rel="icon" href="data:,">
</head>
<body data-break="{break_key}">
{body}
<script src="/assets/app.js" defer></script>
</body>
</html>
"#,
        poll = state.poll_interval_ms,
        break_key = esc(&break_key),
        body = body,
    )
}

// ---------------------------------------------------------------------------------
// 2a — evidence console
// ---------------------------------------------------------------------------------

fn console(s: &DashboardState) -> String {
    format!(
        r#"<header class="topbar">{header}</header>
<div class="shell">
  <main class="col-main">
    {hero}
    {funnel}
    {table}
  </main>
  <aside class="rail">
    {rail}
  </aside>
</div>"#,
        header = header(s, false),
        hero = hero(s),
        funnel = funnel(s),
        table = table(s),
        rail = rail(s),
    )
}

fn header(s: &DashboardState, broken: bool) -> String {
    let h = &s.header;
    let pill = if broken {
        format!(
            r#"<span class="pill pill-blind" data-key="header.transport.label">{}</span>"#,
            esc(&h.transport.label)
        )
    } else {
        format!(
            r#"<span class="pill pill-stream" data-state="{state}" data-key="header.transport.state">
             <span class="dot"></span>
             <span data-key="header.transport.label">{label}</span>
           </span>"#,
            state = esc(&h.transport.state),
            label = esc(&h.transport.label),
        )
    };
    let meta = if broken {
        format!(
            r#"<span>universe <b class="bad" data-key="header.universe_events">{events}</b> events</span>
               <span>last good <b data-key="header.last_good_ago">{last}</b></span>"#,
            events = h.universe_events,
            last = opt(&h.last_good_ago),
        )
    } else {
        format!(
            r#"<span>universe <b data-key="header.universe_events">{events}</b> events</span>
               <span>refresh <b data-key="header.refresh_secs">{refresh}s</b></span>
               <span>uptime <b data-key="header.uptime">{uptime}</b></span>"#,
            events = h.universe_events,
            refresh = h.refresh_secs,
            uptime = esc(&h.uptime),
        )
    };

    format!(
        r#"<div class="brand">
     <span class="mark"></span>
     <span class="wordmark">polyarb</span>
     <span class="brand-sub">scanner · phase A</span>
   </div>
   <span class="pill pill-mode">{mode}</span>
   <span class="spacer"></span>
   {pill}
   <div class="meta num">{meta}<span class="clock" id="clock">--:--:--</span></div>"#,
        mode = esc(&s.header.mode_pill),
        pill = pill,
        meta = meta,
    )
}

fn hero(s: &DashboardState) -> String {
    let h = &s.hero;
    format!(
        r#"<section class="panel hero">
  <div class="hero-lead">
    <div class="kpil">Soak day <span data-key="hero.soak_day">{day}</span> of <span data-key="hero.soak_days">{days}</span> — is there enough fillable edge to build execution?</div>
    <div class="hero-row">
      <div class="hero-metric num" data-key="hero.survivors">{survivors}</div>
      <div class="hero-qual">opportunities net-positive after spread, fee and depth — and still fillable on re-poll</div>
    </div>
  </div>
  <div class="vrule"></div>
  <div class="stat">
    <div class="kpil">Simulated net edge</div>
    <div class="stat-v num" data-key="hero.net_edge_maker">{edge}</div>
    <div class="stat-n">at walked size · maker basis, hypothetical</div>
  </div>
  <div class="stat">
    <div class="kpil">Median net maker</div>
    <div class="stat-v num"><span data-key="hero.median_net_maker_bps">{maker}</span> <span class="unit">bps</span></div>
    <div class="stat-n">taker basis: <span data-key="hero.median_net_taker_bps">{taker}</span> bps</div>
  </div>
  <div class="stat">
    <div class="kpil">Detect latency</div>
    <div class="stat-v num"><span data-key="hero.latency_p50_ms">{p50}</span> <span class="unit">ms</span></div>
    <div class="stat-n">p50 · p95 <span data-key="hero.latency_p95_ms">{p95}</span>ms</div>
  </div>
</section>"#,
        day = h.soak_day,
        days = h.soak_days,
        survivors = h.survivors,
        edge = esc(&h.net_edge_maker),
        maker = opt(&h.median_net_maker_bps),
        taker = opt(&h.median_net_taker_bps),
        p50 = h
            .latency_p50_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into()),
        p95 = h
            .latency_p95_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into()),
    )
}

fn funnel(s: &DashboardState) -> String {
    let rows: String = s.funnel.stages.iter().map(funnel_row).collect();
    let vanished = s
        .funnel
        .vanished_pct
        .as_deref()
        .map(|p| {
            format!(
                "Of what survived, <b>{}</b> was gone by the first re-poll and is recorded \
                 <span class=\"bad\">vanished</span> — never revised in its favour.",
                esc(p)
            )
        })
        .unwrap_or_else(|| {
            "Nothing has resolved in this window yet, so no re-poll verdict is claimed.".into()
        });
    let partial = if s.funnel.partial_window {
        "<br>Part of this window predates the funnel counters, so the first stages cover \
         less than the window they are labelled with."
    } else {
        ""
    };

    format!(
        r#"<section class="panel funnel">
  <div class="panel-head">
    <h2 class="panel-title">Where the gaps die</h2>
    <span class="panel-note">last <span data-key="funnel.window_hours">{hours}</span>h · executable book sides only, never mid or last</span>
  </div>
  <div class="funnel-rows">{rows}</div>
  <p class="footnote">{vanished} Stages 1–4 count constructions <em>per detection pass</em>; the last counts <em>distinct opportunities</em>, because a re-poll verdict belongs to a row and not to a pass — the two are not divisible into each other.{partial}</p>
</section>"#,
        hours = s.funnel.window_hours,
        rows = rows,
        vanished = vanished,
        partial = partial,
    )
}

fn funnel_row(stage: &FunnelStage) -> String {
    let key = stage.key;
    if !stage.instrumented {
        return format!(
            r#"<div class="frow frow-dark" title="{note}">
     <div class="flabel">{label}</div>
     <div class="ftrack"><div class="fnone">not instrumented</div></div>
     <div class="fcount num">—</div>
     <div class="fpct num">—</div>
   </div>"#,
            note = esc(&stage.note),
            label = esc(&stage.label),
        );
    }
    let tone = match key {
        "detected" => "base",
        "held" => "good",
        _ => "accent",
    };
    format!(
        r#"<div class="frow" title="{note}">
     <div class="flabel">{label}</div>
     <div class="ftrack"><div class="fbar fbar-{tone}" data-key="funnel.{key}.bar_pct" style="width:{bar}%"></div></div>
     <div class="fcount num" data-key="funnel.{key}.count">{count}</div>
     <div class="fpct num" data-key="funnel.{key}.pct">{pct}</div>
   </div>"#,
        note = esc(&stage.note),
        label = esc(&stage.label),
        tone = tone,
        key = key,
        bar = esc(stage.bar_pct.as_deref().unwrap_or("0")),
        count = stage
            .count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".into()),
        pct = opt(&stage.pct),
    )
}

fn table(s: &DashboardState) -> String {
    let rows: String = s.opportunities.iter().map(opp_row).collect();
    let body = if rows.is_empty() {
        format!(
            r#"<div class="empty">none above the {} net floor in this window — that is a measurement, not an absence of edge</div>"#,
            esc(&s.opportunity_note)
        )
    } else {
        rows
    };
    format!(
        r#"<section class="opps">
  <div class="panel-head">
    <h2 class="panel-title">Opportunities</h2>
    <span class="panel-note" data-key="opportunity_note">{note}</span>
  </div>
  <div class="thead">
    <div>Event</div><div>Detector</div><div class="r">Gross</div><div class="r">Spread</div>
    <div class="r">Net mk</div><div class="r">Net tk</div><div class="r">Size</div><div class="r">Verdict</div>
  </div>
  <div id="opp-rows">{body}</div>
</section>"#,
        note = esc(&s.opportunity_note),
        body = body,
    )
}

fn opp_row(o: &OpportunityView) -> String {
    format!(
        r#"<div class="trow num" data-id="{id}">
  <div class="cell-event" title="{title}">{title}<span class="dim"> · {cat} {fee}</span> <span class="lbl lbl-{label_key}">{label}</span></div>
  <div class="det det-{det}">{det}</div>
  <div class="r muted" data-key="opp.{id}.gross_pct">{gross}</div>
  <div class="r muted" data-key="opp.{id}.spread_pct">{spread}</div>
  <div class="r mk" data-key="opp.{id}.net_maker_bps">{mk}</div>
  <div class="r tk {tk_tone}" data-key="opp.{id}.net_taker_bps">{tk}</div>
  <div class="r size" data-key="opp.{id}.size_usd">{size}</div>
  <div class="r"><span class="verdict v-{verdict}" data-key="opp.{id}.verdict_display">{verdict_display}</span></div>
</div>"#,
        id = o.id,
        title = esc(&o.event_title),
        cat = esc(&o.category),
        fee = esc(&o.fee_display),
        label = esc(&o.label),
        label_key = esc(&o.label.replace('-', "")),
        det = esc(&o.detector),
        gross = esc(&o.gross_pct),
        spread = opt(&o.spread_pct),
        mk = opt(&o.net_maker_bps),
        tk = esc(&o.net_taker_bps),
        tk_tone = if o.net_taker_positive { "pos" } else { "neg" },
        size = esc(&o.size_usd),
        verdict = esc(&o.verdict),
        verdict_display = esc(&o.verdict_display),
    )
}

fn rail(s: &DashboardState) -> String {
    let pipeline: String = s
        .pipeline
        .iter()
        .map(|row| {
            format!(
                r#"<div class="prow num"><span class="dot dot-{dot}"></span><span class="pname">{name}</span><span class="pval" data-key="pipeline.{key}">{value}</span></div>"#,
                dot = row.dot,
                name = esc(row.name),
                key = esc(&row.name.to_lowercase().replace(' ', "_")),
                value = esc(&row.value),
            )
        })
        .collect();

    let categories: String = s
        .categories
        .iter()
        .map(|c| {
            format!(
                r#"<div class="crow num"><span class="cname">{name}</span><span class="crate">{rate}</span><span class="ccount">{count}</span><span class="cbps">{bps}</span></div>"#,
                name = esc(&c.name),
                rate = esc(&c.rate_display),
                count = c.count,
                bps = c
                    .median_net_maker_bps
                    .as_deref()
                    .map(|b| esc(&format!("{b}bps")))
                    .unwrap_or_else(|| "—".into()),
            )
        })
        .collect();

    let log: String = s
        .log
        .iter()
        .map(|l| {
            format!(
                r#"<div class="lrow num"><span class="ltime">{time}</span><span class="lbody"><span class="lkind lk-{kind}">{kind}</span> <span class="lsubj">{subject}</span> <span class="ldetail">{detail}</span></span></div>"#,
                time = esc(&l.time),
                kind = esc(l.kind),
                subject = esc(&l.subject),
                detail = esc(&l.detail),
            )
        })
        .collect();

    // The crypto caveat is mandatory wherever a crypto zero appears: a blank cell that
    // reads as "no edge" is a wrong conclusion the dashboard would be causing.
    let caveat = if s.crypto_zero {
        r#"<p class="footnote small">crypto 5m/15m series open and resolve inside the 600s refresh — <b>0 opportunities</b> here means not looked at properly, not no edge.</p>"#
    } else {
        ""
    };

    format!(
        r#"<div class="rail-block">
  <div class="kpil">Pipeline</div>
  <div class="rail-rows" id="pipeline-rows">{pipeline}</div>
</div>
<div class="rail-block">
  <div class="kpil">By category · taker rate</div>
  <div class="rail-rows" id="category-rows">{categories}</div>
  {caveat}
</div>
<div class="rail-block">
  <div class="kpil">Log</div>
  <div class="rail-rows" id="log-rows">{log}</div>
</div>"#,
        pipeline = pipeline,
        categories = categories,
        caveat = caveat,
        log = log,
    )
}

// ---------------------------------------------------------------------------------
// 2d — break state
// ---------------------------------------------------------------------------------

fn break_page(s: &DashboardState, b: &BreakState) -> String {
    let cards: String = b
        .cards
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                r#"<div class="bcard{first}">
     <div class="kpil">{label}</div>
     <div class="bcard-v num {tone}">{value}{suffix}</div>
     {note}
   </div>"#,
                first = if i == 0 { " bcard-first" } else { "" },
                label = esc(&c.label),
                tone = c.value_tone,
                value = esc(&c.value),
                suffix = c
                    .suffix
                    .as_deref()
                    .map(|x| format!(r#" <span class="bcard-sfx">{}</span>"#, esc(x)))
                    .unwrap_or_default(),
                note = c
                    .note
                    .as_deref()
                    .map(|n| format!(r#"<div class="bcard-n">{}</div>"#, esc(n)))
                    .unwrap_or_default(),
            )
        })
        .collect();

    let log: String = b
        .log
        .iter()
        .map(|l| {
            format!(
                r#"<div class="brow num"><span class="ltime">{time}</span> <span class="lkind lk-{kind}">{kind}</span> <span class="lsubj">{subject}</span> <span class="ldetail">{detail}</span></div>"#,
                time = esc(&l.time),
                kind = esc(l.kind),
                subject = esc(&l.subject),
                detail = esc(&l.detail),
            )
        })
        .collect();

    format!(
        r#"<header class="topbar topbar-break">{header}</header>
<div class="break">
  <div class="break-eyebrow">{icon}<span>{eyebrow}</span></div>
  <h1 class="break-head">{headline}</h1>
  <p class="break-body">{body}</p>
  <div class="bcards">{cards}</div>
  <div class="blog">{log}</div>
  <div class="break-actions">
    <a class="btn-outline" href="/api/state">Open the raw state</a>
    <a class="btn-ghost" href="/">Re-read the store</a>
    <span class="spacer"></span>
    <span class="break-note num">read-only · nothing to pause, the daemon cannot trade</span>
  </div>
</div>"#,
        header = header(s, true),
        icon = WARNING_OCTAGON,
        eyebrow = esc(b.eyebrow),
        // Both strings are built from fixed copy plus already-escaped/numeric fragments in
        // `state.rs`; the only interpolations are counts and timestamps we produced.
        headline = b.headline_html,
        body = b.body_html,
        cards = cards,
        log = log,
    )
}
