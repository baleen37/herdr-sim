import { View, div, svg } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { agents, panes, focused, exec, run, layout, theme, props, shared, setShared, window } from "herdr";
import { claudeSession, sessionPrs } from "./session.js";
import { repoOf, stateOf } from "./links.js";

// This folder's name: the layout's block id, the stack child and the tab bar's remembered panel.
const MOD = "sim-prs";

const FIELDS = "number,title,state,url,isDraft,additions,deletions,reviewDecision";
const POLL = 10000;   // how often the shown panel looks at the focused agent again
const FRESH = 60000;  // an open PR (or a branch) is re-checked at most this often; merged and closed never
// GitHub's own state colours, so they read the same in every theme.
const STATES = {
  OPEN: { label: "Open", color: "#238636" },
  DRAFT: { label: "Draft", color: "#6e7681" },
  MERGED: { label: "Merged", color: "#8957e5" },
  CLOSED: { label: "Closed", color: "#da3633" },
};

function find(node, id) {
  if (node?.id === id) return node;
  for (const child of node?.children ?? []) {
    const found = find(child, id);
    if (found) return found;
  }
  return null;
}

// As one of the side panels beside Files and Changes, only the one shown works.
function hidden() {
  const root = layout(), body = find(root, "panel-body");
  if (!body?.children?.some((c) => c.content?.mod === MOD)) return false;
  return find(root, "panel")?.hidden || find(body, body.active)?.content?.mod !== MOD;
}

function openUrl(url) {
  if (window()?.platform === "windows") { exec("explorer", [url]).catch(() => null); return; }
  exec("sh", ["-c", 'xdg-open "$1" >/dev/null 2>&1 || open "$1"', "sh", url]).catch(() => null);
}

// The panel (follows the focused terminal), or its PRs button beside the tab bar (props { button: true }).
export default class Prs extends View {
  init(_props, cx) {
    // init's first argument isn't the block's props: props() is.
    if (props()?.button) {
      // The tab bar restores each space's panel only if it's one of these.
      const panels = shared("workspace.panels") ?? ["files", "changes"];
      if (!panels.includes(MOD)) setShared("workspace.panels", [...panels, MOD]);
      this.button = true;
      return;
    }
    // url (or folder@branch) → { data, at }; kept so a reopened panel draws at once.
    try { this.cache = JSON.parse(localStorage.getItem("cache") ?? "{}"); } catch { this.cache = {}; }
    this.view = null;
    this.refresh(cx);
    cx.timer.every(POLL, (cx) => { if (!hidden()) this.refresh(cx); });
  }

  // The focused terminal and its folder. A focused mod pane (this one, as a split) keeps the last.
  target() {
    const f = focused(), pane = f?.pane ? panes().find((p) => p.pane === f.pane) : null;
    if (f?.terminal) this.last = { terminal: f.terminal, cwd: pane?.cwd ?? f.cwd, branch: pane?.branch ?? null };
    return this.last ?? {};
  }

  context() { return JSON.stringify(this.target()); }

  async pr(url) {
    const hit = this.cache[url];
    if (hit && (hit.data.state !== "OPEN" || Date.now() - hit.at < FRESH)) return hit.data;
    const r = await exec("gh", ["pr", "view", url, "--json", FIELDS]);
    if (r.code !== 0) return hit?.data ?? null;
    this.cache[url] = { data: JSON.parse(r.stdout), at: Date.now() };
    return this.cache[url].data;
  }

  // The PR of the branch checked out where the terminal is, as `gh pr view` finds it.
  async branchPr(cwd, branch) {
    const id = `${cwd}@${branch}`, hit = this.cache[id];
    if (hit && Date.now() - hit.at < FRESH) return hit.data;
    const r = await exec("gh", ["pr", "view", "--json", FIELDS], { cwd });
    this.cache[id] = { data: r.code === 0 ? JSON.parse(r.stdout) : null, at: Date.now() };
    return this.cache[id].data;
  }

  refresh(cx) {
    if (this.busy) { this.again = true; return; }
    this.busy = true;
    const { terminal, cwd, branch } = this.target(), context = this.context();
    cx.spawn(async (cx) => {
      try {
        // A remote terminal (machine/t1) has its transcripts and checkout on that machine: not read.
        const local = Boolean(terminal) && !terminal.includes("/");
        const claude = local && agents().find((a) => a.address === terminal)?.app === "claude-code";
        const session = claude ? await claudeSession(terminal) : null;
        const urls = session ? await sessionPrs(session) : [];
        const mine = local && cwd && branch ? await this.branchPr(cwd, branch) : null;
        const prs = [];
        for (const url of urls) if (url !== mine?.url) prs.push(await this.pr(url));
        if (this.context() !== context) return;
        this.view = { claude, session, branch, mine, prs: prs.filter(Boolean) };
        this.error = null;
        localStorage.setItem("cache", JSON.stringify(this.cache));
      } catch (e) { this.error = String(e?.message ?? e); }
      finally {
        this.busy = false; cx.notify();
        if (this.again) { this.again = false; this.refresh(cx); }
      }
    });
  }

