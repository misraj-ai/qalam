// Tests for pr-policy.js. No dependencies: `node .github/scripts/pr-policy.test.js`.
"use strict";

const assert = require("node:assert/strict");
const { parseBranch, findLinks, checkLinks, run } = require("./pr-policy.js");

const OWNER = "misraj-ai";
const REPO = "qalam";
let failures = 0;
const tests = [];
const test = (name, fn) => tests.push({ name, fn });

// ---- branch names ----------------------------------------------------------

test("a conventional branch name parses", () => {
  assert.deepEqual(parseBranch("fix/42-cmap-short-bfrange-array"), { type: "fix", number: 42 });
  assert.deepEqual(parseBranch("feat/7-tagged-tables"), { type: "feat", number: 7 });
  for (const type of ["refactor", "perf", "docs", "test", "ci", "chore"]) {
    assert.ok(parseBranch(`${type}/1-x`), type);
  }
});

test("names off the convention are rejected", () => {
  for (const bad of [
    "main",
    "fix-42-thing",          // wrong separator
    "fix/thing",             // no number
    "fix/42",                // no description
    "fix/42-",               // empty description
    "fix/42-Thing",          // uppercase
    "fix/42-two--hyphens",   // empty word
    "feature/42-thing",      // not a listed type
    "bugfix/42-thing",
    "fix/42-thing/extra",
  ]) {
    assert.equal(parseBranch(bad), null, bad);
  }
});

// ---- links -----------------------------------------------------------------

test("closing keywords link an issue", () => {
  for (const body of ["Closes #42", "fixes #42", "Resolved: #42", "This PR\n\nFixes #42."]) {
    assert.deepEqual(findLinks(body, OWNER, REPO), [{ kind: "issue", number: 42 }], body);
  }
});

test("full URLs link issues and discussions", () => {
  assert.deepEqual(
    findLinks("See https://github.com/misraj-ai/qalam/discussions/57", OWNER, REPO),
    [{ kind: "discussion", number: 57 }]
  );
  assert.deepEqual(
    findLinks("https://github.com/misraj-ai/qalam/issues/42", OWNER, REPO),
    [{ kind: "issue", number: 42 }]
  );
});

test("a bare #N in prose is not a link", () => {
  assert.deepEqual(findLinks("Unlike #12, this keeps the order.", OWNER, REPO), []);
});

test("links into other repositories do not count", () => {
  assert.deepEqual(findLinks("https://github.com/someone/else/issues/42", OWNER, REPO), []);
});

test("the template's commented-out example never counts", () => {
  const template = [
    "Closes #",
    "<!-- or, for a discussion:",
    "Discussion: https://github.com/misraj-ai/qalam/discussions/57",
    "-->",
  ].join("\n");
  assert.deepEqual(findLinks(template, OWNER, REPO), []);
});

test("an empty description has no links", () => {
  assert.deepEqual(findLinks(null, OWNER, REPO), []);
});

// ---- combined --------------------------------------------------------------

test("the branch number must match a link", () => {
  const branch = parseBranch("fix/42-thing");
  assert.equal(checkLinks(branch, [{ kind: "issue", number: 42 }]).length, 0);
  assert.equal(checkLinks(branch, [{ kind: "issue", number: 43 }]).length, 1);
  assert.equal(checkLinks(branch, []).length, 1);
  assert.equal(checkLinks(null, []).length, 2);
});

// ---- run(), against a fake GitHub ------------------------------------------

function fakeGithub({ issues = {}, discussions = {} }) {
  return {
    rest: {
      issues: {
        get: async ({ issue_number }) => {
          if (!(issue_number in issues)) {
            const error = new Error("Not Found");
            error.status = 404;
            throw error;
          }
          return { data: issues[issue_number] };
        },
      },
    },
    graphql: async (_query, { number }) => ({
      repository: {
        discussion: number in discussions
          ? { labels: { nodes: discussions[number].map((name) => ({ name })) } }
          : null,
      },
    }),
  };
}

async function outcome({ ref, body, user = { type: "User", login: "someone" }, github }) {
  let failed = null;
  const core = { setFailed: (m) => (failed = m), info: () => {} };
  const context = { repo: { owner: OWNER, repo: REPO }, payload: { pull_request: { head: { ref }, body, user } } };
  await run({ github, context, core });
  return failed;
}

test("an approved issue passes", async () => {
  const github = fakeGithub({ issues: { 42: { labels: [{ name: "bug" }, { name: "approved" }] } } });
  assert.equal(await outcome({ ref: "fix/42-thing", body: "Closes #42", github }), null);
});

test("an unapproved issue fails and says why", async () => {
  const github = fakeGithub({ issues: { 42: { labels: [{ name: "needs-triage" }] } } });
  const failed = await outcome({ ref: "fix/42-thing", body: "Closes #42", github });
  assert.match(failed, /does not have the `approved` label/);
});

test("an approved discussion passes, by URL or by keyword", async () => {
  const github = fakeGithub({ discussions: { 57: ["approved"] } });
  const url = "Discussion: https://github.com/misraj-ai/qalam/discussions/57";
  assert.equal(await outcome({ ref: "feat/57-idea", body: url, github }), null);
  // `Closes #57` where #57 is a discussion: the issue lookup 404s, then the
  // discussion is found.
  assert.equal(await outcome({ ref: "feat/57-idea", body: "Closes #57", github }), null);
});

test("linking a pull request is not an approval", async () => {
  const github = fakeGithub({ issues: { 9: { pull_request: {}, labels: [{ name: "approved" }] } } });
  const failed = await outcome({ ref: "fix/9-thing", body: "Closes #9", github });
  assert.match(failed, /is a pull request/);
});

test("a number that exists nowhere fails", async () => {
  const failed = await outcome({ ref: "fix/404-thing", body: "Closes #404", github: fakeGithub({}) });
  assert.match(failed, /is not an issue or discussion/);
});

test("a bad branch name fails without calling GitHub", async () => {
  const github = { rest: { issues: { get: () => assert.fail("fetched") } }, graphql: () => assert.fail("fetched") };
  const failed = await outcome({ ref: "my-branch", body: "Closes #42", github });
  assert.match(failed, /Branch name must be/);
});

test("bots are exempt", async () => {
  const failed = await outcome({
    ref: "dependabot/cargo/lopdf-0.45",
    body: "",
    user: { type: "Bot", login: "dependabot[bot]" },
    github: fakeGithub({}),
  });
  assert.equal(failed, null);
});

(async () => {
  for (const { name, fn } of tests) {
    try {
      await fn();
      console.log(`ok    ${name}`);
    } catch (error) {
      failures += 1;
      console.log(`FAIL  ${name}\n      ${error.message.split("\n").join("\n      ")}`);
    }
  }
  console.log(`\n${tests.length - failures} passed, ${failures} failed`);
  process.exit(failures ? 1 : 0);
})();
