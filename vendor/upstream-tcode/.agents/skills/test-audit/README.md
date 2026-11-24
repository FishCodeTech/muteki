# test-audit

Adapted from OpenClaw's
[`test-audit` skill](https://github.com/openclaw/openclaw/tree/main/.agents/skills/test-audit)
at commit
[`80930af`](https://github.com/openclaw/openclaw/tree/80930af448ebabc84174146b56bc106d37fab3b4/.agents/skills/test-audit).
The workflow and value bar are upstream's. The Tcode version defers to
CONTRIBUTING.md's **Tests earn their maintenance**, and replaces the
Vitest/TypeScript tooling, OpenClaw paths and skills, and Telegram campaign
examples with Cargo, this workspace's crates, and PR #487. To pull in upstream
changes, diff upstream between the commit above and its current `main`, apply
what fits, and bump the commit.

Skills live in `.agents/skills/`; `.claude/skills` is a symlink to it so Claude
Code loads the same files as the other agents.

## License

MIT License

Copyright (c) 2026 OpenClaw Foundation

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
