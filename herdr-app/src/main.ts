import { View, div, svg, type AsyncContext, type Color, type Context } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { agents, panes, focused, terminals, exec, run, layout, theme, props, shared, setShared, window } from "herdr";
import { repoOf, stateOf, type Pr, type Prs } from "./links.ts";

// This folder's name: the layout's block id, the stack child and the tab bar's remembered panel.
const MOD = "sim-prs";
// How often the shown panel asks again. herdr-sim keeps PR details between runs
// (open ones 30 s, merged and closed 10 min), so most asks cost no GitHub call.
const POLL = 10000;
// GitHub's own state colours, so they read the same in every theme.
const STATES: Record<string, { label: string; color: Color }> = {
  OPEN: { label: "Open", color: "#238636" },
  DRAFT: { label: "Draft", color: "#6e7681" },
  MERGED: { label: "Merged", color: "#8957e5" },
  CLOSED: { label: "Closed", color: "#da3633" },
};

// theme() is typed as plain strings; its values are #rrggbb colours, as gpui-kit wants.
type Theme = Record<string, Color>;
interface Node { id?: string; hidden?: boolean; active?: string; content?: { mod?: string }; children?: Node[] }
interface Target { terminal?: string; cwd?: string | null; branch?: string | null }
interface Shown { claude: boolean; branch: string | null; data: Prs }

function find(node: Node | null, id: string): Node | null {
  if (node?.id === id) return node;
  for (const child of node?.children ?? []) {
    const found = find(child, id);
    if (found) return found;
  }
  return null;
}

// The side panel is open on this mod (the button's lit state).
function showing(): boolean {
  const root = layout() as Node, panel = find(root, "panel");
  return Boolean(panel) && !panel.hidden && find(root, "panel-body")?.active === MOD;
}

// As one of the side panels beside Files and Changes, only the one shown works.
// Anywhere else (a pane of its own) it always works.
function hidden(): boolean {
  const body = find(layout() as Node, "panel-body");
  return Boolean(body?.children?.some((c) => c.id === MOD)) && !showing();
}

// herdr-sim before --prs-json fails on the flag; one older still prints nothing.
function parse(stdout: string): Prs {
  try { return JSON.parse(stdout); } catch { throw new Error("herdr-sim doesn't know --prs-json: update it"); }
}

function openUrl(url: string): void {
  if (window()?.platform === "windows") { exec("explorer", [url]).catch((): void => undefined); return; }
  exec("sh", ["-c", 'xdg-open "$1" >/dev/null 2>&1 || open "$1"', "sh", url]).catch((): void => undefined);
}

// The panel (follows the focused terminal), or its PRs button beside the tab bar (props { button: true }).
// The work is herdr-sim's (`--prs-json`): this draws what it prints. props { bin } names the
// binary when it isn't `herdr-sim` on PATH.
export default class SimPrs extends View {
  button: boolean;
  last: Target;
  shownContext: string | null;
  view: Shown | null;
  error: string | null;
  busy: boolean;
  again: boolean;

  init(_props: unknown, cx: AsyncContext) {
    // init's first argument isn't the block's props: props() is.
    if (props()?.button) {
      // The tab bar restores each space's panel only if it's one of these.
      const panels = (shared("workspace.panels") as string[] | undefined) ?? ["files", "changes"];
      if (!panels.includes(MOD)) setShared("workspace.panels", [...panels, MOD]);
      this.button = true;
      return;
    }
    this.view = null;
    this.refresh(cx);
    cx.timer.every(POLL, (cx: AsyncContext) => { if (!hidden()) this.refresh(cx); });
  }

  // The focused terminal and its folder. A focused mod pane (this one, as a split) keeps the last.
  target(): Target {
    const f = focused(), pane = f?.pane ? panes().find((p) => p.pane === f.pane) : null;
    if (f?.terminal) this.last = { terminal: f.terminal, cwd: pane?.cwd ?? f.cwd, branch: pane?.branch ?? null };
    return this.last ?? {};
  }

