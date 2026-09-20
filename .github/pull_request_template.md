<!--
Every pull request needs an APPROVED issue or discussion, and a branch named
<type>/<number>-<short-description>. The `pr-policy` check enforces both.
See CONTRIBUTING.md#how-a-change-gets-in
-->

## Linked issue or discussion

<!-- Keep ONE of these lines and fill in the number or URL. Text inside these
     comment markers is ignored by the check. -->

Closes #

<!-- or, for a discussion:
Discussion: https://github.com/misraj-ai/qalam/discussions/
-->

## What this changes

<!-- A short summary. Why this approach? -->

## Output changes

<!-- Does this change what any real document produces? If so, show before and after,
     with the fixture and page. Write "None" if output is unchanged. -->

## Checklist

- [ ] The linked issue or discussion has the `approved` label
- [ ] The branch is named `<type>/<number>-<short-description>`
- [ ] `cargo test` passes
- [ ] `cargo clippy --all-targets` and `cargo fmt --check` are clean
- [ ] If output changed, golden files are regenerated and I have read the diff
- [ ] A bug fix comes with a named regression test
