// Paja Desk's window: reads the queue through the core's `work_list` command and shows it
// grouped by what it needs. It keeps nothing of its own; every refresh reads the log again.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// The groups, in the order they are shown. Only the last few landed items are shown: the
// newest issues, since items carry no landing time.
const LANDED_SHOWN = 5;
const GROUPS = [
  { name: "Waiting on you", className: "waiting", test: (i) => i.status === "escalated" },
  { name: "Active", test: (i) => i.status === "active" },
  { name: "Queued", test: (i) => i.status === "queued" },
  { name: "Landed recently", test: (i) => i.status === "landed", last: LANDED_SHOWN },
];

function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "className") node.className = value;
    else node.setAttribute(key, value);
  }
  for (const child of [].concat(children)) {
    node.append(typeof child === "string" ? document.createTextNode(child) : child);
  }
  return node;
}

function issueLink(item) {
  const link = el("a", { href: item.url }, `#${item.number}`);
  link.addEventListener("click", (event) => {
    event.preventDefault();
    window.__TAURI__.opener.openUrl(item.url).catch(showError);
  });
  return link;
}

function itemRow(item) {
  const row = el("div", { className: "row" }, [
    el("span", { className: "number" }, [issueLink(item)]),
    el("span", { className: "title" }, item.title),
    el("span", { className: "status" }, item.open ? item.status : `${item.status} (issue closed)`),
  ]);
  const children = [row];
  if (item.status === "active") {
    const key = `${item.repo}#${item.number}`;
    const open = following.has(key);
    const toggle = el("button", { type: "button", className: "quiet" }, open ? "Hide run" : "Follow run");
    toggle.addEventListener("click", () => {
      if (open) {
        following.delete(key);
        lastRun.delete(key);
      } else {
        following.add(key);
      }
      refresh().catch(showError);
    });
    row.append(toggle);
    if (open) children.push(runView(item));
  }
  const last = (item.answers || []).at(-1);
  if (last && last.by === "arbiter") {
    children.push(el("div", { className: "note" }, `answered by arbiter: ${last.answer}`));
  }
  if (item.status === "escalated") {
    if (item.note) children.push(el("div", { className: "note" }, item.note));
    children.push(answerForm(item));
  }
  return el("li", {}, children);
}

// The active items whose run view is open, by `repo#number`.
const following = new Set();

function duration(ms) {
  const secs = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  return h ? `${h}h ${m}m` : m ? `${m}m ${s}s` : `${s}s`;
}

function stepName(step) {
  const who = step.reviewer ? `${step.role} (reviewer ${step.reviewer})` : step.role;
  return step.model ? `${who} on ${step.model}` : who;
}

function spent(step) {
  const parts = [];
  if (step.input_tokens != null) {
    const read = (step.input_tokens ?? 0) + (step.cache_read_input_tokens ?? 0) + (step.cache_creation_input_tokens ?? 0);
    parts.push(`${read.toLocaleString()} tokens in, ${(step.output_tokens ?? 0).toLocaleString()} out`);
  }
  if (step.cost_usd != null) parts.push(`$${step.cost_usd.toFixed(2)}`);
  if (step.num_turns != null) parts.push(`${step.num_turns} turns`);
  if (step.duration_ms != null) parts.push(duration(step.duration_ms));
  return parts.length ? parts.join(", ") : "spend unknown";
}

// What each open run view last read, by `repo#number`, so a refresh shows it until the new
// read arrives instead of flashing.
const lastRun = new Map();

// The open run views on the page, by `repo#number`: the box and its item.
const runBoxes = new Map();

// The run view of an active item: filled by `work_run`, read again on every refresh (each
// refresh rebuilds the rows), and its step clock ticks every second.
function runView(item) {
  const key = `${item.repo}#${item.number}`;
  const box = el("div", { className: "run" });
  if (lastRun.has(key)) box.replaceChildren(...runParts(lastRun.get(key)));
  else box.append(el("p", { className: "muted" }, "Reading the run..."));
  runBoxes.set(key, { box, item });
  readRun(key, item, box);
  return box;
}

function readRun(key, item, box) {
  invoke("work_run", { repo: item.repo, number: item.number })
    .then((run) => {
      if (run && run.error) throw new Error(run.error);
      // A reply that comes after "Hide run" is dropped, so a quick re-open doesn't flash it.
      if (!following.has(key)) return;
      lastRun.set(key, run);
      box.replaceChildren(...runParts(run));
    })
    .catch((err) => box.replaceChildren(el("p", { className: "error" }, `error: ${err?.message ?? err}`)));
}

