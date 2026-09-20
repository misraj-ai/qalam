// The rules the `pr-policy` workflow enforces, kept out of the YAML so they can
// be tested with plain Node: `node .github/scripts/pr-policy.test.js`.
//
// Two kinds of function live here. The pure ones (`parseBranch`, `findLinks`,
// `checkLinks`) take strings and return data. `run` is the only one that talks
// to GitHub, and it does nothing but fetch and report.

"use strict";

/** Branch prefixes, and what each is for. Keep in sync with CONTRIBUTING.md. */
const TYPES = ["feat", "fix", "refactor", "perf", "docs", "test", "ci", "chore"];

/** The label a maintainer applies once a change is agreed. */
const APPROVED_LABEL = "approved";

const BRANCH_PATTERN = new RegExp(
  `^(${TYPES.join("|")})/(\\d+)-[a-z0-9]+(?:-[a-z0-9]+)*$`
);

/**
 * Split a branch name into its type and number.
 * Returns `null` when the name does not follow the convention.
 */
function parseBranch(ref) {
  const match = BRANCH_PATTERN.exec(ref);
  if (!match) return null;
  return { type: match[1], number: Number(match[2]) };
}

/**
 * Find the issue and discussion links in a pull request description.
 *
 * Only *explicit* links count: a closing keyword before `#N`, or a full URL into
 * this repository. A bare `#12` in prose ("unlike #12") is not a link to the
 * work this PR implements. HTML comments are removed first, so the template's
 * own example text never satisfies the check.
 */
function findLinks(body, owner, repo) {
  const text = (body || "").replace(/<!--[\s\S]*?-->/g, "");
  const links = [];
  const seen = new Set();
  const add = (kind, number) => {
    const key = `${kind}:${number}`;
    if (!seen.has(key)) {
      seen.add(key);
      links.push({ kind, number });
    }
  };

  // Closes #12, fixes: #12, Resolved #12 — GitHub's own closing keywords.
  const keyword = /\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b:?\s+#(\d+)\b/gi;
  for (const m of text.matchAll(keyword)) add("issue", Number(m[1]));

  const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const url = new RegExp(
    `https://github\\.com/${escape(owner)}/${escape(repo)}/(issues|discussions)/(\\d+)\\b`,
    "gi"
  );
  for (const m of text.matchAll(url)) {
    add(m[1].toLowerCase() === "issues" ? "issue" : "discussion", Number(m[2]));
  }
  return links;
}

/**
 * Compare the branch against the links, before anything is fetched.
 * Returns a list of problems; empty means these two rules pass.
 */
function checkLinks(branch, links) {
  const problems = [];
  if (!branch) {
    problems.push(
      "Branch name must be `<type>/<number>-<short-description>`, with type one of: " +
        TYPES.map((t) => `\`${t}\``).join(", ") +
        ". Example: `fix/42-cmap-short-bfrange-array`."
    );
  }
  if (links.length === 0) {
    problems.push(
      "The description must link an issue (`Closes #42`) or a discussion " +
        "(its full URL) in this repository."
    );
  }
  if (branch && links.length > 0 && !links.some((l) => l.number === branch.number)) {
    problems.push(
      `The branch names #${branch.number}, but the description links ` +
        links.map((l) => `#${l.number}`).join(", ") +
        ". They must match."
    );
  }
  return problems;
}

/** Labels on an issue or a discussion, or `null` if no such item exists. */
async function fetchLabels(github, owner, repo, link) {
  if (link.kind === "issue") {
    try {
      const { data } = await github.rest.issues.get({ owner, repo, issue_number: link.number });
      // The issues API also returns pull requests; a PR is not an approval.
      if (data.pull_request) return { kind: "pull request", labels: [] };
      return { kind: "issue", labels: data.labels.map((l) => (typeof l === "string" ? l : l.name)) };
    } catch (error) {
      if (error.status !== 404 && error.status !== 410) throw error;
      // Issues and discussions share one numbering; `Closes #N` may name a
      // discussion. Fall through and look for one.
    }
  }

  const result = await github.graphql(
    `query($owner: String!, $repo: String!, $number: Int!) {
       repository(owner: $owner, name: $repo) {
         discussion(number: $number) { labels(first: 50) { nodes { name } } }
       }
     }`,
    { owner, repo, number: link.number }
  );
  const discussion = result.repository && result.repository.discussion;
  if (!discussion) return null;
  return { kind: "discussion", labels: discussion.labels.nodes.map((n) => n.name) };
}

/** Entry point for actions/github-script. */
async function run({ github, context, core }) {
  const pr = context.payload.pull_request;
  const { owner, repo } = context.repo;

  // Dependabot and other bots cannot open an issue first.
  if (pr.user && pr.user.type === "Bot") {
    core.info(`Skipping policy for bot account ${pr.user.login}.`);
    return;
  }

  const branch = parseBranch(pr.head.ref);
  const links = findLinks(pr.body, owner, repo);
  const problems = checkLinks(branch, links);

  if (branch && problems.length === 0) {
    const link = links.find((l) => l.number === branch.number);
    const found = await fetchLabels(github, owner, repo, link);
    if (!found) {
      problems.push(`#${link.number} is not an issue or discussion in ${owner}/${repo}.`);
    } else if (found.kind === "pull request") {
      problems.push(`#${link.number} is a pull request; link the issue or discussion it came from.`);
    } else if (!found.labels.includes(APPROVED_LABEL)) {
      problems.push(
        `The ${found.kind} #${link.number} does not have the \`${APPROVED_LABEL}\` label yet. ` +
          "Wait for a maintainer to approve it, then re-run this check."
      );
    }
  }

  if (problems.length > 0) {
    const guide = `https://github.com/${owner}/${repo}/blob/main/CONTRIBUTING.md#how-a-change-gets-in`;
    core.setFailed(
      ["This pull request does not follow the contribution workflow:", ...problems.map((p) => `- ${p}`), `See ${guide}`].join("\n")
    );
    return;
  }
  core.info(`OK: ${pr.head.ref} implements approved #${branch.number}.`);
}

module.exports = { TYPES, APPROVED_LABEL, parseBranch, findLinks, checkLinks, fetchLabels, run };
