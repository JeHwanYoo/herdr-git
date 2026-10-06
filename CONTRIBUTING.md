# Contributing

You can contribute bug reports, fixes, documentation, and feature proposals.

## Before you start

Search existing issues and pull requests for the same problem. For a new feature or a change across several modules, open an issue to discuss the scope before implementing it. Small fixes and documentation edits can go directly to a pull request.

Use the issue templates. For bugs, include steps to reproduce, expected and actual behavior, and your Herdr Git, Herdr, OS, Git, and terminal versions. For feature requests, describe the task you are trying to complete and how the proposed behavior would help.

Remove private repository data and credentials from logs and screenshots.

## Set up development

Install Rust 1.88 or later with rustfmt and Clippy, Git, and [just](https://just.systems/man/en/installation.html). To run the plugin, you also need Herdr 0.9.0 or later on macOS or Linux. The linking script uses a POSIX shell and awk.

1. Open [JeHwanYoo/herdr-git](https://github.com/JeHwanYoo/herdr-git) on GitHub and click **Fork** to create a copy under your account.
2. Clone your fork below. Replace `YOUR_USERNAME` with your GitHub username.
3. Create a branch from `main` and run the checks.

```sh
git clone https://github.com/YOUR_USERNAME/herdr-git.git
cd herdr-git
git switch -c describe-your-change
just check
```

Build and link the plugin into Herdr:

```sh
just link
```

This builds the release binary, links the local checkout, and adds a `prefix+u` binding to your Herdr configuration if it is missing. If that key is assigned to another command, the script stops. Run `just link` again after changing Rust, then close and reopen the Git pane to use the rebuilt binary.

## Make a change

Keep each pull request focused on one problem. Follow [AGENTS.md](AGENTS.md) and the module rules in [src/AGENTS.md](src/AGENTS.md). Write code that explains itself without comments.

For code changes, run `just check` before requesting review. It rejects Rust comments, checks formatting, runs Clippy with warnings treated as errors, and runs the tests. Add regression tests when the risk and impact justify them. For UI changes, test the affected workflow in Herdr and include screenshots when they help show the result. Use a disposable repository when testing Git operations that change files or history.

Run `just check-comments` to check comments alone. This scans tracked Rust files and untracked Rust files that Git does not ignore, including tests and examples. Line, block, and documentation comments are forbidden; comment markers inside literals are allowed. Violations report the file, line, and column. CI runs the same check.

For documentation changes, check links, commands, and formatting. Keep README.md focused on current usage. Describe feature changes and their context in your pull request.

The repository owner writes the final entries in [CHANGELOG.md](CHANGELOG.md) and `changelogs/`, and runs `just bump` for official releases. Leave release notes and the project version unchanged in contribution pull requests.

## Open a pull request

Push your branch to your fork and open a pull request against `main`. Use the [PR template](.github/pull_request_template.md), replace its prompts with your answers, and keep its section headings. Write a title that names the change.

Explain the problem and resulting behavior, link related issues, and report the checks you ran with their results. If you could not run a check, say which one and why. Include steps for testing behavior that automated checks do not cover. Mark unfinished work as a draft and respond to review feedback in the pull request.

## References

The workflow follows the contribution practices in [Open Source Guides](https://opensource.guide/how-to-contribute/). GitHub documents how to expose [contribution guidelines](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/setting-guidelines-for-repository-contributors) and configure [issue and pull request templates](https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests).