  refresh(cx: Context) {
    if (this.busy) { this.again = true; return; }
    this.busy = true;
    const target = this.target(), { terminal, cwd, branch } = target, context = JSON.stringify(target);
    cx.spawn(async (cx: AsyncContext) => {
      try {
        // A remote terminal (machine/t1) has its transcripts and checkout on that machine: not read.
        const local = Boolean(terminal) && !terminal.includes("/");
        const claude = local && agents().find((a) => a.address === terminal)?.app === "claude-code";
        // The claude process; `hc get` run from a mod never returns, terminals() has it.
        const pid = claude ? terminals().find((t) => t.address === terminal)?.foreground?.pid : null;
        const args = ["--prs-json", ...(pid ? ["--pid", String(pid)] : []), ...(local && cwd && branch ? ["--cwd", cwd] : [])];
        const r = await exec(props()?.bin ?? "herdr-sim", args);
        if (r.code !== 0) throw new Error((r.stderr || r.stdout).trim() || `herdr-sim exited ${r.code}`);
        if (JSON.stringify(this.target()) !== context) return;
        this.view = { claude, branch, data: parse(r.stdout) };
        this.error = null;
      } catch (e) { this.error = e instanceof Error ? e.message : String(e); }
      finally {
        this.busy = false; cx.notify();
        if (this.again) { this.again = false; this.refresh(cx); }
      }
    });
  }

  toolbar(t: Theme) {
    const on = showing();
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

  heading(label: string, t: Theme) {
    return div().flex_none().px(10).pt(10).pb(4).text_size(11).font_semibold().text_color(t.dim).child(label);
  }

  row(p: Pr, t: Theme) {
    const state = STATES[stateOf(p)] ?? { label: p.state, color: t.dim };
    return v_flex().id(`pr-${p.url}`).flex_none().px(10).py(7).gap(4).rounded(7).cursor_pointer()
      .hover((s) => s.bg(t.hover)).on_click(() => openUrl(p.url))
      .child(h_flex().gap(8).items_start()
        .child(div().flex_none().h(18).px(7).flex().items_center().rounded(9).bg(state.color)
          .text_size(10).font_semibold().text_color("#ffffff").child(state.label))
        .child(div().flex_1().min_w(0).text_color(t.foreground).child(p.title)))
      .child(h_flex().gap(8).text_size(11).text_color(t.dim)
        .child(div().min_w(0).truncate().child(`${repoOf(p.url)}#${p.number}`))
        .child(div().flex_none().text_color(t.ansi2).child(`+${p.additions}`))
        .child(div().flex_none().text_color(t.ansi1).child(`−${p.deletions}`))
        .when(p.approved, (e) => e.child(div().flex_none().text_color(t.ansi2).child("✓ Approved"))));
  }

  render(cx: Context) {
    const t = theme() as Theme;
    if (this.button) return this.toolbar(t);
    // Ask again when the panel comes back into view or the focused terminal changes.
    const context = hidden() ? null : JSON.stringify(this.target());
    if (context && context !== this.shownContext) this.refresh(cx);
    this.shownContext = context;

    const v = this.view, empty = (text: string) => div().px(16).py(20).text_size(12).text_center().text_color(t.dim).child(text);
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
          .child(v.data.branch ? this.row(v.data.branch, t) : empty(v.branch ? "No PR for this branch" : "Not on a git branch"))
          .child(this.heading("This session", t))
          .when(v.data.prs.length > 0, (e) => e.children(v.data.prs.map((p) => this.row(p, t))))
          .when(!v.data.prs.length, (e) => e.child(empty(
            !v.claude ? "Focus a Claude Code terminal to see the PRs its session touched"
              : !v.data.session ? "No Claude Code session found for this terminal"
              : v.data.branch ? "No other PRs in this session" : "This session hasn't touched a PR yet")))));
  }
}