// A run that is killed writes no event, so while a run view is open Desk reads that run again
// every few seconds: its run lock comes free, and the view turns to interrupted. Only the open
// run views are read again, not the whole queue, so an answer being typed keeps its focus.
const FOLLOW_EVERY_MS = 5000;
setInterval(() => {
  for (const [key, { box, item }] of runBoxes) {
    if (!box.isConnected || !following.has(key)) {
      runBoxes.delete(key);
      continue;
    }
    readRun(key, item, box);
  }
}, FOLLOW_EVERY_MS);

function runParts(run) {
  if (!run) return [el("p", { className: "muted" }, "No run has started this item yet.")];
  const parts = [];
  if (run.state === "interrupted") {
    parts.push(el("p", { className: "warning-inline" },
      "Interrupted: no paja run is working on it now. Start paja run in its checkout to go on from here."));
  }
  if (run.state === "claimed") {
    parts.push(el("p", { className: "muted" },
      "Claimed by hand since its last run, so paja run leaves it alone. Set it back to queued for a run to take it."));
    return parts;
  }
  // An interrupted run's step isn't running, so it gets no clock.
  const now = !run.step ? []
    : run.state !== "running" ? [`, stopped in ${stepName(run.step)}`]
    : [`, ${stepName(run.step)} for `,
       el("span", { className: "clock", "data-since": String(run.step.started_ms) }, duration(Date.now() - run.step.started_ms))];
  parts.push(el("p", {}, [el("strong", {}, `Round ${run.round} of ${run.max_rounds}`), ...now]));
  if (run.env) {
    parts.push(el("p", { className: "muted" }, `Environment ${run.env.name}: ${run.env.path} (${run.env.branch})`));
  }
  if (run.pr) {
    const link = el("a", { href: run.pr }, run.pr);
    link.addEventListener("click", (event) => {
      event.preventDefault();
      window.__TAURI__.opener.openUrl(run.pr).catch(showError);
    });
    parts.push(el("p", {}, ["Pull request ", link]));
  }
  for (const round of run.rounds) {
    const steps = round.steps.map((s) => el("li", {}, `${stepName({ ...s, role: s.role ?? s.step })}: ${spent(s)}`));
    parts.push(el("div", { className: "round" }, [
      el("div", {}, [el("strong", {}, `Round ${round.round}: ${round.verdict}`), round.detail ? ` - ${round.detail}` : ""]),
      el("ul", {}, steps),
    ]));
  }
  return parts;
}

setInterval(() => {
  for (const clock of document.querySelectorAll(".clock")) {
    clock.textContent = duration(Date.now() - Number(clock.dataset.since));
  }
}, 1000);

// A message that stays until the person dismisses it, whatever the queue does meanwhile. The
// oldest go once more than NOTICES pile up.
const NOTICES = 20;
function notice(text) {
  const close = el("button", { type: "button", className: "quiet" }, "Dismiss");
  const entry = el("li", {}, [el("span", {}, text), close]);
  close.addEventListener("click", () => entry.remove());
  const list = document.getElementById("notices");
  list.append(entry);
  while (list.children.length > NOTICES) list.firstElementChild.remove();
}

// What the person has typed, by item, so a refresh or an error never loses it.
const drafts = new Map();

function answerForm(item) {
  const key = `${item.repo}#${item.number}`;
  const text = el("textarea", { rows: "3", placeholder: "Your answer" });
  text.value = drafts.get(key) ?? "";
  text.addEventListener("input", () => drafts.set(key, text.value));
  const send = el("button", { type: "button" }, "Send answer");
  send.addEventListener("click", async () => {
    if (!text.value.trim()) {
      notice(`${item.repo}#${item.number}: write an answer first.`);
      return;
    }
    send.disabled = true;
    const result = await invoke("work_answer", {
      repo: item.repo,
      number: item.number,
      text: text.value,
    }).catch((err) => ({ error: String(err) }));
    send.disabled = false;
    if (result && result.error) {
      // A notice outlives the refresh that may rebuild this item (or, when the answer was
      // recorded but posting it failed, drop it from "waiting on you").
      notice(`${item.repo}#${item.number}: error: ${result.error}`);
      return;
    }
    drafts.delete(key);
    refresh().catch(showError);
  });
  return el("div", { className: "answer" }, [
    text,
    el("div", { className: "actions" }, [send]),
    el("div", { className: "muted" }, [
      "Or answer with ",
      el("code", {}, `paja work answer ${item.number} "<your answer>"`),
    ]),
  ]);
}

