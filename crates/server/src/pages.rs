//! HTML pages.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_backend::{AgentEvent, AuthStatus, Cli, Provider, UsageWindow};
use maud::{DOCTYPE, Markup, html};
use work_queue::{State as ItemState, WorkItem};
use worker_pool::{
    ActiveJob, Activity, Author, ClaimOrder, DEFAULT_BUILD_JOBS, IssueOrder, ItemMeta,
    NotesVersion, PoolSnapshot, Project, ProjectSuggestions, Scheme, Span, Suggestion,
    SuggestionStatus, UsageLimits,
};

use crate::routes::{
    MAX_BUILD_JOBS, MAX_ISSUE_BATCH, MAX_RETENTION_DAYS, MAX_TURNS, MAX_WEIGHT, MAX_WORKERS,
};
use crate::{AppState, Settings};

/// Agent events shown per worker card.
const CARD_EVENTS: usize = 8;
/// Longest event text shown before it is cut.
const EVENT_CHARS: usize = 240;

const STYLE: &str = r#"
:root {
  color-scheme: light;
  --bg: #eef0f6; --panel: #ffffff; --text: #171b2e; --muted: #5d6580;
  --border: #dde1ee; --accent: #3b5bdb; --on-accent: #ffffff; --link: #2f4bc0;
  --ok: #17935f; --bad: #c9342b; --warn: #b87500; --code: #f1f3fa;
  --accent-soft: #e4e9ff; --ok-soft: #dcf5e9; --warn-soft: #fff0cf; --bad-soft: #ffe1de;
  --track: #e3e6f2; --glow: #dfe6ff;
  --shadow: 0 1px 2px #1b255f14, 0 8px 24px -12px #1b255f33;
}
@media (prefers-color-scheme: dark) {
  :root {
    color-scheme: dark;
    --bg: #0e1120; --panel: #171b30; --text: #e8ebf7; --muted: #9aa3c2;
    --border: #272d4a; --accent: #8ea6ff; --on-accent: #0b1030; --link: #a3b6ff;
    --ok: #4fd09a; --bad: #ff8b82; --warn: #eab654; --code: #1d2240;
    --accent-soft: #232c5a; --ok-soft: #14392c; --warn-soft: #3d2f10; --bad-soft: #442020;
    --track: #262c4a; --glow: #1d2754;
    --shadow: 0 1px 2px #0006, 0 12px 28px -14px #000a;
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg) radial-gradient(900px 400px at 85% -10%, var(--glow), transparent 70%) no-repeat;
  color: var(--text); font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
header { position: sticky; top: 0; z-index: 5; display: flex; gap: 6px; align-items: center; padding: 10px 16px;
  border-bottom: 1px solid var(--border); backdrop-filter: blur(14px);
  background: color-mix(in srgb, var(--bg) 78%, transparent); }
header h1 { font-size: 17px; margin: 0 14px 0 0; display: flex; align-items: center; gap: 10px; }
.logo { width: 28px; height: 28px; border-radius: 9px; display: grid; place-items: center; color: #fff;
  background: linear-gradient(135deg, var(--accent), #7a5cf0); }
header a { padding: 6px 12px; border-radius: 999px; font-weight: 500; }
header a:hover { text-decoration: none; color: var(--text); }
a, a:visited { color: var(--link); text-decoration: none; }
a:hover { text-decoration: underline; }
header a, header a:visited { color: var(--muted); }
header a.here { background: var(--accent-soft); color: var(--accent); font-weight: 600; }
main { max-width: 1200px; margin: 0 auto; padding: 16px; }
h2 { font-size: 18px; margin: 22px 0 10px; letter-spacing: -.01em; }
.panel { background: var(--panel); border: 1px solid var(--border);
  border-radius: 16px; padding: 16px; box-shadow: var(--shadow); }
.bar { display: flex; flex-wrap: wrap; gap: 12px 24px; align-items: center; }
.notice { background: var(--code); border-left: 3px solid var(--accent);
  padding: 10px 12px; border-radius: 6px; margin-bottom: 14px; }
.grid { display: grid; gap: 12px;
  grid-template-columns: repeat(auto-fill, minmax(340px, 1fr)); }
.card h3 { margin: 0 0 6px; font-size: 15px; display: flex; justify-content: space-between; }
.muted { color: var(--muted); }
.ok { color: var(--ok); } .bad { color: var(--bad); } .warn { color: var(--warn); }
.now { background: var(--code); border-radius: 6px; padding: 8px 10px; margin-top: 8px;
  overflow-wrap: anywhere; }
.now code { background: none; padding: 0; }
.timer { font-variant-numeric: tabular-nums; color: var(--muted); }
ul.events { list-style: none; margin: 8px 0 0; padding: 0; font-size: 13px; }
ul.events li { padding: 3px 0; border-top: 1px solid var(--border);
  overflow-wrap: anywhere; }
code, pre { font-family: ui-monospace, "SF Mono", Menlo, monospace; font-size: 13px; }
code { background: var(--code); padding: 1px 4px; border-radius: 4px; }
pre { background: var(--code); padding: 12px; border-radius: 6px;
  white-space: pre-wrap; overflow-wrap: anywhere; }
table { width: 100%; border-collapse: collapse; }
td, th { text-align: left; padding: 6px 8px; border-top: 1px solid var(--border);
  vertical-align: top; overflow-wrap: anywhere; }
th { color: var(--muted); font-weight: 500; border-top: 0; }
form.inline { display: inline; }
details.menu { display: inline-block; position: relative; }
details.menu summary { cursor: pointer; color: var(--link); list-style: none; }
details.menu[open] > div { position: absolute; right: 0; z-index: 1; display: flex;
  flex-direction: column; gap: 6px; margin-top: 4px; padding: 8px; white-space: nowrap;
  background: var(--panel); border: 1px solid var(--border); border-radius: 6px; }
button, input, select { font: inherit; color: inherit; }
button { background: var(--panel); border: 1px solid var(--border);
  border-radius: 10px; padding: 5px 14px; cursor: pointer; font-weight: 500; }
button.primary { background: var(--accent); border-color: var(--accent); color: var(--on-accent);
  font-weight: 600; }
button:hover { border-color: var(--accent); }
button:disabled { opacity: .5; cursor: default; }
input[type=text], input[type=password], input[type=number], select, textarea {
  background: var(--bg); border: 1px solid var(--border); border-radius: 6px;
  padding: 5px 8px; width: 100%; }
label { display: block; margin: 8px 0 2px; font-size: 13px; color: var(--muted); }
label.check { display: flex; gap: 6px; align-items: center; color: var(--text); }
label.check input { width: auto; }
#fallbacks { list-style: none; padding: 0; margin: 0 0 8px; }
#fallbacks li { display: flex; gap: 10px; align-items: center; padding: 6px 0; }
#fallbacks li.dragging { opacity: .4; }
#fallbacks .handle { cursor: grab; color: var(--muted); }
#fallbacks select { width: auto; min-width: 12em; }
.fields { display: grid; gap: 4px 12px;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); }
.stack > * + * { margin-top: 12px; }

.filters { display: grid; gap: 8px 12px; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
  align-items: end; }
.filters label { margin: 0 0 2px; }
.filters .wide { grid-column: span 2; }
.stats { display: grid; gap: 12px; grid-template-columns: repeat(auto-fill, minmax(190px, 1fr)); }
.stat strong { display: block; font-size: 22px; font-variant-numeric: tabular-nums; }
.small { font-size: 12px; }
.charts { display: grid; gap: 12px; grid-template-columns: repeat(auto-fill, minmax(380px, 1fr)); }
.chart { position: relative; }
.chart h3 { margin: 0 0 8px; font-size: 14px; }
.chart > div { position: relative; height: 240px; }
.chart.empty > div::after { content: "No data for these filters"; position: absolute; inset: 0;
  display: flex; align-items: center; justify-content: center; color: var(--muted);
  background: var(--panel); }
td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
table.items { min-width: 1150px; }
table.items td, table.items th { overflow-wrap: normal; }
table.items td:first-child { overflow-wrap: anywhere; min-width: 150px; }
table.items th.sortable { cursor: pointer; white-space: nowrap; }
table.items tr.row { cursor: pointer; }
table.items tr.row:hover, table.items tr.row:focus-visible { background: var(--code); outline: none; }
table.items tr.detail > td { background: var(--bg); padding: 10px 8px 14px; }
table.turns th, table.turns td { font-size: 12px; }
.tag { margin-left: 6px; padding: 0 5px; border: 1px solid var(--border); border-radius: 4px;
  font-size: 11px; color: var(--muted); }
a.pr { margin-left: 8px; font-size: 12px; }
.err { max-width: 40ch; }
details.comment { margin-top: 8px; }
details.comment summary { cursor: pointer; color: var(--link); }
details.comment textarea { margin: 8px 0; min-height: 4.5em; }
.suggestion p.text { margin: 8px 0; white-space: pre-wrap; overflow-wrap: anywhere; }
.suggestion .remark { margin: 4px 0; padding-left: 8px; border-left: 3px solid var(--border); }
.suggestion .actions { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 8px; }
.scroll { overflow-x: auto; }
@media (max-width: 600px) { header { flex-wrap: wrap; gap: 10px 16px; } }

/* Provider marks, status strip and pills. */
.pmark { --pbg: #5d6580; flex: none; width: 34px; height: 34px; border-radius: 11px; display: grid; place-items: center;
  background: var(--pbg); color: #fff; font-weight: 700; overflow: hidden; box-shadow: inset 0 0 0 1px #ffffff26; }
.pmark i { width: 20px; height: 20px; background: #fff; -webkit-mask: var(--logo) center / contain no-repeat;
  mask: var(--logo) center / contain no-repeat; }
.pmark img { width: 100%; height: 100%; display: block; }
.pill { display: inline-flex; align-items: center; gap: 7px; padding: 2px 11px 2px 9px; border-radius: 999px;
  font-size: 13px; font-weight: 600; white-space: nowrap; }
.pill.ok { background: var(--ok-soft); color: var(--ok); } .pill.warn { background: var(--warn-soft); color: var(--warn); }
.pill.bad { background: var(--bad-soft); color: var(--bad); } .pill.idle { background: var(--code); color: var(--muted); }
.dot { width: 8px; height: 8px; border-radius: 50%; background: currentColor; }
.pill.ok .dot { animation: pulse 1.8s ease-out infinite; }
@keyframes pulse { 0% { box-shadow: 0 0 0 0 color-mix(in srgb, currentColor 55%, transparent); } 100% { box-shadow: 0 0 0 8px transparent; } }
.stepper { display: inline-flex; align-items: center; gap: 2px; border: 1px solid var(--border); border-radius: 10px; background: var(--panel); }
.stepper button { border: 0; background: none; width: 30px; height: 30px; padding: 0; font-size: 16px; }
.stepper b { min-width: 24px; text-align: center; }
.spacer { flex: 1; }

/* Plan usage: a compact card per provider, a bar per window. */
.usage { display: grid; gap: 12px; grid-template-columns: repeat(auto-fill, minmax(min(100%, 250px), 1fr)); }
.ucard { background: var(--panel); border: 1px solid var(--border); border-radius: 16px; padding: 12px 14px;
  box-shadow: var(--shadow); min-width: 0; }
.ucard.limited { border-color: color-mix(in srgb, var(--bad) 55%, var(--border)); }
.uhead { display: flex; align-items: center; gap: 9px; }
.uhead h3 { margin: 0; font-size: 14px; line-height: 1.25; overflow-wrap: anywhere; } .uhead .pill { margin-left: auto; font-size: 12px; padding: 0 9px; }
.ucard .pmark { width: 28px; height: 28px; border-radius: 9px; } .ucard .pmark i { width: 16px; height: 16px; }
.meter { margin-top: 9px; }
.mtop { display: flex; justify-content: space-between; align-items: baseline; gap: 14px; font-size: 12.5px; margin-bottom: 4px; }
.mlab { font-weight: 600; } .mlab .muted { font-weight: 400; font-size: 12px; }
.mtop strong { font-variant-numeric: tabular-nums; white-space: nowrap; margin-left: auto; padding-left: 6px; }
.mbar { position: relative; height: 8px; border-radius: 999px; background: var(--track); }
.mbar b { display: block; height: 100%; border-radius: 999px; background: var(--ok); }
.mbar.warn b { background: var(--warn); } .mbar.bad b { background: var(--bad); }
.mbar i { position: absolute; top: -3px; bottom: -3px; width: 2px; margin-left: -1px; border-radius: 2px; background: var(--text); }
.ustate { margin-top: 9px; padding: 6px 10px; background: var(--code); border-radius: 10px; font-size: 12.5px; }
.ustate form { margin-left: 6px; } .ustate button { padding: 1px 10px; font-size: 12.5px; }

/* Workers: a timeline of what the agent just did. */
.wcard.idle { background: transparent; box-shadow: none; border-style: dashed; }
.wtop { display: flex; justify-content: space-between; align-items: center; gap: 10px; margin-bottom: 6px; }
.wtop h3 { margin: 0; font-size: 16px; }
.chips { display: flex; flex-wrap: wrap; gap: 6px; margin: 8px 0; }
.chip { font-size: 12px; padding: 1px 9px; border-radius: 999px; background: var(--code); color: var(--muted); }
.chip.acc { background: var(--accent-soft); color: var(--accent); font-weight: 600; }
ul.timeline { list-style: none; margin: 10px 0 0; padding: 0 0 0 4px; position: relative; }
ul.timeline::before { content: ""; position: absolute; left: 13px; top: 8px; bottom: 8px; width: 2px; background: var(--border); }
ul.timeline li { position: relative; display: flex; gap: 10px; padding: 4px 0; font-size: 13px; overflow-wrap: anywhere; }
ul.timeline .ic { flex: none; width: 20px; height: 20px; border-radius: 7px; display: grid; place-items: center; z-index: 1;
  background: var(--panel); border: 1px solid var(--border); color: var(--muted); font-size: 11px; }
ul.timeline li.cur .ic { background: var(--accent); border-color: var(--accent); color: var(--on-accent); animation: pulse2 1.6s ease-out infinite; }
@keyframes pulse2 { 0% { box-shadow: 0 0 0 0 color-mix(in srgb, var(--accent) 50%, transparent); } 100% { box-shadow: 0 0 0 9px transparent; } }

/* Queue: a lane per state. Collapse, expand and filters are checkboxes, so they need no script. */
.qwrap input { position: absolute; opacity: 0; width: 1px; height: 1px; pointer-events: none; }
.qfilters { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-bottom: 12px; }
.qlab { font-size: 12px; font-weight: 600; letter-spacing: .06em; text-transform: uppercase; color: var(--muted); }
.qchip { display: inline-flex; align-items: center; gap: 7px; border: 1px solid var(--border); background: var(--panel);
  border-radius: 999px; padding: 4px 12px; font-weight: 600; font-size: 13.5px; cursor: pointer; }
.qchip i { width: 8px; height: 8px; border-radius: 3px; background: var(--hue); }
.qchip:hover { border-color: var(--accent); }
.qfilters > input:checked + .qchip { background: var(--accent-soft); border-color: var(--accent); color: var(--accent); }
.qfilters > input:checked + .qchip.attn { background: var(--bad-soft); border-color: var(--bad); color: var(--bad); }
.qfilters > input:focus-visible + .qchip { outline: 2px solid var(--accent); outline-offset: 2px; }
.lanes { display: flex; flex-direction: column; gap: 12px; }
.sw { display: grid; grid-template-columns: 150px minmax(0, 1fr); gap: 12px; background: var(--code); border-radius: 18px; padding: 10px; }
.sw.empty { opacity: .75; }
.sw-head { display: flex; flex-direction: column; gap: 2px; align-self: start; padding: 8px 10px; border-radius: 12px; font-weight: 600; font-size: 13.5px; cursor: pointer; }
.sw-head:hover { background: var(--panel); }
.sw-head .nm { display: flex; align-items: center; gap: 8px; } .sw-head i { width: 9px; height: 9px; border-radius: 50%; background: var(--lane); }
.sw-head .count { font-size: 30px; line-height: 1.1; font-weight: 700; font-variant-numeric: tabular-nums; }
.sw-head .tog { font-size: 12px; font-weight: 500; color: var(--muted); }
.tog::after { content: "Hide"; }
.sw-body { display: grid; gap: 10px; min-width: 0; align-content: start; grid-template-columns: repeat(auto-fill, minmax(min(100%, 250px), 1fr)); }
.kcard { position: relative; background: var(--panel); border: 1px solid var(--border); border-radius: 14px; padding: 12px; min-width: 0;
  box-shadow: 0 1px 2px #1b255f10; }
.kcard.attn { border-color: color-mix(in srgb, var(--bad) 45%, var(--border)); }
.kproj { display: flex; align-items: center; gap: 6px; font-size: 12px; color: var(--muted); font-family: ui-monospace, "SF Mono", Menlo, monospace; }
.kproj i { width: 8px; height: 8px; border-radius: 3px; background: var(--hue); }
.ktitle { font-weight: 600; margin: 4px 0 8px; overflow-wrap: anywhere; line-height: 1.3; }
.kmeta { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; font-size: 12px; color: var(--muted); }
.kstate { margin-top: 8px; font-size: 12.5px; overflow-wrap: anywhere; }
.kacts { display: flex; flex-wrap: wrap; gap: 6px 10px; align-items: center; margin-top: 10px; font-size: 13px; }
.kacts button { padding: 2px 10px; font-size: 13px; }
.ring { width: 22px; height: 22px; flex: none; }
.kcard.more { display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 2px; text-align: center;
  min-height: 84px; background: transparent; border-style: dashed; color: var(--accent); cursor: pointer; box-shadow: none; }
.kcard.more b { font-size: 15px; } .kcard.more span { font-size: 12.5px; color: var(--muted); } .kcard.more:hover { background: var(--panel); }
.kcard.fewer, .sw-body .extra { display: none; }
.kempty { grid-column: 1 / -1; padding: 10px 12px; border: 1px dashed var(--border); border-radius: 12px; color: var(--muted); font-size: 13px; }
.qwrap:has(#qa:checked) .kcard.card:not(.attn) { display: none; }
@media (max-width: 640px) {
  .sw { grid-template-columns: 1fr; }
  .sw-head { flex-direction: row; align-items: center; gap: 10px; width: 100%; } .sw-head .tog { margin-left: auto; } .sw-head .count { font-size: 22px; }
}
#col-open:checked ~ #queue #lane-open .sw-body { display: none; }
#col-open:checked ~ #queue #lane-open .tog::after { content: "Show"; }
#exp-open:checked ~ #queue #lane-open .extra { display: block; }
#exp-open:checked ~ #queue #lane-open .kcard.more { display: none; }
#exp-open:checked ~ #queue #lane-open .kcard.fewer { display: flex; }
#col-open:focus-visible ~ #queue #lane-open .sw-head, #exp-open:focus-visible ~ #queue #lane-open .kcard.more { outline: 2px solid var(--accent); outline-offset: 2px; }
#col-inprogress:checked ~ #queue #lane-inprogress .sw-body { display: none; }
#col-inprogress:checked ~ #queue #lane-inprogress .tog::after { content: "Show"; }
#exp-inprogress:checked ~ #queue #lane-inprogress .extra { display: block; }
#exp-inprogress:checked ~ #queue #lane-inprogress .kcard.more { display: none; }
#exp-inprogress:checked ~ #queue #lane-inprogress .kcard.fewer { display: flex; }
#col-inprogress:focus-visible ~ #queue #lane-inprogress .sw-head, #exp-inprogress:focus-visible ~ #queue #lane-inprogress .kcard.more { outline: 2px solid var(--accent); outline-offset: 2px; }
#col-inreview:checked ~ #queue #lane-inreview .sw-body { display: none; }
#col-inreview:checked ~ #queue #lane-inreview .tog::after { content: "Show"; }
#exp-inreview:checked ~ #queue #lane-inreview .extra { display: block; }
#exp-inreview:checked ~ #queue #lane-inreview .kcard.more { display: none; }
#exp-inreview:checked ~ #queue #lane-inreview .kcard.fewer { display: flex; }
#col-inreview:focus-visible ~ #queue #lane-inreview .sw-head, #exp-inreview:focus-visible ~ #queue #lane-inreview .kcard.more { outline: 2px solid var(--accent); outline-offset: 2px; }
#col-hold:checked ~ #queue #lane-hold .sw-body { display: none; }
#col-hold:checked ~ #queue #lane-hold .tog::after { content: "Show"; }
#exp-hold:checked ~ #queue #lane-hold .extra { display: block; }
#exp-hold:checked ~ #queue #lane-hold .kcard.more { display: none; }
#exp-hold:checked ~ #queue #lane-hold .kcard.fewer { display: flex; }
#col-hold:focus-visible ~ #queue #lane-hold .sw-head, #exp-hold:focus-visible ~ #queue #lane-hold .kcard.more { outline: 2px solid var(--accent); outline-offset: 2px; }
#col-closed:checked ~ #queue #lane-closed .sw-body { display: none; }
#col-closed:checked ~ #queue #lane-closed .tog::after { content: "Show"; }
#exp-closed:checked ~ #queue #lane-closed .extra { display: block; }
#exp-closed:checked ~ #queue #lane-closed .kcard.more { display: none; }
#exp-closed:checked ~ #queue #lane-closed .kcard.fewer { display: flex; }
#col-closed:focus-visible ~ #queue #lane-closed .sw-head, #exp-closed:focus-visible ~ #queue #lane-closed .kcard.more { outline: 2px solid var(--accent); outline-offset: 2px; }
@media (prefers-reduced-motion: reduce) { * { animation: none !important; } }
"#;

/// Keeps the page's `data-live` sections current: every two seconds while
/// the tab is visible, fetches the page again and swaps in each section
/// whose content changed. A section is left alone while a field in it has
/// focus or an open menu, so nothing is replaced under the user's hands. It
/// also closes an open menu when the user clicks outside it.
/// One draggable row of the fallback list; a blank one is the template that
/// "Add provider" clones.
fn fallback_row(chosen: Option<Provider>) -> Markup {
    html! {
        li draggable="true" {
            span.handle title="Drag to reorder" { "⠿" }
            select name="fallback" {
                @for p in Provider::ALL {
                    option value=(provider_value(p)) selected[chosen == Some(p)] { (p.label()) }
                }
            }
            button type="button" .remove { "Remove" }
        }
    }
}

const LIVE_SCRIPT: &str = r#"
(() => {
  const url = location.pathname + "?live=1";
  async function refresh() {
    if (document.hidden) return;
    let doc;
    try {
      const res = await fetch(url, { cache: "no-store" });
      if (!res.ok) return;
      doc = new DOMParser().parseFromString(await res.text(), "text/html");
    } catch (_) {
      return;
    }
    for (const section of document.querySelectorAll("[data-live]")) {
      const next = doc.getElementById(section.id);
      const busy = section.querySelector("details[open]") ||
        (section.contains(document.activeElement) &&
          document.activeElement.matches("input, select, textarea"));
      if (next && !busy && next.innerHTML !== section.innerHTML) {
        section.innerHTML = next.innerHTML;
      }
    }
  }
  // A click anywhere outside an open menu closes it, so only one is open.
  document.addEventListener("click", (e) => {
    for (const menu of document.querySelectorAll("details.menu[open]")) {
      if (!menu.contains(e.target)) menu.removeAttribute("open");
    }
  });
  setInterval(refresh, 2000);
  document.addEventListener("visibilitychange", refresh);
})();
"#;

fn layout(title: &str, here: &str, refresh: bool, notice: Option<String>, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                @if refresh {
                    // Without scripts, the whole page reloads instead.
                    noscript { meta http-equiv="refresh" content="2"; }
                }
                title { (title) " · AI Worker Control" }
                style { (maud::PreEscaped(STYLE)) }
            }
            body {
                header {
                    h1 {
                        span.logo aria-hidden="true" {
                            svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" {
                                circle cx="8" cy="8" r="2.2" {}
                                path d="M8 1.5v2.3M8 12.2v2.3M1.5 8h2.3M12.2 8h2.3" {}
                            }
                        }
                        "AI Worker Control"
                    }
                    a href="/" class=[(here == "dashboard").then_some("here")] { "Dashboard" }
                    a href="/history" class=[(here == "history").then_some("here")] { "History" }
                    a href="/suggestions" class=[(here == "suggestions").then_some("here")] { "Suggestions" }
                    a href="/settings" class=[(here == "settings").then_some("here")] { "Settings" }
                }
                main {
                    @if let Some(notice) = notice { div.notice { (notice) } }
                    (body)
                }
                @if refresh { script { (maud::PreEscaped(LIVE_SCRIPT)) } }
            }
        }
    }
}

/// The history dashboard's page. Its filters, figures and charts are drawn
/// in the browser from `/history.json`.
pub(crate) fn history(notice: Option<String>) -> Markup {
    let chart = |id: &str, title: &str| {
        html! { div.panel.chart { h3 { (title) } div { canvas #(id) {} } } }
    };
    let field = |id: &str, label: &str| {
        html! { div { label for=(id) { (label) } select #(id) {} } }
    };
    let body = html! {
        h2 { "History" }
        p #error.bad hidden {}
        noscript { p.bad { "The history dashboard needs JavaScript." } }
        div.panel {
            div.filters {
                div.wide { label for="f-q" { "Search" }
                    input #f-q type="text" placeholder="Item, model, error, pull request…"; }
                (field("f-project", "Project"))
                (field("f-provider", "Provider"))
                (field("f-model", "Model"))
                (field("f-effort", "Effort"))
                (field("f-state", "State"))
                div { label for="f-from" { "From" } input #f-from type="date"; }
                div { label for="f-to" { "To" } input #f-to type="date"; }
                div { button #f-clear type="button" { "Clear filters" } }
            }
        }
        h2 { "Summary" }
        div.stats #stats {}
        h2 { "Charts" }
        div.charts {
            (chart("c-turns", "Runs per day, by provider"))
            (chart("c-tokens", "Tokens per day (as the CLIs count them)"))
            (chart("c-cost", "Cost per day, as the CLIs estimate it"))
            (chart("c-models", "Agent hours by model and effort"))
            (chart("c-quota", "Plan usage spent, by provider"))
            (chart("c-outcomes", "How runs ended"))
            (chart("c-failures", "Why runs failed"))
            (chart("c-turns-per-item", "Runs needed per finished item"))
        }
        h2 { "Breakdown" }
        div.panel {
            label for="group-by" { "Group runs by" }
            select #group-by {}
            div.scroll #breakdown {}
        }
        h2 { "Bodies of work " span.muted #count {} }
        div.panel.scroll #items {}
        p.muted.small {
            "Plan usage is the change in the provider's reported session and weekly allowance across \
             a run; providers report it for the whole plan, not per model, so with several workers \
             on one provider the figures overlap."
        }
        script src="/static/chart.umd.js" {}
        script src="/static/history.js" {}
    };
    layout("History", "history", false, notice, body)
}

/// The queue's lanes, top to bottom: state, title and dot color.
const LANES: [(ItemState, &str, &str); 5] = [
    (ItemState::Open, "Open", "var(--muted)"),
    (ItemState::InProgress, "In progress", "var(--accent)"),
    (ItemState::InReview, "In review", "var(--warn)"),
    (ItemState::Hold, "Hold", "var(--bad)"),
    (ItemState::Closed, "Closed", "var(--ok)"),
];
/// Cards a lane shows before the rest go behind its "more" tile.
const LANE_CARDS: usize = 3;

/// One item of the queue, ready to draw.
struct QueueCard<'a> {
    item: &'a WorkItem,
    state: ItemState,
    meta: ItemMeta,
    /// What the item is waiting on, in a sentence.
    status: String,
    /// The color class of `status`.
    tone: &'static str,
    /// Whether the item needs the user's eye: held, failing or stuck.
    attn: bool,
    /// Lower sorts first within a lane.
    rank: u8,
    pull_request: Option<String>,
    has_worktree: bool,
    worker: Option<usize>,
    max_turns: u32,
}

fn queue_card<'a>(
    app: &AppState,
    settings: &Settings,
    snapshot: &PoolSnapshot,
    projects: &[Project],
    state: ItemState,
    item: &'a WorkItem,
) -> QueueCard<'a> {
    let meta = app.pool().meta(state, item).unwrap_or_default();
    let max_turns = settings.max_turns.max(1);
    let used = format!("{} of {max_turns} turns used", meta.turns);
    let known = settings.projects.iter().any(|p| p.name == item.project);
    let (status, tone, rank, attn) = match state {
        ItemState::Open => match &meta.last_error {
            Some(error) => (
                format!("retrying ({used}); last run: {error}"),
                "warn",
                1,
                true,
            ),
            None if !known => (
                format!("no project “{}” configured", item.project),
                "muted",
                1,
                true,
            ),
            None => ("open".to_string(), "muted", 3, false),
        },
        ItemState::InProgress => ("in progress".to_string(), "warn", 3, false),
        ItemState::Hold => {
            let reason = meta
                .hold_reason
                .clone()
                .unwrap_or_else(|| "set aside for manual review".into());
            (format!("on hold: {reason}"), "bad", 0, true)
        }
        ItemState::InReview if meta.needs_rework.is_some() => {
            let last = meta
                .last_error
                .as_ref()
                .map(|e| format!("; last run: {e}"))
                .unwrap_or_default();
            (
                format!("changes requested; waiting for a worker ({used}){last}"),
                "warn",
                0,
                true,
            )
        }
        ItemState::InReview if meta.ci_running => (
            format!("waiting for CI to finish ({used})"),
            "warn",
            2,
            false,
        ),
        ItemState::InReview => (format!("in review ({used})"), "warn", 3, false),
        ItemState::Closed => ("closed".to_string(), "ok", 4, false),
    };
    let pull_request = match state {
        ItemState::InReview => meta.pr_url.clone(),
        ItemState::Closed => app.pool().pull_request(state, item),
        _ => None,
    };
    let worker = snapshot.workers.iter().find_map(|w| match &w.activity {
        Activity::Working(job) if job.item == *item => Some(w.id),
        _ => None,
    });
    QueueCard {
        item,
        state,
        has_worktree: state == ItemState::Closed
            && projects
                .iter()
                .find(|p| p.name == item.project)
                .is_some_and(|p| p.worktree_path(item).exists()),
        meta,
        status,
        tone,
        attn,
        rank,
        pull_request,
        worker,
        max_turns,
    }
}

/// A project's accent color, the same each time it is drawn.
fn project_hue(name: &str) -> u32 {
    name.bytes()
        .fold(7_u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)))
        % 360
}

/// `name` as a fragment of an HTML id or class.
fn slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// A ring showing how much of an item's turn allowance is spent.
fn turn_ring(turns: u32, max: u32) -> Markup {
    let circumference = 2.0 * std::f64::consts::PI * 8.0;
    let spent = f64::from(turns.min(max)) / f64::from(max.max(1));
    let tone = if turns >= max {
        "var(--bad)"
    } else if turns + 1 >= max {
        "var(--warn)"
    } else {
        "var(--accent)"
    };
    html! {
        svg.ring viewBox="0 0 22 22" aria-hidden="true" {
            circle cx="11" cy="11" r="8" fill="none" stroke="var(--track)" stroke-width="3" {}
            circle cx="11" cy="11" r="8" fill="none" stroke=(tone) stroke-width="3" stroke-linecap="round"
                stroke-dasharray={ (format!("{:.1} {circumference:.1}", circumference * spent)) }
                transform="rotate(-90 11 11)" {}
        }
    }
}

fn queue_card_markup(c: &QueueCard, extra: bool) -> Markup {
    let item = c.item;
    let encoded = url_encode(&item.to_string());
    let class = format!(
        "kcard card p-{}{}{}",
        slug(&item.project),
        if c.attn { " attn" } else { "" },
        if extra { " extra" } else { "" },
    );
    html! {
        div class=(class) style={ "--hue: hsl(" (project_hue(&item.project)) " 65% 55%)" } {
            div.kproj { i {} (item.project) }
            div.ktitle title=(item) { (item.file) }
            div.kmeta {
                (turn_ring(c.meta.turns, c.max_turns))
                span { (c.meta.turns) " of " (c.max_turns) " turns" }
                @if let Some(id) = c.worker { span.pill.ok { span.dot {} "Worker " (id + 1) } }
            }
            div.kstate.(c.tone) { (c.status) }
            div.kacts {
                @match c.state {
                    ItemState::Hold => {
                        form.inline method="post" action="/resume" {
                            input type="hidden" name="item" value=(item);
                            button title="Reset its turn count and give it back to the agents" { "Return to agent" }
                        }
                        form.inline method="post" action="/close" {
                            input type="hidden" name="item" value=(item);
                            button title="Close it without more work, such as after its pull request was closed" { "Close" }
                        }
                    }
                    ItemState::InReview => {
                        @if let Some(url) = &c.pull_request {
                            a href=(url) target="_blank" rel="noopener" { "Pull request" }
                        }
                        a href={ "/result?item=" (encoded) } { "Result" }
                        @if c.pull_request.is_some() {
                            details.menu {
                                summary { "Next action" }
                                div {
                                    form.inline method="post" action="/review/check" {
                                        input type="hidden" name="item" value=(item);
                                        button title="Read the latest reviews, comments and CI results now, and queue a rework if they ask for changes" { "Check latest" }
                                    }
                                    form.inline method="post" action="/review/rework" {
                                        input type="hidden" name="item" value=(item);
                                        button title="Act on feedback on the latest commit even if a rework already answered it, for when that rework changed nothing" { "Rework again" }
                                    }
                                    form.inline method="post" action="/review/bump" {
                                        input type="hidden" name="item" value=(item);
                                        button title="Push an empty commit to the pull request so the remote checks run again" { "Bump PR" }
                                    }
                                }
                            }
                        }
                    }
                    ItemState::Closed => {
                        a href={ "/result?item=" (encoded) } { "Result" }
                        @if let Some(url) = &c.pull_request {
                            a href=(url) target="_blank" rel="noopener" { "Pull request" }
                        }
                        @if c.has_worktree {
                            form.inline method="post" action="/cleanup" {
                                input type="hidden" name="item" value=(item);
                                button title="Remove the worktree; delete the branch too if it is merged" { "Clean up" }
                            }
                        }
                    }
                    ItemState::Open | ItemState::InProgress => {}
                }
            }
        }
    }
}

/// One lane of the queue. Past `LANE_CARDS` items, the rest wait behind a
/// "more" tile; held items are never hidden.
fn lane(state: ItemState, title: &str, color: &str, mut cards: Vec<QueueCard>) -> Markup {
    let dir = state.dir_name();
    // Items that need attention come first; the rest keep their order.
    cards.sort_by_key(|c| {
        let spent = if state == ItemState::Closed {
            0
        } else {
            c.meta.turns
        };
        (c.rank, std::cmp::Reverse(spent))
    });
    let shown = if state == ItemState::Hold || cards.len() <= LANE_CARDS {
        cards.len()
    } else {
        LANE_CARDS - 1
    };
    let hidden = cards.len() - shown;
    let hidden_attn = cards.iter().skip(shown).filter(|c| c.attn).count();
    html! {
        section.sw id={ "lane-" (dir) } class=[cards.is_empty().then_some("empty")] style={ "--lane: " (color) } {
            label.sw-head for={ "col-" (dir) } {
                span.nm { i {} (title) }
                span.count { (cards.len()) }
                span.tog {}
            }
            div.sw-body {
                @for (i, card) in cards.iter().enumerate() { (queue_card_markup(card, i >= shown)) }
                @if hidden > 0 {
                    label.kcard.more for={ "exp-" (dir) } {
                        b { "+" (hidden) " more" }
                        span { @if hidden_attn > 0 { (hidden_attn) " need attention" } @else { "all on track" } }
                    }
                    label.kcard.fewer for={ "exp-" (dir) } { b { "Show fewer" } }
                }
                @if cards.is_empty() { div.kempty { "Nothing here" } }
            }
        }
    }
}

/// The queue: project and attention filters, and a lane per state. Every
/// toggle is a checkbox the lanes and cards react to in CSS, so none of it
/// is lost when the live section is swapped.
fn queue_section(
    app: &AppState,
    settings: &Settings,
    snapshot: &PoolSnapshot,
    items: &[(ItemState, Vec<WorkItem>)],
) -> Markup {
    let projects = app.pool().config().projects;
    let names: BTreeSet<&str> = items
        .iter()
        .flat_map(|(_, list)| list.iter().map(|i| i.project.as_str()))
        .chain(settings.projects.iter().map(|p| p.name.as_str()))
        .collect();
    let filter_css: String = names
        .iter()
        .map(|name| {
            let slug = slug(name);
            format!(
                ".qwrap:has(.pf:checked):not(:has(#pf-{slug}:checked)) .kcard.p-{slug} {{ display: none !important; }}\n"
            )
        })
        .collect();
    let held = items
        .iter()
        .any(|(state, list)| *state == ItemState::Hold && !list.is_empty());
    html! {
        div.qwrap {
            style { (maud::PreEscaped(filter_css)) ".qwrap:has(#qa:checked) .kcard.card:not(.attn) { display: none !important; }" }
            div.qfilters {
                span.qlab { "Project" }
                @for name in &names {
                    input.pf type="checkbox" id={ "pf-" (slug(name)) };
                    label.qchip for={ "pf-" (slug(name)) } style={ "--hue: hsl(" (project_hue(name)) " 65% 55%)" } { i {} (name) }
                }
                span.spacer {}
                input #qa type="checkbox";
                label.qchip.attn for="qa" { "Needs attention" }
            }
            @for (state, _) in items {
                input.col type="checkbox" id={ "col-" (state.dir_name()) } checked[*state == ItemState::Closed];
                input.exp type="checkbox" id={ "exp-" (state.dir_name()) };
            }
            div #queue data-live {
                @if held {
                    div.bar style="margin-bottom:12px" {
                        form.inline method="post" action="/resume-all" {
                            button title="Reset the turn count of every held item and give them all back to the agents" { "Return all held to agent" }
                        }
                        form.inline method="post" action="/close-all" onsubmit="return confirm('Close every held item without more work?')" {
                            button title="Close every held item without more work" { "Close all held" }
                        }
                    }
                }
                div.lanes {
                    @for (state, title, color) in LANES {
                        @let list = items.iter().find(|(s, _)| *s == state).map(|(_, l)| l.as_slice()).unwrap_or_default();
                        (lane(
                            state,
                            title,
                            color,
                            list.iter().map(|i| queue_card(app, settings, snapshot, &projects, state, i)).collect(),
                        ))
                    }
                }
            }
        }
    }
}

pub(crate) fn dashboard(app: &AppState, notice: Option<String>) -> Markup {
    let snapshot = app.pool().snapshot();
    let settings = app.settings();
    let queue = app.pool().queue();
    let items: Vec<_> = ItemState::ALL
        .iter()
        .map(|s| (*s, queue.list(*s).unwrap_or_default()))
        .collect();
    let provider = settings.provider(settings.active);
    let auth = match provider {
        Some(p) if p.use_api_key || !settings.active.supports_plan() => "API key",
        _ => "plan login",
    };

    let body = html! {
        div #status data-live {
            div.panel.bar {
                @if snapshot.running {
                    span.pill.ok { span.dot {} "Running" }
                } @else {
                    span.pill.idle { "Stopped" }
                }
                div { strong { (settings.active.label()) } " " span.muted { "(" (auth) ")" } }
                div.bar {
                    span.muted { "Workers" }
                    span.stepper {
                        form.inline method="post" action="/workers" {
                            input type="hidden" name="count" value=(settings.workers.saturating_sub(1));
                            button disabled[settings.workers == 0] aria-label="Fewer workers" { "−" }
                        }
                        b { (settings.workers) }
                        form.inline method="post" action="/workers" {
                            input type="hidden" name="count" value=(settings.workers + 1);
                            button disabled[settings.workers >= MAX_WORKERS] aria-label="More workers" { "+" }
                        }
                    }
                }
                span.spacer {}
                @if snapshot.running {
                    form.inline method="post" action="/stop" { button { "Stop" } }
                } @else {
                    form.inline method="post" action="/start" { button.primary { "Start" } }
                }
            }
            @if let Some(error) = &snapshot.last_error {
                p.bad { "Could not read the work queue: " (error) }
            }
        }

        div #usage data-live {
            h2 { "Plan usage" }
            (usage_panel(&snapshot, &settings.limits))
            p.muted { "Balancing: " (settings.balancing.scheme.label()) }
        }

        div #workers data-live {
            h2 { "Workers" }
            @if snapshot.workers.is_empty() {
                p.muted { "No workers running. Press Start to begin." }
            }
            div.grid {
                @for worker in &snapshot.workers {
                    @match &worker.activity {
                        Activity::Idle => {
                            div.panel.card.wcard.idle {
                                div.wtop { h3 { "Worker " (worker.id + 1) } span.pill.idle { "idle" } }
                                @if worker.id >= snapshot.target_workers || !snapshot.running {
                                    p.muted { "Stopping." }
                                } @else if snapshot.limited.is_some() {
                                    p.bad { "Waiting: plan usage reached the stop level." }
                                } @else {
                                    p.muted { "Waiting for open work." }
                                }
                            }
                        }
                        Activity::Working(job) => {
                            div.panel.card.wcard {
                                div.wtop { h3 { "Worker " (worker.id + 1) } span.timer { (elapsed(job.started)) } }
                                strong { (job.item) }
                                div.chips {
                                    span.chip.acc { (job.provider.label()) }
                                    @if !job.model.is_empty() { span.chip { (job.model) } }
                                }
                                (now_line(job))
                                ul.timeline {
                                    @let skip = job.events.len().saturating_sub(CARD_EVENTS);
                                    @let last = job.events.len().saturating_sub(1);
                                    @for (i, event) in job.events.iter().enumerate().skip(skip) {
                                        li class=[(i == last).then_some("cur")] {
                                            span.ic { (event_icon(event)) }
                                            span { (event_line(event)) }
                                        }
                                    }
                                    @if job.events.is_empty() { li { span.ic { "·" } span.muted { "Starting agent…" } } }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Outside the live sections, which refresh and would drop a typed link.
        div {
            h2 { "Import a pull request" }
            form.panel.bar method="post" action="/import" {
                input type="text" name="url" required style="flex:1; min-width:260px"
                    placeholder="https://github.com/owner/repo/pull/123"
                    aria-label="Pull request link";
                button.primary { "Import" }
            }
            p.muted {
                "Adds an open pull request to the queue, in review, for the project that opens pull requests "
                "against its repository. Its branch must be on that project's push remote. Changes requested on "
                "it and failing checks are then reworked like those on the app's own pull requests."
            }
        }

        div {
            h2 { "Queue" }
            (queue_section(app, &settings, &snapshot, &items))
        }
    };
    layout("Dashboard", "dashboard", true, notice, body)
}

/// A glyph for the kind of step an agent event is.
fn event_icon(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::ToolUse { name, .. } => match name.to_ascii_lowercase().as_str() {
            "read" | "view_file" | "read_file" => "▤",
            "edit" | "write" | "multiedit" | "write_file" | "replace" => "✎",
            "grep" | "glob" | "search" | "grep_search" | "find_by_name" => "⌕",
            "bash" | "shell" | "run_command" | "run_shell_command" => "▸",
            _ => "•",
        },
        AgentEvent::Text(_) => "¶",
        AgentEvent::Finished { ok: true, .. } => "✓",
        AgentEvent::Finished { ok: false, .. } => "✕",
        _ => "·",
    }
}

/// The suggestions page: those awaiting a decision, and the rejected ones
/// still kept.
pub(crate) fn suggestions(app: &AppState, notice: Option<String>) -> Markup {
    let days = app.settings().suggestion_retention_days;
    let projects = app.pool().suggestions().unwrap_or_default();
    let body = html! {
        div #suggestions data-live {
            h2 { "Agent instruction suggestions" }
            (suggestions_panel(&projects, days))
        }
    };
    layout("Suggestions", "suggestions", true, notice, body)
}

fn suggestions_panel(projects: &[ProjectSuggestions], retention_days: u32) -> Markup {
    let none = projects
        .iter()
        .all(|p| p.open.is_empty() && p.rejected.is_empty());
    html! {
        @if none {
            p.muted {
                "None. When a run fails for a reason within the agent's control, an agent "
                "suggests how to improve the project's instructions, and the suggestion "
                "appears here."
            }
        }
        @for project in projects {
            @if !project.open.is_empty() || !project.rejected.is_empty() || project.versions > 0 {
                h3 {
                    (project.project) " "
                    @if project.versions > 0 {
                        a.small href={ "/notes?project=" (url_encode(&project.project)) } {
                            "Notes history (" (project.versions) " versions)"
                        }
                    }
                }
            }
            div.grid {
                @for s in &project.open { (suggestion_card(&project.project, s, false)) }
            }
            @if !project.rejected.is_empty() {
                details style="margin-top:8px" {
                    summary.muted {
                        "Rejected (" (project.rejected.len()) "), kept for " (retention_days) " days"
                    }
                    div.grid style="margin-top:8px" {
                        @for s in &project.rejected { (suggestion_card(&project.project, s, true)) }
                    }
                }
            }
        }
    }
}

fn suggestion_card(project: &str, s: &Suggestion, rejected: bool) -> Markup {
    let hidden = html! {
        input type="hidden" name="project" value=(project);
        input type="hidden" name="id" value=(s.id);
    };
    html! {
        div.panel.card.suggestion {
            h3 {
                span { (s.item) span.tag { (s.failure) } }
                span.muted { "#" (s.id) " · " (utc(s.created_at)) }
            }
            p.text { (s.text) }
            @if !s.error.is_empty() {
                details {
                    summary.muted.small { @if s.failure == "review" { "The reviewer's feedback" } @else { "The failure" } }
                    pre { (s.error) }
                }
            }
            @for remark in &s.remarks {
                div.remark.small {
                    strong { @match remark.by { Author::User => "You", Author::Agent => "Agent" } }
                    ": " (remark.text)
                }
            }
            @if let Some(error) = &s.evaluation_error {
                p.warn.small { "The agent could not evaluate it: " (error) }
            }
            @if s.status == SuggestionStatus::Evaluating {
                p.muted { "The agent is evaluating your comment…" }
            } @else if rejected {
                div.actions {
                    form.inline method="post" action="/suggestions/restore" {
                        (hidden)
                        button { "Restore" }
                    }
                    form.inline method="post" action="/suggestions/accept" {
                        (hidden)
                        button.primary { "Accept" }
                    }
                }
            } @else {
                div.actions {
                    form.inline method="post" action="/suggestions/accept" {
                        (hidden)
                        button.primary title="Add it to the project's notes file now" { "Accept" }
                    }
                    form.inline method="post" action="/suggestions/reject" {
                        (hidden)
                        button title="Turn it down; it is kept for a while in case you change your mind" { "Reject" }
                    }
                }
                details.comment {
                    summary { "Comment" }
                    form method="post" action="/suggestions/comment" {
                        (hidden)
                        textarea name="comment" required placeholder="Your comment" {}
                        div.actions {
                            button name="mode" value="add"
                                title="Add your comment to the suggestion and put it in the notes file" {
                                "Add to suggestion and accept"
                            }
                            button name="mode" value="evaluate"
                                title="Send the suggestion and your comment to the agent to reconsider" {
                                "Ask the agent to evaluate"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A project's notes file over time: each version, newest first, with a
/// button to put the file back as it was then.
pub(crate) fn notes(project: &str, versions: &[NotesVersion], notice: Option<String>) -> Markup {
    let body = html! {
        h2 { (project) " notes history" }
        @if versions.is_empty() {
            p.muted { "No suggestion has been accepted yet, so the notes file has no history." }
        }
        div.stack {
            @for (i, v) in versions.iter().enumerate() {
                div.panel {
                    div.bar {
                        strong { "Version " (v.id) }
                        span.muted { (utc(v.at)) " UTC · " (v.note) }
                        @if i == 0 {
                            span.ok { "current" }
                        } @else {
                            form.inline method="post" action="/notes/revert" {
                                input type="hidden" name="project" value=(project);
                                input type="hidden" name="version" value=(v.id);
                                button title="Put the notes file back as this version had it. This becomes a new version, so nothing is lost." {
                                    "Revert to this version"
                                }
                            }
                        }
                    }
                    details {
                        summary.muted.small { "Show the file" }
                        pre { @if v.content.is_empty() { "(empty)" } @else { (v.content) } }
                    }
                }
            }
        }
        p { a href="/suggestions" { "← Suggestions" } }
    };
    layout("Notes history", "suggestions", false, notice, body)
}

/// `YYYY-MM-DD HH:MM`, in UTC, for Unix seconds.
fn utc(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    // Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        secs / 3_600 % 24,
        secs / 60 % 60
    )
}

pub(crate) fn settings(
    app: &AppState,
    settings: &Settings,
    status: impl Fn(Cli) -> AuthStatus,
    notice: Option<String>,
) -> Markup {
    let body = html! {
        h2 { "Workers" }
        form.panel.bar method="post" action="/workers" {
            input type="hidden" name="back" value="settings";
            label for="count" { "Number of workers" }
            input #count type="number" name="count" min="0" max=(MAX_WORKERS) value=(settings.workers) style="width:6em";
            button { "Save" }
        }
        form.panel.bar method="post" action="/settings/max-turns" style="margin-top:12px" {
            label for="max_turns" { "Turns per item before hold" }
            input #max_turns type="number" name="max_turns" min="1" max=(MAX_TURNS) value=(settings.max_turns) style="width:6em";
            button { "Save" }
            span.muted {
                "Every agent run on an item is a turn, failed ones included; failures retry until "
                "the limit, then the item goes on hold for you to review and return."
            }
        }
        form.panel.bar method="post" action="/settings/suggestion-retention" style="margin-top:12px" {
            label for="days" { "Days to keep rejected suggestions" }
            input #days type="number" name="days" min="1" max=(MAX_RETENTION_DAYS) value=(settings.suggestion_retention_days) style="width:6em";
            button { "Save" }
            span.muted {
                "A rejected suggestion for the agent instructions can be restored from the "
                "dashboard until this many days have passed."
            }
        }
        form.panel.bar method="post" action="/settings/review-comments" style="margin-top:12px" {
            label.check {
                input type="checkbox" name="follow_all_comments" checked[settings.follow_all_comments];
                "Follow review comments even if changes are not requested"
            }
            button { "Save" }
            span.muted {
                "Only a maintainer can request changes. With this on, inline comments on a pull "
                "request's latest commit from anyone queue a rework too."
            }
        }
        form.panel.bar method="post" action="/settings/ci-logs" style="margin-top:12px" {
            label.check {
                input type="checkbox" name="follow_ci_logs" checked[settings.follow_ci_logs];
                "Give the agent the logs of failing CI runs"
            }
            button { "Save" }
            span.muted {
                "A failing check already queues a rework. With this on, the tail of each failed "
                "run's log goes with it, so the agent can fix what CI actually reported."
            }
        }
        form.panel.bar method="post" action="/settings/claim-order" style="margin-top:12px" {
            label for="order" { "Take new items" }
            select #order name="order" style="width:auto" {
                @for order in ClaimOrder::ALL {
                    option value=(claim_order_value(order)) selected[order == settings.claim_order] { (order.label()) }
                }
            }
            button { "Save" }
            span.muted {
                "Alphanumeric goes by project, then file name. The others go by when the item "
                "file was last changed. Reworks of items in review always come first."
            }
        }

        h2 { "AI system" }
        form.panel.bar method="post" action="/settings/active" {
            label for="active" { "Workers use" }
            select #active name="provider" style="width:auto" {
                @for p in Provider::ALL.into_iter().filter(|p| settings.is_enabled(*p)) {
                    option value=(provider_value(p)) selected[p == settings.active] { (p.label()) }
                }
            }
            button { "Use" }
        }

        div.grid style="margin-top:12px" {
            @for p in &settings.providers {
                div.panel.stack {
                    h3 style="margin:0" {
                        (p.provider.label())
                        @if p.provider == settings.active { " " span.ok { "· active" } }
                        @if !p.enabled { " " span.muted { "· disabled" } }
                    }
                    form.bar method="post" action="/settings/provider/enabled" {
                        input type="hidden" name="provider" value=(provider_value(p.provider));
                        input type="hidden" name="enabled" value=(!p.enabled);
                        button { @if p.enabled { "Disable" } @else { "Enable" } }
                    }
                    @if p.provider.supports_plan() {
                        (login_block(app, p.provider, status(p.provider.cli())))
                    } @else {
                        p.muted { "API key only; this provider has no plan login." }
                    }
                    form method="post" action="/settings/provider" {
                        input type="hidden" name="provider" value=(provider_value(p.provider));
                        @if p.provider.supports_plan() {
                            label.check {
                                input type="checkbox" name="use_api_key" checked[p.use_api_key];
                                "Use an API key instead of the plan login"
                            }
                        }
                        div.fields {
                            div {
                                label { "API key" }
                                input type="password" name="api_key" autocomplete="off"
                                    placeholder=(if p.api_key.is_empty() { "not set" } else { "saved — leave blank to keep" });
                            }
                            // `vibe` takes its model from its own config; Freebuff picks its own.
                            @if !matches!(p.provider.cli(), Cli::Vibe | Cli::Freebuff) {
                                div {
                                    label { "Model" }
                                    input type="text" name="model" value=(p.model) placeholder="CLI default";
                                }
                            }
                            @let levels = p.provider.cli().effort_levels();
                            @if !levels.is_empty() {
                                div {
                                    label { "Effort" }
                                    select name="effort" {
                                        option value="" selected[p.effort.is_empty()] { "CLI default" }
                                        @for level in levels {
                                            option value=(level) selected[p.effort == *level] { (level) }
                                        }
                                    }
                                }
                            }
                            @if !matches!(p.provider.cli(), Cli::Vibe | Cli::Freebuff) {
                                div style="grid-column:1/-1" {
                                    label { "Other models" }
                                    textarea name="alternatives" rows="3"
                                        placeholder="model | effort | when to use it" {
                                        (crate::settings::alternatives_text(&p.alternatives))
                                    }
                                    p.muted style="margin:4px 0 0" {
                                        "One per line; effort and description are optional. With any listed, an agent reads each item (and each rework) and picks from these and the model above."
                                    }
                                }
                            }
                            @if matches!(p.provider, Provider::Deepseek | Provider::OpenRouter | Provider::OpenAiCompatible | Provider::Cloudflare) {
                                div {
                                    label { "Base URL" }
                                    input type="text" name="base_url" value=(p.base_url)
                                        placeholder=(if p.provider == Provider::Deepseek { "https://api.deepseek.com/anthropic" } else if p.provider == Provider::OpenRouter { "https://openrouter.ai/api" } else if p.provider == Provider::Cloudflare { "https://api.cloudflare.com/client/v4/accounts/<account id>/ai/v1" } else { "http://localhost:8000/v1 (Responses API)" });
                                }
                            }
                            @if p.provider == Provider::Cloudflare {
                                div {
                                    label { "Analytics token" }
                                    input type="password" name="analytics_token" autocomplete="off"
                                        placeholder=(if p.analytics_token.is_empty() { "not set — usage is not shown" } else { "saved — leave blank to keep" });
                                    p.muted style="margin:4px 0 0" {
                                        "An API token with Account Analytics: Read. It lets the dashboard show today's Workers AI neurons against the 10,000 free per day."
                                    }
                                }
                            }
                        }
                        p.bar {
                            button { "Save" }
                            @if !p.api_key.is_empty() {
                                label.check { input type="checkbox" name="clear_key"; "Remove saved key" }
                            }
                            @if !p.analytics_token.is_empty() {
                                label.check { input type="checkbox" name="clear_analytics_token"; "Remove analytics token" }
                            }
                        }
                    }
                }
            }
        }

        h2 { "Plan usage limits" }
        form.panel method="post" action="/settings/limits" {
            div.fields {
                div {
                    label for="warn" { "Current window: warn at (%, comma-separated)" }
                    input #warn type="text" name="warn" value=(settings.limits.warn.iter().map(u8::to_string).collect::<Vec<_>>().join(", "));
                }
                div {
                    label for="stop" { "Current window: stop at (%)" }
                    input #stop type="number" name="stop" min="1" max="100" value=(settings.limits.stop);
                }
                div {
                    label for="week_warn" { "Week: warn at (%, comma-separated)" }
                    input #week_warn type="text" name="week_warn" value=(settings.limits.week_warn.iter().map(u8::to_string).collect::<Vec<_>>().join(", "));
                }
                div {
                    label for="week_stop" { "Week: stop at (%)" }
                    input #week_stop type="number" name="week_stop" min="1" max="100" value=(settings.limits.week_stop);
                }
            }
            label.check {
                input type="checkbox" name="failover" checked[settings.limits.failover];
                "At the stop level, fail over to the next provider below (priority failover; the other balancing schemes always share work across all of them)"
            }
            h3 style="margin-bottom:6px" { "Fallback providers, in order" }
            ul #fallbacks {
                @for chosen in settings.fallbacks.iter().copied() {
                    (fallback_row(Some(chosen)))
                }
            }
            template #fallback-template { (fallback_row(None)) }
            p { button type="button" #add-fallback { "Add provider" } }
            p.bar {
                button { "Save" }
                span.muted {
                    "Levels apply to each window a provider reports, the current one and the week. "
                    "At the stop level no new run starts on that provider; running agents finish their item. "
                    "Each fallback uses the model and effort on its card above, and is skipped unless it is set up. Drag a row by its handle to reorder; nothing changes until you save. "
                    "Plan logins report their usage; a provider on an API key doesn't (except OpenRouter on a free model, which reports its daily free-request allowance, and Cloudflare with an analytics token, which reports its daily free neurons), so it is never stopped."
                }
            }
            script src="/static/fallbacks.js" {}
        }

        h2 { "Per-provider usage limits" }
        form.panel method="post" action="/settings/provider-limits" {
            p.muted style="margin-top:0" {
                "Overrides the plan usage limits above for one provider, whether or not it is active. Leave a field blank to follow the global level."
            }
            table {
                tr { th { "Provider" } th { "Window: warn at" } th { "Window: stop at" } th { "Week: warn at" } th { "Week: stop at" } }
                @for p in Provider::ALL {
                    @let own = settings.limits.providers.get(&p).cloned().unwrap_or_default();
                    @let levels = |l: &Option<Vec<u8>>| l.iter().flatten().map(u8::to_string).collect::<Vec<_>>().join(", ");
                    @let name = |level: &str| format!("{}_{level}", provider_value(p));
                    tr {
                        td { (p.label()) }
                        td { input type="text" name=(name("warn")) value=(levels(&own.warn)) placeholder=(settings.limits.warn.iter().map(u8::to_string).collect::<Vec<_>>().join(", ")); }
                        td { input type="number" name=(name("stop")) min="1" max="100" value=[own.stop] placeholder=(settings.limits.stop); }
                        td { input type="text" name=(name("week_warn")) value=(levels(&own.week_warn)) placeholder=(settings.limits.week_warn.iter().map(u8::to_string).collect::<Vec<_>>().join(", ")); }
                        td { input type="number" name=(name("week_stop")) min="1" max="100" value=[own.week_stop] placeholder=(settings.limits.week_stop); }
                    }
                }
            }
            p.bar { button { "Save" } }
        }

        h2 { "Load balancing" }
        form.panel method="post" action="/settings/balancing" {
            p.muted style="margin-top:0" {
                "Spreads items across " (settings.active.label()) " and the fallbacks above, among those under their stop level."
            }
            div.fields {
                div {
                    label for="scheme" { "Scheme" }
                    select #scheme name="scheme" {
                        @for scheme in Scheme::ALL {
                            option value=(scheme_value(scheme)) selected[scheme == settings.balancing.scheme] { (scheme.label()) }
                        }
                    }
                }
                @for p in std::iter::once(settings.active).chain(settings.fallbacks.iter().copied().filter(|p| *p != settings.active && settings.is_enabled(*p))) {
                    div {
                        @let name = format!("weight_{}", provider_value(p));
                        label for=(name) { (p.label()) " weight" }
                        input id=(name) type="number" name=(name) min="1" max=(MAX_WEIGHT) value=(settings.balancing.weight(p));
                    }
                }
            }
            label.check {
                input type="checkbox" name="rework_any_provider" checked[settings.balancing.rework_any_provider];
                "Reworks may run on any provider (off: a rework waits for the provider that opened its pull request)"
            }
            p.bar {
                button { "Save" }
            }
            ul.muted {
                li { strong { "Priority failover" } ": the active provider, then each fallback in order once the ones before it reach the stop level." }
                li { strong { "Round robin" } ": each provider in turn." }
                li { strong { "Weighted" } ": in proportion to the weights, spread evenly; weights only matter here." }
                li { strong { "Pace-based" } ": the provider furthest below its pace, i.e. least used for how much of its plan window has passed. A provider that reports no usage, such as one on an API key, counts as exactly on pace." }
            }
        }

        h2 { "Projects" }
        div.panel.stack {
            p.muted {
                "Work items for a project go in " code { "work/open/<name>/" } ". "
                "Relative paths are relative to " code { (app.root().display()) } "."
            }
            @if !settings.projects.is_empty() {
                table {
                    tr { th { "Name" } th { "Spec directory" } th { "Repository" } th { "Branch from" } th { "Worktree setup" } th { "Build jobs" } th { "Pull requests" } th { "Issues" } th {} }
                    @for project in &settings.projects {
                        tr {
                            td { (project.name) }
                            td { code { (project.spec_dir.display()) } }
                            td { code { (project.repo.display()) } }
                            td { code { (project.base_ref) } }
                            td {
                                @if let Some(setup) = &project.setup { code { (setup) } }
                                @else { span.muted { "none" } }
                            }
                            td { (project.build_jobs) }
                            td {
                                @if let Some(pr) = &project.pr {
                                    "push to " code { (pr.push_remote) } ", open against "
                                    code { (pr.repo) " " (pr.base) }
                                    @if pr.draft { " (draft)" }
                                } @else { span.muted { "off" } }
                            }
                            td {
                                @if let Some(issues) = &project.issues {
                                    code { (issues.url) } ", " (issues.limit) " at a time, "
                                    (issues.order.label())
                                } @else { span.muted { "off" } }
                            }
                            td {
                                form.inline method="post" action="/settings/projects/delete" {
                                    input type="hidden" name="name" value=(project.name);
                                    button { "Remove" }
                                }
                            }
                        }
                    }
                }
            }
            form method="post" action="/settings/projects" {
                div.fields {
                    div { label { "Name" } input type="text" name="name" placeholder="phase" required; }
                    div { label { "Spec directory" } input type="text" name="spec_dir" placeholder="spec/phase" required; }
                    div { label { "Repository" } input type="text" name="repo" placeholder="/home/me/dev/phase or ../phase" required; }
                    div { label { "Branch from" } input type="text" name="base_ref" placeholder="main"; }
                    div { label { "Build jobs" } input type="number" name="build_jobs" min="1" max=(MAX_BUILD_JOBS) placeholder=(DEFAULT_BUILD_JOBS); }
                }
                div.fields {
                    div { label { "Push to remote (optional)" } input type="text" name="pr_remote" placeholder="fork"; }
                    div { label { "Open pull request against" } input type="text" name="pr_repo" placeholder="phase-rs/phase"; }
                    div { label { "Pull request base branch" } input type="text" name="pr_base" placeholder="main"; }
                    div { label.check style="margin-top:26px" { input type="checkbox" name="pr_draft"; "Open as draft" } }
                }
                div.fields {
                    div { label { "Issues link (optional)" } input type="text" name="issues_url" placeholder="https://github.com/phase-rs/phase/issues"; }
                    div { label { "Issues at a time" } input type="number" name="issues_limit" min="1" max=(MAX_ISSUE_BATCH) placeholder="5"; }
                    div {
                        label { "Take issues" }
                        select name="issues_order" {
                            @for order in IssueOrder::ALL {
                                option value=(order.value()) { (order.label()) }
                            }
                        }
                    }
                    div { label { "Access token (optional)" } input type="password" name="issues_token" autocomplete="off"; }
                }
                p.muted {
                    "Open issues of a GitHub, GitLab, Bitbucket or Gitea/Forgejo repository become work items "
                    code { "issue-<number>.txt" } ", a few at a time: that many are kept waiting in "
                    code { "open" } ", and more follow as workers take them. The token is only needed for private repositories."
                }
                label { "Worktree setup command (optional)" }
                input type="text" name="setup" placeholder="e.g. cp \"$AIWC_REPO\"/data/*.json data/";
                label { "Ignored comment markers (optional, one per line)" }
                textarea name="ignore_comments" rows="2" placeholder="pr-review-deferred" {}
                p.muted {
                    "A pull request comment or review containing any of these is not feedback, so it queues no rework. "
                    "Use it for a bot's notice that asks for nothing."
                }
                p.muted {
                    "Each item gets a fresh git worktree on its own " code { "ai/<item>" } " branch, beside the repo in "
                    code { "<repo>-worktrees/" } ". This command runs there with " code { "sh -c" } " before the agent "
                    "starts, to supply gitignored or generated files the build needs; "
                    code { "$AIWC_REPO" } " is your checkout and " code { "$AIWC_WORKTREE" } " the new worktree. "
                    "Each worker keeps its own " code { "CARGO_TARGET_DIR" } " build cache there too, and builds with "
                    code { "CARGO_BUILD_JOBS" } " set to the build jobs. "
                    "With a push remote set, finished work is pushed there and a pull request opened with "
                    code { "gh" } " (your GitHub login), titled and described by the agent following the repo's PR template. "
                    "To change a project, add it again under the same name."
                }
                p { button { "Add project" } }
            }
        }
    };
    layout("Settings", "settings", false, notice, body)
}

fn login_block(app: &AppState, provider: Provider, status: AuthStatus) -> Markup {
    let cli = provider.cli();
    let session = app.logins().session(cli);
    html! {
        div {
            "Plan login (" code { (cli.program()) } "): "
            @match status {
                AuthStatus::LoggedIn => span.ok { "signed in" },
                AuthStatus::LoggedOut => span.bad { "not signed in" },
                AuthStatus::Unknown => span.muted { "unknown" },
            }
            " "
            @if agent_backend::login_invocation(cli).is_some() {
                form.inline method="post" action="/login" {
                    input type="hidden" name="provider" value=(provider_value(provider));
                    button { "Log in" }
                }
            } @else {
                div.muted { "Sign in by running " code { (cli.program()) } " once in a terminal." }
            }
        }
        @if let Some(session) = session {
            pre { (session.output) }
            @if session.running {
                form.bar method="post" action="/login/input" {
                    input type="hidden" name="provider" value=(provider_value(provider));
                    input type="text" name="line" placeholder="Paste a code if asked" style="width:auto;flex:1";
                    button { "Send" }
                    a href="/settings" { "Refresh" }
                }
            }
        }
    }
}

pub(crate) fn result(item: &WorkItem, body: &str) -> Markup {
    let body = html! {
        h2 { (item) }
        pre { (body) }
        p { a href="/" { "← Dashboard" } }
    };
    layout("Result", "dashboard", false, None, body)
}

fn event_line(event: &AgentEvent) -> Markup {
    html! {
        @match event {
            AgentEvent::Text(text) => (clip(text)),
            AgentEvent::ToolUse { name, detail } => { code { (name) } " " (clip(detail)) },
            AgentEvent::Notice(text) => span.muted { (clip(text)) },
            AgentEvent::Usage(_) => span.muted { "Plan usage updated" },
            AgentEvent::Model(model) => span.muted { "Model: " (model) },
            AgentEvent::Metrics(_) => span.muted { "Token usage reported" },
            // Claude ends a turn while its background commands run, then
            // carries on, so this is not necessarily the end of the run.
            AgentEvent::Finished { ok: true, summary } => {
                span.ok { "Turn ended" } @if let Some(s) = summary { ": " (clip(s)) }
            },
            AgentEvent::Status(text) => span.muted { (clip(text)) },
            AgentEvent::Background(tasks) => span.muted { "Background: " (tasks.join("; ")) },
            AgentEvent::Finished { ok: false, summary } => {
                span.bad { "Agent failed" @if let Some(s) = summary { ": " (clip(s)) } }
            },
        }
    }
}

fn clip(text: &str) -> String {
    clip_to(text, EVENT_CHARS)
}

/// `text`, trimmed and cut to `chars` characters.
fn clip_to(text: &str, chars: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// A provider's logo on its brand color, or its initial when it has none.
fn provider_mark(provider: Provider) -> Markup {
    let (color, logo) = match provider {
        Provider::Anthropic => ("#d97757", Some("claude.svg")),
        Provider::OpenAi => ("#0d0d0d", Some("openai.svg")),
        Provider::Gemini => ("#3b6df0", Some("googlegemini.svg")),
        Provider::Cloudflare => ("#f38020", Some("cloudflare.svg")),
        Provider::OpenRouter => ("#000000", Some("openrouter.png")),
        Provider::Freebuff => ("#000000", Some("freebuff.png")),
        _ => ("#5d6580", None),
    };
    let initial = provider.label().chars().next().unwrap_or('?');
    html! {
        span.pmark style={ "--pbg: " (color) } aria-hidden="true" {
            @match logo {
                Some(file) if file.ends_with(".png") => img src={ "/static/" (file) } alt="";
                Some(file) => i style={ "--logo: url(/static/" (file) ")" } {}
                None => (initial)
            }
        }
    }
}

fn usage_panel(snapshot: &PoolSnapshot, limits: &UsageLimits) -> Markup {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    html! {
        @if let Some(limited) = &snapshot.limited {
            p.bad { "Workers are waiting: " (limited) ". Override a provider below to let runs start on it again." }
        }
        @for (provider, error) in &snapshot.suspended {
            p.warn {
                (provider.label()) " is set aside as a fallback until the active provider can run again. "
                "Its last run failed: " (clip(error))
            }
        }
        @if snapshot.usage.is_empty() {
            p.muted { "No usage reported yet. Claude reports its plan usage during each run, and Antigravity, Codex, Freebuff (its daily free hours), an OpenRouter key on a free model and Cloudflare with an analytics token are asked for theirs every five minutes (OpenRouter's is its daily free-request allowance, Cloudflare's its daily free neurons)." }
        } @else {
            div.usage {
                @for u in &snapshot.usage {
                    @let limits = &limits.for_provider(u.provider);
                    @let hit = limits.stop_hit(&u.usage, now);
                    @let overridden = snapshot.overridden.contains(&u.provider);
                    @let warning = limits.plan_warning(&u.usage, now);
                    div.ucard class=[(hit.is_some() && !overridden).then_some("limited")] {
                        div.uhead {
                            (provider_mark(u.provider))
                            h3 { (u.provider.label()) }
                            @if hit.is_some() && !overridden {
                                span.pill.bad { "Stopped" }
                            } @else if hit.is_some() || warning.is_some() {
                                span.pill.warn { "Watch" }
                            } @else {
                                span.pill.ok { "Healthy" }
                            }
                        }
                        (meter(u.usage.session, Span::Window, now, limits))
                        (meter(u.usage.weekly, Span::Week, now, limits))
                        @if let Some(hit) = hit {
                            div.ustate {
                                @if overridden {
                                    span.warn { "Past the " (hit.level) "% " (hit.span.label()) " stop level; overridden until the window resets" }
                                } @else {
                                    span.bad { "Stopped at the " (hit.level) "% " (hit.span.label()) " level " }
                                    form.inline method="post" action="/usage/override" {
                                        input type="hidden" name="provider" value=(provider_value(u.provider));
                                        button { "Override" }
                                    }
                                }
                            }
                        } @else if let Some(level) = warning {
                            div.ustate { span.warn { "Past the " (level) "% warning" } }
                        }
                    }
                }
            }
        }
    }
}

/// A bar for one usage window, with a tick at its stop level.
fn meter(window: Option<UsageWindow>, span: Span, now: u64, limits: &UsageLimits) -> Markup {
    let name = match span {
        Span::Window => "Session",
        Span::Week => "Week",
    };
    let Some(w) = window else {
        return html! { div.meter { div.mtop { span.mlab { (name) } span.muted { "not reported" } } } };
    };
    let percent = w.percent_at(now);
    let class = if limits.stopped(span, percent) {
        Some("bad")
    } else {
        limits.warning(span, percent).map(|_| "warn")
    };
    let stop = limits.stop_at(span).min(100);
    html! {
        div.meter {
            div.mtop {
                span.mlab {
                    (name)
                    span.muted {
                        @if let Some(reset) = w.resets_at.filter(|r| *r > now) {
                            " · resets in " (duration(reset - now))
                        } @else if w.resets_at.is_some() {
                            " · reset since last report"
                        }
                    }
                }
                strong class=[class] { (percent) "%" }
            }
            div.mbar class=[class] {
                b style={ "width: " (percent.min(100)) "%" } {}
                i style={ "left: " (stop) "%" } title={ "Runs stop at " (stop) "%" } {}
            }
        }
    }
}

/// A rough length of time: `3d 4h`, `2h 14m` or `9m`.
fn duration(secs: u64) -> String {
    let (days, hours, mins) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

/// What the agent is doing right now, and for how long: waiting on
/// background commands, running a step, or thinking; then its own latest
/// account of where it stands.
fn now_line(job: &ActiveJob) -> Markup {
    html! {
        div.now {
            @if !job.background.is_empty() {
                span.warn { "Waiting on " (job.background.join("; ")) }
                " " span.timer { (elapsed(job.background_since.unwrap_or(job.last_event))) }
            } @else {
                @match job.events.back() {
                    Some(AgentEvent::ToolUse { name, detail }) => {
                        strong { (activity(name, detail)) }
                        " " span.timer { (elapsed(job.last_event)) }
                        @if !detail.is_empty() { div { code { (clip_to(detail, 120)) } } }
                    }
                    Some(AgentEvent::Finished { .. }) => {
                        strong { "Wrapping up" } " " span.timer { (elapsed(job.last_event)) }
                    }
                    None => { strong { "Starting" } " " span.timer { (elapsed(job.started)) } }
                    Some(_) => { strong { "Thinking" } " " span.timer { (elapsed(job.last_event)) } }
                }
            }
            @if let Some(status) = &job.status {
                div.muted { "Agent's status: " (status) }
            }
        }
    }
}

/// A plain description of a tool call: for shell commands, what the
/// command is for when it is a common one.
fn activity(tool: &str, detail: &str) -> String {
    let lower = tool.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "bash" | "shell" | "run_command" | "run_shell_command"
    ) {
        let command = detail.trim_start();
        let label = if command.contains("clippy") {
            "Linting (clippy)"
        } else if command.contains("nextest") || command.contains("cargo test") {
            "Running tests"
        } else if command.contains("cargo build") || command.contains("cargo check") {
            "Building"
        } else if command.contains("cargo fmt") || command.contains("rustfmt") {
            "Formatting"
        } else if command.contains("check-parser-combinators") {
            "Running Gate A"
        } else if command.starts_with("git ") {
            "Running git"
        } else if command.starts_with("gh ") {
            "Querying GitHub"
        } else {
            "Running a command"
        };
        return label.to_string();
    }
    let label = match lower.as_str() {
        "read" | "view_file" | "read_file" => "Reading",
        "edit" | "write" | "multiedit" | "write_file" | "replace" => "Editing",
        "grep" | "glob" | "search" | "grep_search" | "find_by_name" => "Searching",
        "webfetch" | "websearch" | "web_search" | "read_url_content" => "Looking it up",
        "todowrite" => "Planning",
        "task" | "agent" => "Delegating to a subagent",
        _ => return format!("Using {tool}"),
    };
    label.to_string()
}

fn elapsed(started: SystemTime) -> String {
    clock(started.elapsed().map(|d| d.as_secs()).unwrap_or(0))
}

/// `m:ss`, or `h:mm:ss` from an hour on.
fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3_600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The form value of a scheme, matching its serde name.
fn claim_order_value(order: ClaimOrder) -> &'static str {
    match order {
        ClaimOrder::Alphanumeric => "alphanumeric",
        ClaimOrder::Fifo => "fifo",
        ClaimOrder::Lifo => "lifo",
    }
}

fn scheme_value(scheme: Scheme) -> &'static str {
    match scheme {
        Scheme::Priority => "priority",
        Scheme::RoundRobin => "round_robin",
        Scheme::Weighted => "weighted",
        Scheme::Pace => "pace",
    }
}

/// The form value of a provider, matching its serde name.
pub(crate) fn provider_value(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "anthropic",
        Provider::Gemini => "gemini",
        Provider::OpenAi => "open_ai",
        Provider::Deepseek => "deepseek",
        Provider::OpenRouter => "open_router",
        Provider::OpenAiCompatible => "open_ai_compatible",
        Provider::Mistral => "mistral",
        Provider::Cloudflare => "cloudflare",
        Provider::Freebuff => "freebuff",
    }
}

pub(crate) fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn clock_shows_hours_once_past_one() {
        assert_eq!(clock(5), "0:05");
        assert_eq!(clock(59 * 60 + 9), "59:09");
        assert_eq!(clock(3_600 + 62), "1:01:02");
    }

    #[test]
    fn utc_formats_unix_seconds() {
        assert_eq!(utc(0), "1970-01-01 00:00");
        assert_eq!(utc(951_782_400 + 3_600 * 13 + 60 * 5), "2000-02-29 13:05");
        assert_eq!(utc(1_700_000_000), "2023-11-14 22:13");
    }

    #[test]
    fn common_commands_get_plain_names() {
        assert_eq!(
            activity(
                "Bash",
                "cargo clippy --workspace --all-targets -- -D warnings"
            ),
            "Linting (clippy)"
        );
        assert_eq!(
            activity("Bash", "cargo nextest run -p engine"),
            "Running tests"
        );
        assert_eq!(activity("shell", "git status"), "Running git");
        assert_eq!(activity("Bash", "ls -la"), "Running a command");
        assert_eq!(activity("view_file", "/a.rs"), "Reading");
        assert_eq!(activity("Monitor", ""), "Using Monitor");
    }

    #[test]
    fn clip_cuts_long_text_on_a_char_boundary() {
        let long = "é".repeat(EVENT_CHARS + 5);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), EVENT_CHARS + 1);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip(" short "), "short");
    }

    #[test]
    fn provider_values_match_serde_names() {
        for p in Provider::ALL {
            let json = format!("\"{}\"", provider_value(p));
            assert_eq!(serde_json_like(&json), p);
        }
    }

    fn serde_json_like(json: &str) -> Provider {
        #[derive(serde::Deserialize)]
        struct W {
            p: Provider,
        }
        let w: W = toml::from_str(&format!("p = {json}")).unwrap();
        w.p
    }

    #[test]
    fn slug_makes_ids_from_project_names() {
        assert_eq!(slug("Docs Site_2"), "docs-site-2");
        assert_eq!(slug("phase"), "phase");
    }

    #[test]
    fn project_hue_is_stable_and_in_range() {
        assert_eq!(project_hue("phase"), project_hue("phase"));
        assert!(project_hue("anything at all") < 360);
    }

    #[test]
    fn url_encode_escapes_separators() {
        assert_eq!(url_encode("phase/a b.txt"), "phase%2Fa%20b.txt");
    }
}
