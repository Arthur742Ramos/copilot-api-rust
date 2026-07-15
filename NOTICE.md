# Notice

## Project License

Original work in this repository is released under the
[Zero-Clause BSD License](./LICENSE). It may be used, copied, modified, and
distributed without an attribution requirement.

The upstream notice below is retained for the project this implementation was
derived from.

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

## Upstream MIT Notice

The upstream project is distributed under the MIT License. Its license notice is
preserved here:

```
MIT License

Copyright (c) 2025-, Erick Christian Purwanto, Cao Zhiyuan, and a number of
other contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
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