let knownRepos = [];

function render(items) {
  // A run view stays open only while its item is active: one that landed or was parked is let go.
  const active = new Set(items.filter((i) => i.status === "active").map((i) => `${i.repo}#${i.number}`));
  for (const key of [...following]) {
    if (!active.has(key)) {
      following.delete(key);
      lastRun.delete(key);
    }
  }
  runBoxes.clear();
  const queue = document.getElementById("queue");
  queue.replaceChildren();
  const repos = [...new Set(items.map((i) => i.repo))].sort();
  knownRepos = repos.filter((r) => r);
  if (repos.length === 0) {
    queue.append(el("p", { className: "muted" }, "No work items yet. Run `paja work sync` in a repository's checkout."));
  }
  for (const repo of repos) {
    queue.append(el("h3", {}, repo || "(no repository)"));
    const mine = items.filter((i) => i.repo === repo);
    for (const group of GROUPS) {
      let shown = mine.filter(group.test);
      if (group.last) shown = shown.slice(-group.last).reverse();
      if (shown.length === 0) continue;
      queue.append(el("h2", {}, `${group.name} (${shown.length})`));
      queue.append(el("ul", { className: group.className ?? "" }, shown.map(itemRow)));
    }
  }
}

function showError(err) {
  const box = document.getElementById("error");
  box.textContent = typeof err === "string" ? err : err?.message ?? String(err);
  box.hidden = false;
}

// Each refresh has a number; an older one that finishes late doesn't draw over a newer one.
let latest = 0;

async function refresh() {
  const mine = ++latest;
  const result = await invoke("work_list");
  if (mine !== latest) return;
  const box = document.getElementById("error");
  if (result && !Array.isArray(result) && result.error) {
    showError(`error: ${result.error}`);
    return;
  }
  box.hidden = true;
  render(result);
  document.getElementById("updated").textContent = `updated ${new Date().toLocaleTimeString()}`;
}

// The "New work" form: edit, then review what will be filed, then file it.
function byId(id) {
  return document.getElementById(id);
}

function showFiling(step) {
  byId("file-edit").hidden = step !== "edit";
  byId("file-confirm").hidden = step !== "confirm";
}

// A middle click on a link would make the webview try to open it itself: send it to the
// opener, as a left click does, and let it decide what may open.
document.addEventListener("auxclick", (event) => {
  const link = event.target.closest?.("a[href]");
  if (!link) return;
  event.preventDefault();
  if (event.button === 1) window.__TAURI__.opener.openUrl(link.href).catch(showError);
});

byId("new-work").addEventListener("click", () => {
  const select = byId("file-repo");
  const chosen = select.value;
  select.replaceChildren(...knownRepos.map((r) => el("option", { value: r }, r)));
  if (knownRepos.includes(chosen)) select.value = chosen;
  byId("filing").hidden = false;
  byId("file-result").hidden = true;
  showFiling("edit");
});

byId("file-cancel").addEventListener("click", () => {
  byId("filing").hidden = true;
});

byId("file-review").addEventListener("click", () => {
  const result = byId("file-result");
  if (!byId("file-repo").value || !byId("file-title").value.trim()) {
    result.className = "error";
    result.textContent = "Pick a repository and give the issue a title.";
    result.hidden = false;
    return;
  }
  result.hidden = true;
  byId("confirm-repo").textContent = byId("file-repo").value;
  byId("confirm-title").textContent = byId("file-title").value;
  byId("confirm-body").textContent = byId("file-body").value;
  showFiling("confirm");
});

byId("file-back").addEventListener("click", () => showFiling("edit"));

byId("file-send").addEventListener("click", async () => {
  const send = byId("file-send");
  const result = byId("file-result");
  send.disabled = true;
  const filed = await invoke("work_file", {
    repo: byId("file-repo").value,
    title: byId("file-title").value,
    body: byId("file-body").value,
  }).catch((err) => ({ error: String(err) }));
  send.disabled = false;
  result.hidden = false;
  if (filed && filed.error) {
    // The text stays in the form, to fix and send again.
    result.className = "error";
    result.textContent = `error: ${filed.error}`;
    showFiling("edit");
    return;
  }
  result.className = "";
  result.replaceChildren("Filed ", issueLink({ url: filed.url, number: filed.url.split("/").pop() }));
  byId("file-title").value = "";
  byId("file-body").value = "";
  showFiling("edit");
});

