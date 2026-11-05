import { exec, home, readTextFile, terminals } from "herdr";
import { prUrls } from "./links.js";

// readTextFile, not fs: a mod reads files outside its folder only through it.
const readJson = (path) => readTextFile(path).then((text) => (text ? JSON.parse(text) : null)).catch(() => null);

// The session a Claude Code terminal shows, as herdr-sim finds it: the claude
// process's ~/.claude/sessions/<pid>.json, followed to its job when the
// conversation was parked with /background.
// The pid comes from terminals(): `hc get` run from a mod never returns.
export async function claudeSession(terminal) {
  const pid = terminals().find((t) => t.address === terminal)?.foreground?.pid;
  const s = pid ? await readJson(`${home()}/.claude/sessions/${pid}.json`) : null;
  if (!s?.sessionId) return null;
  const job = s.parkedJobId ? await readJson(`${home()}/.claude/jobs/${s.parkedJobId}/state.json`) : null;
  return job?.sessionId ?? s.sessionId;
}

// The PRs a session touched: Claude Code writes a pr-link record to its
// transcript when it creates, pushes to, edits or comments on one.
export async function sessionPrs(session) {
  const r = await exec("grep", ["-rhF", '"type":"pr-link"', "--include", `${session}.jsonl`, `${home()}/.claude/projects`]);
  return prUrls(r.stdout);
}
