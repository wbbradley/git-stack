# AGENTS.md

This repository is managed with [`git-stack`](https://github.com/wbbradley/git-stack),
a tool for developing and managing **stacked git branches**.

If you are an LLM/agent that needs to create, restack, or open PRs for stacked
branches here, run:

```sh
git stack llms
```

That prints an exhaustive, self-contained reference — the mental model, every
subcommand and its flags, the state/config file formats, auth resolution, and
the restack / PR / sync semantics. Treat its output as the single source of
truth for driving git-stack; you should not need to read this repo's source to
operate the tool.

If `git stack` is not on PATH, see the **Installation** section of `README.md`.

<!-- OPENWIKI:START -->

## OpenWiki

This repository uses OpenWiki for recurring code documentation. Start with `openwiki/quickstart.md`, then follow its links to architecture, workflows, domain concepts, operations, integrations, testing guidance, and source maps.

The scheduled OpenWiki GitHub Actions workflow refreshes the repository wiki. Do not hand-edit generated OpenWiki pages unless explicitly asked; prefer updating source code/docs and letting OpenWiki regenerate.

<!-- OPENWIKI:END -->