// The About line: Desk's version and the `paja` it found, with a warning when they differ.
async function showAbout() {
  const about = await invoke("about");
  if (about.error) {
    throw new Error(about.error);
  }
  const pajaText = about.paja
    ? `paja ${about.paja} (${about.paja_path})`
    : "no paja found next to Desk or on PATH";
  byId("about").textContent = `Paja Desk ${about.desk} · ${pajaText}`;
  const warning = byId("versions");
  if (!about.paja) {
    warning.textContent = "Desk found no paja program. Install Paja (or Desk's installer, which includes it) so runs and the plugin work.";
    warning.hidden = false;
  } else if (!about.matches) {
    warning.textContent = `Desk is ${about.desk} but paja is ${about.paja}. Install the same release of both.`;
    warning.hidden = false;
  }
}

listen("paja://events-changed", () => refresh().catch(showError));
showAbout().catch(showError);
refresh().catch(showError);

// Repositories and the runs Desk starts in them.
function runLine(repo) {
  const runs = repo.runs || {};
  if (runs.desk) {
    const kind = runs.every ? "keeps running" : "runs one item";
    return `Paja Desk ${kind} here, started ${new Date(runs.started_ms).toLocaleTimeString()}.`;
  }
  if (runs.terminal) return `A paja run started elsewhere is working on #${runs.terminal}.`;
  return "Not running.";
}

const repoNotes = new Map();

function repoRow(repo) {
  const buttons = [];
  const act = (label, call, quiet) => {
    const button = el("button", { type: "button", className: quiet ? "quiet" : "" }, label);
    button.addEventListener("click", async () => {
      button.disabled = true;
      const result = await call().catch((err) => ({ error: String(err) }));
      button.disabled = false;
      if (result && result.error) repoNotes.set(repo.repo, `error: ${result.error}`);
      else if (result && result.checks) repoNotes.set(repo.repo, doctorText(result));
      else repoNotes.delete(repo.repo);
      showRepos().catch(showError);
    });
    buttons.push(button);
  };
  const runs = repo.runs || {};
  if (runs.desk) {
    act("Stop after this step", () => invoke("run_stop", { repo: repo.repo, now: false }));
    act("Stop now", () => invoke("run_stop", { repo: repo.repo, now: true }), true);
  } else if (!runs.terminal && repo.checkout) {
    act("Run one item", () => invoke("run_start", { repo: repo.repo, every: false }));
    act("Keep running", () => invoke("run_start", { repo: repo.repo, every: true }));
  }
  if (repo.checkout) act("Check", () => invoke("repo_doctor", { repo: repo.repo }), true);
  const pool = el("button", { type: "button", className: "quiet" }, "Pool");
  pool.addEventListener("click", () => openPool(repo.repo));
  buttons.push(pool);
  const children = [
    el("div", { className: "row" }, [el("strong", {}, repo.repo), el("span", { className: "muted" }, repo.checkout ?? "checkout unknown: add it below")]),
    el("div", {}, runLine(repo)),
    el("div", { className: "actions" }, buttons),
  ];
  const note = repoNotes.get(repo.repo);
  if (note) children.push(el("pre", { className: note.startsWith("error") ? "error" : "" }, note));
  if (runs.output) children.push(el("pre", { className: "output" }, runs.output));
  return el("li", {}, children);
}

function doctorText(report) {
  const lines = report.checks.map((c) => `${c.status === "ok" ? "ok  " : c.status === "warning" ? "warn" : "FAIL"} ${c.name}: ${c.detail}${c.fix ? ` (${c.fix})` : ""}`);
  lines.unshift(report.ok ? "Ready to run." : "A check failed; runs won't start until it is fixed.");
  return lines.join("\n");
}

let reposShown = false;

async function showRepos() {
  if (!reposShown) return;
  const result = await invoke("repos");
  if (result && result.error) {
    showError(`error: ${result.error}`);
    return;
  }
  byId("repo-list").replaceChildren(...result.map(repoRow));
}

byId("show-repos").addEventListener("click", () => {
  reposShown = !reposShown;
  byId("repos").hidden = !reposShown;
  showRepos().catch(showError);
});

byId("repo-add").addEventListener("click", async () => {
  const path = byId("repo-path").value.trim();
  const out = byId("repo-result");
  if (!path) return;
  const result = await invoke("repo_add", { path }).catch((err) => ({ error: String(err) }));
  out.hidden = false;
  out.className = result && result.error ? "error" : "muted";
  out.textContent = result && result.error ? `error: ${result.error}` : `Added; ${(result.imported || []).length} new items.`;
  if (!result.error) byId("repo-path").value = "";
  showRepos().catch(showError);
  refresh().catch(showError);
});

