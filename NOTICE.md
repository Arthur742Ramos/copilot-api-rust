# Notice

## Project Lineage

`copilot-api-rust` is a Rust port of [`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api)
(published on npm as [`@jeffreycao/copilot-api`](https://www.npmjs.com/package/@jeffreycao/copilot-api)),
a TypeScript gateway that wraps the GitHub Copilot API and exposes
OpenAI- and Anthropic-compatible endpoints.

This project is an independent re-implementation of that software in Rust. The
behavior, route table, configuration schema, and CLI are translated from the
upstream TypeScript source. All credit for the original design belongs to the
upstream authors and contributors.

The upstream project itself was originally forked from
[`ericc-ch/copilot-api`](https://github.com/ericc-ch/copilot-api) and is now
maintained independently as `caozhiyuan/copilot-api`.

## Original Attribution

The upstream project is distributed under the MIT License. Its copyright notice
is preserved here:

```
MIT License

Copyright (c) 2025-, Erick Christian Purwanto, Cao Zhiyuan, and a number of
other contributors
```

Many thanks to Erick Christian Purwanto, Cao Zhiyuan, and all contributors to
the upstream projects for the foundation this port builds on.

## GitHub Copilot Security Notice

Excessive automated or scripted use of Copilot (including rapid or bulk
requests, such as via automated tools) may trigger GitHub's abuse-detection
systems.

You may receive a warning from GitHub Security, and further anomalous activity
could result in temporary suspension of your Copilot access.

GitHub prohibits use of their servers for excessive automated bulk activity or
any activity that places undue burden on their infrastructure.

Please review:

- [GitHub Acceptable Use Policies](https://docs.github.com/site-policy/acceptable-use-policies/github-acceptable-use-policies#4-spam-and-inauthentic-activity-on-github)
- [GitHub Copilot Terms](https://docs.github.com/site-policy/github-terms/github-terms-for-additional-products-and-features#github-copilot)

Use this project responsibly to avoid account restrictions.
