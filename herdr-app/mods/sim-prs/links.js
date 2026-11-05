// Pure helpers, no herdr imports, so tests/ can run them under node.

// The PR URLs in a Claude Code transcript's pr-link records (as `grep` prints
// them), newest first, each once.
export function prUrls(text) {
  const urls = [];
  for (const line of text.split("\n").reverse()) {
    try {
      const url = JSON.parse(line).prUrl;
      if (url && !urls.includes(url)) urls.push(url);
    } catch { /* a blank or partly written line */ }
  }
  return urls;
}

// "owner/name" from a PR URL, or "".
export function repoOf(url) {
  return url.match(/github\.com\/([^/]+\/[^/]+)\/pull\//)?.[1] ?? "";
}

// The state a row shows: a draft is its own, as on github.com.
export function stateOf(pr) {
  return pr.isDraft && pr.state === "OPEN" ? "DRAFT" : pr.state;
}
