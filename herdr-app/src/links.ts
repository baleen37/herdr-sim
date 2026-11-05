// Pure helpers, no herdr imports, so tests/ can run them under node.

/** A PR as `herdr-sim --prs-json` prints it. */
export interface Pr {
  url: string;
  number: string;
  title: string;
  state: string;
  draft: boolean;
  approved: boolean;
  additions: number;
  deletions: number;
}

/** A checkout with uncommitted changes, as `herdr-sim --prs-json` prints it. */
export interface Change {
  top: string;
  repo: string | null;
  /** Only for a linked worktree. */
  branch: string | null;
  /** "+12 -3", "+4" or "-2". */
  diff: string;
}

/** What `herdr-sim --prs-json` prints. */
export interface Prs {
  session: string | null;
  changes: Change[];
  branch: Pr | null;
  prs: Pr[];
}

/** "owner/name" from a PR URL, or "". */
export function repoOf(url: string): string {
  return url.match(/github\.com\/([^/]+\/[^/]+)\/pull\//)?.[1] ?? "";
}

/** The state a row shows: a draft is its own, as on github.com. */
export function stateOf(pr: Pick<Pr, "state" | "draft">): string {
  return pr.draft && pr.state === "OPEN" ? "DRAFT" : pr.state;
}

/** A change's "+12 -3" as its added and removed parts, either possibly "". */
export function splitDiff(diff: string): [string, string] {
  const parts = diff.split(" ");
  return [parts.find((p) => p.startsWith("+")) ?? "", parts.find((p) => p.startsWith("-")) ?? ""];
}