  toolbar(t) {
    const root = layout(), panel = find(root, "panel"), on = panel && !panel.hidden && find(root, "panel-body")?.active === MOD;
    return h_flex().size_full().pr(10).items_center().justify_end().bg(t.chrome).border_b(1).border_color(t.border)
      .child(h_flex().id(`panel-${MOD}`).h(26).px(10).flex_none().items_center().rounded(6)
        .text_size(12).bg(on ? t.selected : t.chrome).text_color(on ? t.foreground : t.muted)
        .hover((s) => s.bg(t.hover)).on_mouse_down("left", (_e, cx) => cx.stop_propagation())
        .on_click(() => {
          if (on) run("layout.hide", { id: "panel" });
          else { run("layout.select", { stack: "panel-body", child: MOD }); run("layout.show", { id: "panel" }); }
        })
        .child("PRs"));
  }

  heading(label, t) {
    return div().flex_none().px(10).pt(10).pb(4).text_size(11).font_semibold().text_color(t.dim).child(label);
  }

  row(p, t) {
    const state = STATES[stateOf(p)] ?? { label: p.state, color: t.dim }, repo = repoOf(p.url);
    return v_flex().id(`pr-${p.url}`).flex_none().px(10).py(7).gap(4).rounded(7).cursor_pointer()
      .hover((s) => s.bg(t.hover)).on_click(() => openUrl(p.url))
      .child(h_flex().gap(8).items_start()
        .child(div().flex_none().h(18).px(7).flex().items_center().rounded(9).bg(state.color)
          .text_size(10).font_semibold().text_color("#ffffff").child(state.label))
        .child(div().flex_1().min_w(0).text_color(t.foreground).child(p.title)))
      .child(h_flex().gap(8).text_size(11).text_color(t.dim)
        .child(div().min_w(0).truncate().child(`${repo}#${p.number}`))
        .child(div().flex_none().text_color(t.ansi2).child(`+${p.additions}`))
        .child(div().flex_none().text_color(t.ansi1).child(`−${p.deletions}`))
        .when(p.reviewDecision === "APPROVED", (e) => e.child(div().flex_none().text_color(t.ansi2).child("✓ Approved"))));
  }

  render(cx) {
    const t = theme();
    if (this.button) return this.toolbar(t);
    const away = hidden();
    if ((this.away && !away) || (!away && this.context() !== this.shownContext)) { this.shownContext = this.context(); this.refresh(cx); }
    this.away = away;

    const v = this.view, empty = (text) => div().px(16).py(20).text_size(12).text_center().text_color(t.dim).child(text);
    return v_flex().id(MOD).size_full().min_h(0).rounded(10).overflow_hidden().bg(t.background).border_1().border_color(t.border).text_color(t.foreground).text_size(13)
      .child(h_flex().h(42).flex_none().px(14).gap(8).items_center().border_b(1).border_color(t.border)
        .child(div().font_semibold().child("Pull requests"))
        .child(div().flex_1().min_w(0).text_size(11).text_color(t.dim).text_right().truncate().child(v?.branch ?? ""))
        .child(div().size(12).flex_none().when(this.busy, (e) => e.child(svg("mods/workspace-chrome/spinner.svg").size(12).text_color(t.working).spin(900)))))
      .child(v_flex().id("prs-list").flex_1().min_h(0).overflow_y_scrollbar().p(6).gap(1)
        .when(Boolean(this.error), (e) => e.child(div().px(10).py(4).text_size(11).text_color(t.ansi1).child(this.error)))
        .when(!v, (e) => e.child(empty("Looking at the focused terminal…")))
        .when(Boolean(v), (e) => e
          .child(this.heading("This branch", t))
          .child(v.mine ? this.row(v.mine, t) : empty(v.branch ? "No PR for this branch" : "Not on a git branch"))
          .child(this.heading("This session", t))
          .when(v.prs.length > 0, (e) => e.children(v.prs.map((p) => this.row(p, t))))
          .when(!v.prs.length, (e) => e.child(empty(
            !v.claude ? "Focus a Claude Code terminal to see the PRs its session touched"
              : !v.session ? "No Claude Code session found for this terminal"
              : v.mine ? "No other PRs in this session" : "This session hasn't touched a PR yet")))));
  }
}
