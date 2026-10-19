// The history dashboard: fetches /history.json and draws the filters, summary
// figures, charts and the per-item table. Everything that came from an agent
// (item names, errors, models) is put on the page as text, never as markup.
(() => {
  "use strict";

  const PALETTE = ["#4c78a8", "#f58518", "#54a24b", "#e45756", "#72b7b2",
    "#b279a2", "#eeca3b", "#9d755d", "#bab0ac"];
  const PROVIDERS = { anthropic: "Anthropic", gemini: "Gemini", open_ai: "OpenAI",
    deepseek: "Deepseek", open_ai_compatible: "OpenAI-compatible" };
  const STATES = { open: "open", inprogress: "in progress", inreview: "in review",
    closed: "closed", hold: "on hold" };
  const OUTCOMES = ["review", "closed", "hold", "failed"];
  const FILTER_KEYS = ["q", "project", "provider", "model", "effort", "state", "from", "to"];
  const GROUPS = { provider: "Provider", model: "Model", effort: "Effort",
    project: "Project", reason: "Reason for the run" };

  let data = { generated_at: 0, items: [] };
  let lastText = "";
  let filters = Object.fromEntries(FILTER_KEYS.map((k) => [k, ""]));
  let sort = { key: "started_at", desc: true };
  let group = "provider";
  const open = new Set();
  const charts = {};

  const $ = (id) => document.getElementById(id);
  const providerName = (p) => PROVIDERS[p] || p;
  const modelLabel = (t) => t.models.length ? t.models.join(" + ")
    : (t.model_requested || "unreported");
  const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

  function el(tag, attrs, ...children) {
    const node = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === false || v == null) continue;
      if (k === "class") node.className = v;
      else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
      else node.setAttribute(k, v === true ? "" : v);
    }
    for (const c of children.flat()) {
      if (c == null || c === false) continue;
      node.append(c instanceof Node ? c : document.createTextNode(String(c)));
    }
    return node;
  }

  // ---------- formatting ----------
  const nf = new Intl.NumberFormat();
  const fmtNum = (n) => n == null ? "–" : nf.format(Math.round(n));
  const fmtTokens = (n) => n >= 1e6 ? (n / 1e6).toFixed(2) + "M"
    : n >= 1e4 ? (n / 1e3).toFixed(1) + "k" : fmtNum(n);
  const fmtCost = (micro) => "$" + (micro / 1e6).toFixed(micro >= 1e7 ? 0 : 2);
  function fmtDur(secs) {
    if (secs == null) return "–";
    secs = Math.round(secs);
    if (secs < 60) return secs + "s";
    const m = Math.floor(secs / 60);
    if (m < 60) return m + "m " + (secs % 60) + "s";
    const h = Math.floor(m / 60);
    if (h < 48) return h + "h " + (m % 60) + "m";
    return Math.floor(h / 24) + "d " + (h % 24) + "h";
  }
  const fmtTime = (secs) => secs ? new Date(secs * 1000).toLocaleString([], {
    month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "–";
  function day(secs) {
    const d = new Date(secs * 1000);
    const p = (n) => String(n).padStart(2, "0");
    return d.getFullYear() + "-" + p(d.getMonth() + 1) + "-" + p(d.getDate());
  }
  function median(xs) {
    if (!xs.length) return null;
    const s = [...xs].sort((a, b) => a - b);
    const mid = s.length >> 1;
    return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2;
  }
  // A backfilled run's length is unknown (stored as 0), and so are the times of
  // an item with such runs: they are counted from when it was queued.
  const known = (t) => !t.backfilled || t.duration_ms > 0;
  const sum = (xs) => xs.reduce((a, b) => a + b, 0);
  const pct = (a, b) => b ? Math.round(100 * a / b) + "%" : "–";

  // ---------- filtering ----------
  function dateBounds() {
    // Local midnight at the start of "from", end of "to".
    const from = filters.from ? new Date(filters.from + "T00:00:00").getTime() / 1000 : -Infinity;
    const to = filters.to ? new Date(filters.to + "T23:59:59").getTime() / 1000 : Infinity;
    return [from, to];
  }

  function turnMatches(t) {
    const [from, to] = dateBounds();
    return (!filters.provider || t.provider === filters.provider)
      && (!filters.model || modelLabel(t).split(" + ").includes(filters.model)
        || t.model_requested === filters.model)
      && (!filters.effort || (t.effort || "default") === filters.effort)
      && t.started_at >= from && t.started_at <= to;
  }

  function haystack(item) {
    const parts = [item.item, item.project, item.state, item.pr_url, item.finished];
    for (const t of item.records) {
      parts.push(t.provider, providerName(t.provider), t.model_requested, t.effort,
        t.error, t.failure, t.reason, t.outcome, ...t.models, ...t.feedback);
    }
    return parts.filter(Boolean).join("\n").toLowerCase();
  }

  // The items to show, each with only the turns that pass the filters.
  function visible() {
    const q = filters.q.trim().toLowerCase();
    const out = [];
    for (const item of data.items) {
      if (filters.project && item.project !== filters.project) continue;
      if (filters.state && (item.state || "removed") !== filters.state) continue;
      if (q && !haystack(item).includes(q)) continue;
      const turns = item.records.filter(turnMatches);
      if (!turns.length) continue;
      out.push({ item, turns });
    }
    return out;
  }

  // ---------- filter controls ----------
  function fillSelect(id, values, label) {
    const select = $(id);
    const current = filters[id.replace("f-", "")];
    select.replaceChildren(el("option", { value: "" }, label),
      ...values.map(([v, text]) => el("option", { value: v, selected: v === current }, text)));
  }

  function renderFilterOptions() {
    const turns = data.items.flatMap((i) => i.records);
    const uniq = (xs) => [...new Set(xs.filter(Boolean))].sort();
    fillSelect("f-project", uniq(data.items.map((i) => i.project)).map((v) => [v, v]), "All projects");
    fillSelect("f-provider", uniq(turns.map((t) => t.provider)).map((v) => [v, providerName(v)]), "All providers");
    fillSelect("f-model", uniq(turns.flatMap((t) => t.models.length ? t.models : [t.model_requested]))
      .map((v) => [v, v]), "All models");
    fillSelect("f-effort", uniq(turns.map((t) => t.effort || "default")).map((v) => [v, v]), "All efforts");
    fillSelect("f-state", [...Object.entries(STATES), ["removed", "removed from queue"]], "All states");
  }

  function bindFilters() {
    for (const key of FILTER_KEYS) {
      const input = $("f-" + key);
      input.value = filters[key];
      input.addEventListener("input", () => {
        filters[key] = input.value;
        saveHash();
        render();
      });
    }
    $("f-clear").addEventListener("click", () => {
      for (const key of FILTER_KEYS) { filters[key] = ""; $("f-" + key).value = ""; }
      saveHash();
      render();
    });
    $("group-by").addEventListener("input", (e) => { group = e.target.value; render(); });
    matchMedia("(prefers-color-scheme: dark)").addEventListener("change", render);
  }

  function saveHash() {
    const params = new URLSearchParams();
    for (const key of FILTER_KEYS) if (filters[key]) params.set(key, filters[key]);
    history.replaceState(null, "", params.size ? "#" + params : location.pathname);
  }

  function loadHash() {
    const params = new URLSearchParams(location.hash.slice(1));
    for (const key of FILTER_KEYS) filters[key] = params.get(key) || "";
  }

  // ---------- summary figures ----------
  function renderStats(rows) {
    const items = rows.map((r) => r.item);
    const turns = rows.flatMap((r) => r.turns);
    const finished = items.filter((i) => i.finished);
    const measured = finished.filter((i) => !i.backfilled);
    const ttm = items.filter((i) => !i.backfilled).map((i) => i.time_to_merge_s).filter((x) => x != null);
    const backfilled = items.filter((i) => i.backfilled).length;
    const stats = [
      ["Bodies of work", fmtNum(items.length),
        finished.length + " finished, " + (items.length - finished.length) + " in progress or waiting"
          + (backfilled ? "; " + backfilled + " backfilled" : "")],
      ["Runs", fmtNum(turns.length),
        (items.length ? (turns.length / items.length).toFixed(1) : "0") + " per item"],
      ["First-pass rate", pct(finished.filter((i) => i.first_pass).length, finished.length),
        "finished after a single run"],
      ["Run failures", pct(turns.filter((t) => t.outcome === "failed").length, turns.length),
        turns.filter((t) => t.stalled).length + " stalled, "
          + turns.filter((t) => t.fallback).length + " on a fallback"],
      ["Median time to finish", fmtDur(median(measured.map((i) => i.elapsed_s))),
        "first run start to merge or close; backfilled items left out"],
      ["Median time to merge", fmtDur(median(ttm)), "first push for review to merge; backfilled items left out"],
      ["Agent time", fmtDur(sum(turns.map((t) => t.duration_ms)) / 1000),
        "review wait " + fmtDur(sum(items.filter((i) => !i.backfilled).map((i) => i.review_wait_s)))],
      ["Tokens in / out", fmtTokens(sum(turns.map((t) => t.metrics.input_tokens || 0)))
        + " / " + fmtTokens(sum(turns.map((t) => t.metrics.output_tokens || 0))),
        fmtTokens(sum(turns.map((t) => t.metrics.cache_read_tokens || 0))) + " read from cache"],
      ["Cost reported", fmtCost(sum(turns.map((t) => t.metrics.cost_micro_usd || 0))),
        "as the CLIs estimate it"],
      ["Change size", "+" + fmtNum(sum(turns.map((t) => t.lines_added || 0))) + " / −"
        + fmtNum(sum(turns.map((t) => t.lines_removed || 0))),
        fmtNum(sum(turns.map((t) => t.commits))) + " commits"],
    ];
    $("stats").replaceChildren(...stats.map(([label, value, note]) =>
      el("div", { class: "stat" }, el("div", { class: "muted" }, label),
        el("strong", {}, value), el("div", { class: "muted small" }, note))));
  }

  // ---------- charts ----------
  function draw(id, config) {
    const canvas = $(id);
    const empty = !config.data.datasets.some((d) => d.data.some((v) => v));
    canvas.parentElement.classList.toggle("empty", empty);
    if (charts[id]) charts[id].destroy();
    const text = css("--muted");
    const grid = css("--border");
    const base = config.type === "doughnut" ? {} : {
      scales: Object.fromEntries(["x", "y"].map((a) => [a, {
        ticks: { color: text }, grid: { color: grid },
        stacked: config.stacked, beginAtZero: true,
        ...(config.counts && a === (config.indexAxis === "y" ? "x" : "y") ? { ticks: { color: text, precision: 0 } } : {}) }])),
    };
    const indexAxis = config.indexAxis;
    charts[id] = new Chart(canvas, {
      type: config.type,
      data: config.data,
      options: {
        responsive: true, maintainAspectRatio: false, animation: false, indexAxis,
        plugins: { legend: { display: config.legend !== false, labels: { color: text } },
          tooltip: config.tooltip },
        ...base,
      },
    });
  }

  function colorFor(names, name) { return PALETTE[names.indexOf(name) % PALETTE.length]; }

  function dayRange(turns) {
    if (!turns.length) return [];
    const times = turns.map((t) => t.started_at);
    const days = [];
    for (let s = Math.min(...times); day(s) <= day(Math.max(...times)); s += 86400) days.push(day(s));
    if (days[days.length - 1] !== day(Math.max(...times))) days.push(day(Math.max(...times)));
    return days;
  }

  function renderCharts(rows) {
    const turns = rows.flatMap((r) => r.turns);
    const days = dayRange(turns);
    const providers = [...new Set(turns.map((t) => t.provider))].sort();
    const perDay = (pick) => days.map((d) => sum(turns.filter((t) => day(t.started_at) === d).map(pick)));

    draw("c-turns", { counts: true, type: "bar", stacked: true, data: { labels: days,
      datasets: providers.map((p) => ({ label: providerName(p), backgroundColor: colorFor(providers, p),
        data: days.map((d) => turns.filter((t) => t.provider === p && day(t.started_at) === d).length) })) } });

    draw("c-tokens", { type: "bar", stacked: true, data: { labels: days, datasets: [
      { label: "Input", backgroundColor: PALETTE[0], data: perDay((t) => t.metrics.input_tokens || 0) },
      { label: "Output", backgroundColor: PALETTE[1], data: perDay((t) => t.metrics.output_tokens || 0) },
    ] } });

    draw("c-cost", { type: "bar", data: { labels: days, datasets: [
      { label: "Cost (USD)", backgroundColor: PALETTE[2],
        data: perDay((t) => (t.metrics.cost_micro_usd || 0) / 1e6) }] }, legend: false,
      tooltip: { callbacks: { label: (c) => "$" + c.parsed.y.toFixed(2) } } });

    const byModel = new Map();
    for (const t of turns) {
      const k = modelLabel(t) + (t.effort ? " · " + t.effort : "");
      byModel.set(k, (byModel.get(k) || 0) + t.duration_ms / 3.6e6);
    }
    const models = [...byModel.entries()].sort((a, b) => b[1] - a[1]);
    draw("c-models", { type: "bar", indexAxis: "y", legend: false, data: { labels: models.map((m) => m[0]),
      datasets: [{ label: "Agent hours", backgroundColor: PALETTE[3], data: models.map((m) => +m[1].toFixed(2)) }] } });

    const quota = new Map();
    for (const { item, turns: kept } of rows) {
      if (kept.length !== item.records.length) continue; // partial items have no clean total
      for (const q of item.quota) {
        const cur = quota.get(q.provider) || [0, 0];
        quota.set(q.provider, [cur[0] + q.session_points, cur[1] + q.weekly_points]);
      }
    }
    const quotaProviders = [...quota.keys()].sort();
    draw("c-quota", { type: "bar", data: { labels: quotaProviders.map(providerName), datasets: [
      { label: "Session window (points)", backgroundColor: PALETTE[0], data: quotaProviders.map((p) => quota.get(p)[0]) },
      { label: "Weekly window (points)", backgroundColor: PALETTE[1], data: quotaProviders.map((p) => quota.get(p)[1]) },
    ] } });

    draw("c-outcomes", { type: "doughnut", data: { labels: OUTCOMES,
      datasets: [{ backgroundColor: [PALETTE[0], PALETTE[2], PALETTE[1], PALETTE[3]], borderColor: css("--panel"),
        data: OUTCOMES.map((o) => turns.filter((t) => t.outcome === o).length) }] } });

    const failures = new Map();
    for (const t of turns) if (t.failure) failures.set(t.failure, (failures.get(t.failure) || 0) + 1);
    const kinds = [...failures.entries()].sort((a, b) => b[1] - a[1]);
    draw("c-failures", { counts: true, type: "bar", legend: false, data: { labels: kinds.map((k) => k[0]),
      datasets: [{ label: "Failed runs", backgroundColor: PALETTE[3], data: kinds.map((k) => k[1]) }] } });

    const finished = rows.map((r) => r.item).filter((i) => i.finished);
    const bins = [1, 2, 3, 4, 5];
    draw("c-turns-per-item", { counts: true, type: "bar", legend: false, data: {
      labels: bins.map((b) => b === 5 ? "5+" : String(b)),
      datasets: [{ label: "Finished items", backgroundColor: PALETTE[4],
        data: bins.map((b) => finished.filter((i) => b === 5 ? i.turns >= 5 : i.turns === b).length) }] } });
  }

  // ---------- breakdown ----------
  function groupKeys(t) {
    switch (group) {
      case "provider": return [providerName(t.provider) + (t.fallback ? " (fallback)" : "")];
      case "model": return [modelLabel(t)];
      case "effort": return [t.effort || "default"];
      case "reason": return [t.reason + (t.feedback.length ? " (" + t.feedback.join(", ") + ")" : "")];
      default: return [t.project];
    }
  }

  function renderBreakdown(rows) {
    const groups = new Map();
    for (const { item, turns } of rows) {
      for (const t of turns) {
        const record = { ...t, project: item.project };
        for (const k of groupKeys(record)) {
          if (!groups.has(k)) groups.set(k, []);
          groups.get(k).push(t);
        }
      }
    }
    const table = el("table", {},
      el("tr", {}, ["", "Runs", "Failed", "Median run", "Agent time", "Tokens in / out", "Cost", "Lines +/−"]
        .map((h, i) => el("th", { class: i ? "num" : "" }, h))),
      [...groups.entries()].sort((a, b) => b[1].length - a[1].length).map(([k, ts]) => el("tr", {},
        el("td", {}, k),
        el("td", { class: "num" }, ts.length),
        el("td", { class: "num" }, pct(ts.filter((t) => t.outcome === "failed").length, ts.length)),
        el("td", { class: "num" }, fmtDur(median(ts.filter(known).map((t) => t.duration_ms / 1000)))),
        el("td", { class: "num" }, fmtDur(sum(ts.map((t) => t.duration_ms)) / 1000)),
        el("td", { class: "num" }, fmtTokens(sum(ts.map((t) => t.metrics.input_tokens || 0))) + " / "
          + fmtTokens(sum(ts.map((t) => t.metrics.output_tokens || 0)))),
        el("td", { class: "num" }, fmtCost(sum(ts.map((t) => t.metrics.cost_micro_usd || 0)))),
        el("td", { class: "num" }, "+" + fmtNum(sum(ts.map((t) => t.lines_added || 0))) + " / −"
          + fmtNum(sum(ts.map((t) => t.lines_removed || 0)))))));
    $("breakdown").replaceChildren(table);
  }

  // ---------- items table ----------
  const COLUMNS = [
    ["item", "Item", (r) => r.item.project + "/" + r.item.item],
    ["state", "State", (r) => r.item.state || "removed"],
    ["turns", "Runs", (r) => r.turns.length],
    ["providers", "Providers", (r) => [...new Set(r.turns.map((t) => providerName(t.provider)))].join(", ")],
    ["models", "Models", (r) => [...new Set(r.turns.map(modelLabel))].join(", ")],
    ["efforts", "Effort", (r) => [...new Set(r.turns.map((t) => t.effort || "default"))].join(", ")],
    ["started_at", "Started", (r) => r.item.started_at],
    ["elapsed_s", "Elapsed", (r) => r.item.elapsed_s],
    ["active_s", "Agent time", (r) => sum(r.turns.map((t) => t.duration_ms)) / 1000],
    ["tokens", "Tokens in / out", (r) => sum(r.turns.map((t) => t.metrics.output_tokens || 0))],
    ["cost", "Cost", (r) => sum(r.turns.map((t) => t.metrics.cost_micro_usd || 0))],
    ["quota", "Plan used", (r) => sum(r.item.quota.map((q) => q.weekly_points))],
  ];

  function stateCell(item) {
    if (item.finished === "merged") return el("span", { class: "ok" }, "merged");
    if (item.finished === "closed") return el("span", { class: "ok" }, "closed");
    const s = item.state ? STATES[item.state] : "removed";
    return el("span", { class: item.state === "hold" ? "bad" : "warn" }, s);
  }

  function quotaText(item) {
    return item.quota.filter((q) => q.session_points || q.weekly_points)
      .map((q) => providerName(q.provider) + " " + q.session_points + "% session, " + q.weekly_points + "% weekly")
      .join("; ") || "–";
  }

  function pctOf(w) { return w ? w.percent + "%" : "?"; }
  function quotaRange(t) {
    if (!t.quota_before && !t.quota_after) return "–";
    const b = t.quota_before || {}, a = t.quota_after || {};
    return "session " + pctOf(b.session) + "→" + pctOf(a.session)
      + ", weekly " + pctOf(b.weekly) + "→" + pctOf(a.weekly);
  }

  function turnTable(item, turns) {
    const header = ["#", "Why", "Provider", "Model", "Effort", "Started", "Agent / total", "Outcome",
      "Tokens in / out", "Cost", "Commits, lines", "Tools", "Plan usage"];
    return el("table", { class: "turns" }, el("tr", {}, header.map((h) => el("th", {}, h))),
      turns.map((t, i) => el("tr", {},
        el("td", {}, i + 1),
        el("td", {}, t.reason + (t.feedback.length ? " (" + t.feedback.join(", ") + ")" : ""),
          t.backfilled && el("span", { class: "tag" }, "backfilled")),
        el("td", {}, providerName(t.provider), t.fallback && el("span", { class: "tag" }, "fallback")),
        el("td", {}, t.models.join(", ") || "unreported", t.model_requested && el("div", { class: "muted small" }, "asked: " + t.model_requested)),
        el("td", {}, t.effort || "default"),
        el("td", {}, (known(t) ? "" : "~") + fmtTime(t.started_at)),
        el("td", {}, known(t) ? fmtDur(t.agent_ms / 1000) + " / " + fmtDur(t.duration_ms / 1000) : "unknown"),
        el("td", {}, el("span", { class: t.outcome === "failed" ? "bad" : t.outcome === "hold" ? "warn" : "ok" },
          t.outcome), t.stalled && el("span", { class: "tag" }, "stalled"), t.refunded && el("span", { class: "tag" }, "refunded"),
          t.error && el("div", { class: "muted small err" }, (t.failure ? t.failure + ": " : "") + t.error)),
        el("td", { class: "num" }, fmtNum(t.metrics.input_tokens) + " / " + fmtNum(t.metrics.output_tokens)),
        el("td", { class: "num" }, t.metrics.cost_micro_usd != null ? fmtCost(t.metrics.cost_micro_usd) : "–"),
        el("td", {}, (t.backfilled && !t.commits ? "–" : t.commits) + " · " + (t.lines_added == null ? "–" : "+" + t.lines_added + "/−" + t.lines_removed)
          + (t.files_changed != null ? " · " + t.files_changed + " files" : "")),
        el("td", { class: "small" }, Object.entries(t.tool_calls).sort((a, b) => b[1] - a[1])
          .slice(0, 5).map(([k, v]) => k + " " + v).join(", ") || "–"),
        el("td", { class: "small" }, quotaRange(t)))));
  }

  function renderTable(rows) {
    const col = COLUMNS.find((c) => c[0] === sort.key) || COLUMNS[0];
    const sorted = [...rows].sort((a, b) => {
      const x = col[2](a), y = col[2](b);
      const c = typeof x === "number" && typeof y === "number" ? x - y : String(x).localeCompare(String(y));
      return sort.desc ? -c : c;
    });
    const head = el("tr", {}, COLUMNS.map(([key, label]) => el("th", {
      class: "sortable", "aria-sort": sort.key === key ? (sort.desc ? "descending" : "ascending") : "none",
      onclick: () => { sort = { key, desc: sort.key === key ? !sort.desc : false }; renderTable(rows); } },
      label + (sort.key === key ? (sort.desc ? " ▾" : " ▴") : ""))));
    const body = [];
    for (const r of sorted) {
      const { item, turns } = r;
      const id = item.project + "/" + item.item;
      const isOpen = open.has(id);
      const elapsedNote = item.finished ? "" : " so far";
      body.push(el("tr", { class: "row", tabindex: "0", "aria-expanded": String(isOpen),
        onclick: () => { isOpen ? open.delete(id) : open.add(id); renderTable(rows); },
        onkeydown: (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); e.currentTarget.click(); } } },
        el("td", {}, (isOpen ? "▾ " : "▸ "), id, item.backfilled && el("span", { class: "tag",
          title: "Made up afterwards from the item's files; times and sizes are estimates" }, "backfilled"),
          item.pr_url && item.pr_url.startsWith("https://") && el("a", { href: item.pr_url, target: "_blank",
            rel: "noopener", class: "pr", onclick: (e) => e.stopPropagation() }, "PR")),
        el("td", {}, stateCell(item)),
        el("td", { class: "num" }, turns.length + (item.rework_turns ? " (" + item.rework_turns + " rework)" : "")),
        el("td", {}, COLUMNS[3][2](r)),
        el("td", {}, COLUMNS[4][2](r)),
        el("td", {}, COLUMNS[5][2](r)),
        el("td", {}, fmtTime(item.started_at)),
        el("td", { class: "num" }, (item.backfilled ? "~" : "") + fmtDur(item.elapsed_s) + elapsedNote),
        el("td", { class: "num" }, fmtDur(sum(turns.map((t) => t.duration_ms)) / 1000),
          item.review_wait_s ? el("div", { class: "muted small" }, "review " + fmtDur(item.review_wait_s)) : null),
        el("td", { class: "num" }, fmtNum(sum(turns.map((t) => t.metrics.input_tokens || 0))) + " / "
          + fmtNum(sum(turns.map((t) => t.metrics.output_tokens || 0)))),
        el("td", { class: "num" }, fmtCost(sum(turns.map((t) => t.metrics.cost_micro_usd || 0)))),
        el("td", { class: "small" }, quotaText(item))));
      if (isOpen) {
        body.push(el("tr", { class: "detail" }, el("td", { colspan: COLUMNS.length },
          el("div", { class: "muted small" },
            "Queued " + fmtTime(item.queued_at) + " · waited " + fmtDur(item.queue_wait_s) + " to start"
            + (item.finished_at ? " · finished " + fmtTime(item.finished_at) : "")
            + (item.time_to_merge_s != null ? " · merged " + fmtDur(item.time_to_merge_s) + " after the first push" : "")),
          turnTable(item, turns))));
      }
    }
    $("items").replaceChildren(el("table", { class: "items" }, head, body));
    $("count").textContent = rows.length + " of " + data.items.length + " items";
  }

  // ---------- main ----------
  function render() {
    const rows = visible();
    renderStats(rows);
    renderCharts(rows);
    renderBreakdown(rows);
    renderTable(rows);
  }

  async function load() {
    if (document.hidden && lastText) return;
    try {
      const res = await fetch("/history.json", { cache: "no-store" });
      if (!res.ok) throw new Error(res.status + " " + await res.text());
      const text = await res.text();
      $("error").hidden = true;
      if (text === lastText) return;
      lastText = text;
      data = JSON.parse(text);
      // The ledger leaves out empty lists and maps; the page expects them.
      for (const item of data.items) {
        for (const t of item.records) {
          t.models = t.models || [];
          t.feedback = t.feedback || [];
          t.tool_calls = t.tool_calls || {};
          t.metrics = t.metrics || {};
        }
      }
      renderFilterOptions();
      render();
    } catch (e) {
      $("error").hidden = false;
      $("error").textContent = "Could not load the history: " + e.message;
    }
  }

  loadHash();
  $("group-by").replaceChildren(...Object.entries(GROUPS).map(([v, t]) =>
    el("option", { value: v, selected: v === group }, t)));
  bindFilters();
  load();
  setInterval(load, 10000);
  document.addEventListener("visibilitychange", load);
})();
