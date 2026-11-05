// Pure helpers, no herdr imports, so tests/ can run them under node.
/** "owner/name" from a PR URL, or "". */
export function repoOf(url) {
    return url.match(/github\.com\/([^/]+\/[^/]+)\/pull\//)?.[1] ?? "";
}
/** The state a row shows: a draft is its own, as on github.com. */
export function stateOf(pr) {
    return pr.draft && pr.state === "OPEN" ? "DRAFT" : pr.state;
}
/** A change's "+12 -3" as its added and removed parts, either possibly "". */
export function splitDiff(diff) {
    const parts = diff.split(" ");
    return [parts.find((p) => p.startsWith("+")) ?? "", parts.find((p) => p.startsWith("-")) ?? ""];
}
