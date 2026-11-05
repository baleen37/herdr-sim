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

/** What `herdr-sim --prs-json` prints. */
export interface Prs {
  session: string | null;
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