// While runs are going, their output and state are read again every few seconds.
setInterval(() => {
  if (reposShown) showRepos().catch(showError);
}, 3000);
listen("paja://events-changed", () => showRepos().catch(showError));

// Closing while Desk's runs are going asks what happens to them.
listen("paja://close-asked", (event) => {
  byId("closing-repos").textContent = (event.payload || []).join(", ");
  byId("closing").hidden = false;
});
byId("close-leave").addEventListener("click", () => invoke("close_desk", { stop: false }).catch(showError));
// Desk waits for its runs to end (ending a stuck one after the grace period) before it closes.
byId("close-stop").addEventListener("click", () => {
  for (const id of ["close-leave", "close-stop", "close-cancel"]) byId(id).disabled = true;
  byId("close-stop").textContent = "Stopping the runs...";
  invoke("close_desk", { stop: true }).catch((err) => {
    for (const id of ["close-leave", "close-stop", "close-cancel"]) byId(id).disabled = false;
    byId("close-stop").textContent = "Stop them now and close";
    showError(err);
  });
});
byId("close-cancel").addEventListener("click", () => {
  byId("closing").hidden = true;
});

// The pool view: one repository's worker pool (`paja run --every --workers`), all of it from
// `pool_status` and the event log, with a lane count that a running pool reads on its next tick.
const POOL_EVERY_MS = 2000;
const POOL_STREAM = 30;
let poolRepo = null;
// The lane count the person is typing, kept until they set it or close the view.
let poolDraft = null;
let poolLast = null;

function openPool(repo) {
  poolRepo = repo;
  poolDraft = null;
  poolLast = null;
  byId("pool-repo").textContent = repo;
  byId("pool-result").hidden = true;
  byId("pool").hidden = false;
  showPool().catch(showError);
}

function poolNote(status, lanes) {
  const busy = status.lanes.map((lane) => lane.lane);
  if (lanes == null || Number.isNaN(lanes) || busy.length <= lanes) return "";
  const names = busy.length === 1 ? `Lane ${busy[0]} is` : `Lanes ${busy.slice(0, -1).join(", ")} and ${busy[busy.length - 1]} are`;
  const then = lanes === 0 ? "no new lane starts until the count is raised" : `the pool then keeps ${lanes} ${lanes === 1 ? "lane" : "lanes"} busy`;
  return `${names} busy: each finishes its item, and ${then}.`;
}

function poolSummary(status) {
  const target = status.target == null ? "no lane count set" : `${status.busy} of ${status.target} lanes busy`;
  if (status.state !== "running") return `Stopped; ${target}. ${status.waiting.length} waiting, ${status.held} held for your answer.`;
  const up = duration(Date.now() - status.started_ms);
  let text = `Running for ${up}: ${target}, spent $${status.spent_usd.toFixed(2)}, landed ${status.landed_since_start}, ${status.ready} ready, ${status.held} held for your answer.`;
  if (status.landing != null) text += ` Landing #${status.landing}.`;
  if ((status.merging || []).length) text += ` Waiting to land: ${status.merging.map((n) => `#${n}`).join(", ")}.`;
  return text;
}

// The lane count typed in the box, from `min` to 32, or null when it isn't one.
function typedLanes(min) {
  const value = byId("pool-lanes").value.trim();
  const lanes = Number(value);
  if (value === "" || !Number.isInteger(lanes) || lanes < min || lanes > 32) return null;
  return lanes;
}

