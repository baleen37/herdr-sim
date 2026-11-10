// A browser's reopen-closed-tab for Herdr tabs. `sim-tabs.close` (cmd-w) parks the
// focused tab's terminals instead of ending them, so the tab goes but its programs
// keep running; `sim-tabs.reopen` (cmd-shift-w) gives the last closed tab's terminals
// panes again, in its space, under its name, at its position. Only the last LIMIT tabs
// are kept: an older one's terminals end, as tab.close would have ended them.
//
// It draws nothing: a 0px block in the herdr-sim layout, always shown, so the keys
// reach it (a <mod>.<command> only reaches shown copies).

import { View, div } from "gpui-kit";
import { panes, focused, spaces, terminals, run, shared, setShared, nextCommand } from "herdr";

const CLOSED = "sim-tabs.closed";
const LIMIT = 10;
// terminal.keep finishes in the background: how long to wait for its pane.
const WAIT = 3000;

const closed = () => shared(CLOSED) ?? [];
const live = (addr) => terminals().some((t) => t.address === addr && t.state === "live");
const paneOf = (addr) => panes().find((p) => p.terminal === addr);

// Polls every 50 ms until `done()` holds, or WAIT ms; resolves with whether it held.
function until(cx, done) {
  return new Promise((resolve) => {
    const start = Date.now();
    const task = cx.timer.every(50, () => {
      const ok = done();
      if (ok || Date.now() - start > WAIT) { task.cancel(); resolve(Boolean(ok)); }
    });
  });
}

// Commands are handled one at a time, so a second cmd-w waits for the first tab to go
// (terminal.park finishes in the background) and then closes the next tab, as a browser does.
async function close(cx) {
  const at = focused();
  if (!at) return;
  const tab = spaces().flatMap((s) => s.tabs.map((t) => ({ ...t, space: s.id }))).find((t) => t.id === at.tab);
  const addrs = panes().filter((p) => p.tab === at.tab && p.terminal).map((p) => p.terminal);
  // A tab of mod panes only has nothing to keep running.
  if (!tab || addrs.length === 0) { run("tab.close", { id: at.tab }); return; }
  const list = [...closed(), { space: tab.space, name: tab.named ? tab.name : null, index: tab.index, terminals: addrs }];
  for (const old of list.splice(0, Math.max(0, list.length - LIMIT))) {
    for (const t of old.terminals) if (live(t)) run("terminal.kill", { terminal: t });
  }
  setShared(CLOSED, list);
  for (const t of addrs) run("terminal.park", { terminal: t });
  // Mod panes stay behind a parked terminal: once the terminals are out, close what's left.
  // Never before, or tab.close would end them.
  if (!(await until(cx, () => !addrs.some(paneOf)))) return;
  if (panes().some((p) => p.tab === at.tab)) run("tab.close", { id: at.tab });
}

async function reopen(cx) {
  const list = closed();
  let entry, addrs = [];
  // Tabs whose programs all ended since are gone for good.
  while (list.length && addrs.length === 0) {
    entry = list.pop();
    addrs = entry.terminals.filter(live);
  }
  setShared(CLOSED, list);
  if (addrs.length === 0) return;
  if (spaces().some((s) => s.id === entry.space)) run("space.select", { id: entry.space });
  run("terminal.keep", { terminal: addrs[0], place: "tab" });
  // Its pane never showed: keep it on the list so the next cmd-shift-w tries again.
  if (!(await until(cx, () => paneOf(addrs[0])))) { setShared(CLOSED, [...closed(), entry]); return; }
  const first = paneOf(addrs[0]);
  let prev = addrs[0];
  for (const t of addrs.slice(1)) {
    run("pane.focus", { terminal: prev });
    run("terminal.keep", { terminal: t, place: "right" });
    if (await until(cx, () => paneOf(t))) prev = t;
  }
  run("pane.focus", { terminal: addrs[0] });
  // terminal.keep names the tab after the terminal; "" gives an unnamed one its number again.
  run("tab.rename", { id: first.tab, name: entry.name ?? "" });
  run("tab.move", { id: first.tab, index: entry.index });
}

export default class SimTabs extends View {
  init(_props, cx) {
    cx.spawn(async (cx) => {
      for (;;) {
        const command = await nextCommand();
        if (command === "close") await close(cx);
        else if (command === "reopen") await reopen(cx);
      }
    });
  }

  render() {
    return div();
  }
}