function renderPool(status, repo, stream) {
  byId("pool-summary").textContent = poolSummary(status);
  const input = byId("pool-lanes");
  if (poolDraft == null && document.activeElement !== input) input.value = status.target ?? "";
  const runs = (repo && repo.runs) || {};
  const ours = runs.desk && runs.workers != null;
  byId("pool-start").hidden = status.state === "running" || runs.desk || !!runs.terminal || !(repo && repo.checkout);
  byId("pool-stop").hidden = !ours;
  byId("pool-stop-now").hidden = !ours;
  const note = poolNote(status, poolDraft);
  byId("pool-note").textContent = note;
  byId("pool-note").hidden = !note;
  const now = Date.now();
  byId("pool-table").tBodies[0].replaceChildren(
    ...status.lanes.map((lane) =>
      el("tr", {}, [
        el("td", {}, String(lane.lane)),
        el("td", {}, `#${lane.number} ${lane.title}`),
        el("td", {}, lane.stage),
        el("td", {}, lane.step ?? "-"),
        el("td", {}, duration(now - lane.since_ms)),
        el("td", {}, `$${lane.spent_usd.toFixed(2)}`),
        el("td", {}, (lane.paths || []).join(", ") || "-"),
      ]),
    ),
  );
  byId("pool-table").hidden = status.lanes.length === 0;
  byId("pool-waiting").replaceChildren(
    ...(status.waiting.length
      ? status.waiting.map((item) => el("li", {}, `#${item.number} ${item.title}: ${item.why}`))
      : [el("li", { className: "muted" }, "Nothing waiting.")]),
  );
  byId("pool-landed").replaceChildren(
    ...(status.landed.length
      ? status.landed.map((landing) =>
          el("li", {}, `#${landing.number} ${new Date(landing.ts_ms).toLocaleString()}${landing.pr ? ` ${landing.pr}` : ""}`),
        )
      : [el("li", { className: "muted" }, "Nothing landed yet.")]),
  );
  byId("pool-stream").textContent = (stream || [])
    .map((e) => {
      const d = e.data || {};
      const what = [d.lane != null ? `lane ${d.lane}` : "", d.number != null ? `#${d.number}` : "", d.stage || d.role || "", d.outcome || "", d.lanes != null ? `${d.lanes} lanes` : ""]
        .filter(Boolean)
        .join(" ");
      return `${new Date(e.ts_ms).toLocaleTimeString()}  ${e.kind}  ${what}`;
    })
    .join("\n");
}

async function showPool() {
  if (!poolRepo) return;
  const repo = poolRepo;
  const [status, repos, stream] = await Promise.all([
    invoke("pool_status", { repo }),
    invoke("repos"),
    invoke("pool_events", { repo, limit: POOL_STREAM }),
  ]);
  if (repo !== poolRepo) return;
  if (status && status.error) {
    showError(`error: ${status.error}`);
    return;
  }
  poolLast = status;
  const entry = Array.isArray(repos) ? repos.find((r) => r.repo === repo) : null;
  renderPool(status, entry, Array.isArray(stream) ? stream : []);
}

function poolResult(result, ok) {
  const out = byId("pool-result");
  out.hidden = false;
  out.className = result && result.error ? "error" : "muted";
  out.textContent = result && result.error ? `error: ${result.error}` : ok;
}

byId("pool-lanes").addEventListener("input", () => {
  const value = byId("pool-lanes").value;
  poolDraft = value === "" ? null : Number(value);
  if (poolLast) {
    const note = poolNote(poolLast, poolDraft);
    byId("pool-note").textContent = note;
    byId("pool-note").hidden = !note;
  }
});

byId("pool-set").addEventListener("click", async () => {
  const lanes = typedLanes(0);
  if (lanes == null) {
    poolResult({ error: "type a lane count from 0 to 32 first (0 lets the busy lanes finish and starts none)" });
    return;
  }
  const result = await invoke("pool_lanes", { repo: poolRepo, lanes }).catch((err) => ({ error: String(err) }));
  if (!result.error) poolDraft = null;
  poolResult(result, result.running ? `Set to ${lanes}; the pool reads it on its next tick.` : `Set to ${lanes}; the next pool starts with it unless it is given another.`);
  showPool().catch(showError);
});

byId("pool-start").addEventListener("click", async () => {
  const lanes = typedLanes(1);
  if (lanes == null) {
    poolResult({ error: "type how many lanes the pool keeps busy, from 1 to 32, first" });
    return;
  }
  const result = await invoke("run_start", { repo: poolRepo, every: true, workers: lanes }).catch((err) => ({ error: String(err) }));
  poolResult(result, `Started a pool of ${lanes} lanes.`);
  showPool().catch(showError);
  showRepos().catch(showError);
});

for (const [id, now] of [["pool-stop", false], ["pool-stop-now", true]]) {
  byId(id).addEventListener("click", async () => {
    const result = await invoke("run_stop", { repo: poolRepo, now }).catch((err) => ({ error: String(err) }));
    poolResult(result, now ? "Stopping the pool now." : "The pool stops after each lane's step.");
    showPool().catch(showError);
  });
}

byId("pool-close").addEventListener("click", () => {
  poolRepo = null;
  byId("pool").hidden = true;
});

setInterval(() => {
  if (poolRepo) showPool().catch(showError);
}, POOL_EVERY_MS);
listen("paja://events-changed", () => showPool().catch(showError));
