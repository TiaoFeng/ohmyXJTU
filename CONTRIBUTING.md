# 贡献指南

感谢您对 ohmyXJTU 的关注！本项目目前处于公开测试版（beta），任何反馈与贡献都来之不易。

贡献之前，烦请您先阅读 [README](README.md) 了解功能与已知限制，并阅读[用户协议](PRIVACY.md)了解本项目的定位与边界。

## 目录

- [反馈问题](#反馈问题)
- [贡献流程](#贡献流程)
- [开发环境](#开发环境)
- [构建与检查](#构建与检查)
- [代码与测试规范](#代码与测试规范)
- [安全与隐私](#安全与隐私)
- [社区规范](#社区规范)
- [提交 Pull Request](#提交-pull-request)
- [许可证](#许可证)

## 反馈问题

- **遇到 bug**：请使用「[反馈问题](https://github.com/tiaofeng/ohmyXJTU/issues/new?template=bug_report.yml)」模板，并按模板要求提供版本号、复现步骤、期望与实际结果、错误输出。
- **功能建议**：请使用「[功能建议](https://github.com/tiaofeng/ohmyXJTU/issues/new?template=feature_request.yml)」模板。所建议的功能必须遵守《网络安全法》等国家法律法规与《西安交通大学学生违纪处分办法》等校纪校规，且不得用于获取不正当的竞争优势或利益（如「抢课」「抢场地」）。
- **不确定是不是 bug，或想讨论想法**：请移步 [Discussions](https://github.com/tiaofeng/ohmyXJTU/discussions)。

提交前，烦请您先在 [issues](https://github.com/tiaofeng/ohmyXJTU/issues) 搜索一遍（包括已关闭的），或许已有答案。

**安全漏洞请勿公开开 issue**，请参见下方[安全与隐私](#安全与隐私)。

## 贡献流程

1. Fork 本仓库。
2. 从 **`test/alpha`** 分支拉出您的特性分支。`test/alpha` 是本项目的开发分支，`main` 为发布/稳定分支。
3. 实现您的改动，并为每个 bug 修复或新功能**新增或更新可复现旧问题的回归测试**。
4. 本地运行 `just check`，确保全部通过。
5. 按 [Pull Request 模板](.github/pull_request_template.md) 提交 PR，**合并目标为 `test/alpha` 分支**。

> 您须确认合并目标为 `test/alpha` 并理解向其它分支提交的合并请求可能不会被处理。

## 开发环境

- Rust **≥ 1.88**（见 `Cargo.toml` 的 `rust-version`）。
- [`just`](https://github.com/casey/just) 命令运行器。
- 覆盖率工具：`cargo-llvm-cov`，以及 rustup 的 `llvm-tools` 组件（`just check` 的覆盖率步骤依赖它们）。

## 构建与检查

从源码构建（详见 [README](README.md)）：

```bash
cargo build --release
# 产物: target/release/ohmyXJTU
```

常用检查命令：

| 命令 | 作用 |
| --- | --- |
| `just fmt` | 格式检查（`cargo fmt --check`） |
| `just ppy` | Clippy，且警告视为错误（`-D warnings`） |
| `just test` | 运行全部单元测试 |
| `just cover` | 运行测试并生成覆盖率报告 |
| **`just check`** | **提交要求：fmt + clippy + test + 覆盖率，全部必须通过** |

## 代码与测试规范

### 项目结构

代码按职责拆分在 `src/` 下的各模块中，请将改动放在合适的模块，避免把大量功能堆砌在同一个文件或函数里：

| 模块 | 职责 |
| --- | --- |
| `auth` | 统一认证登录（RSA 加密、验证码、短信验证、WebVPN 地址转换） |
| `credentials` | 账号与密码的加密保管（Argon2id + ChaCha20-Poly1305） |
| `domain` | 领域逻辑（课表、考勤匹配、作业汇总、学期） |
| `http` | HTTP 客户端抽象与实现 |
| `session` | 会话管理、访问模式（直连 / WebVPN / 自动）与站点路由 |
| `sites` | 学校各站点接口（考勤 `attendance`、思源学堂 `lms`） |
| `task` | 后台任务调度（登录、数据加载、缓存） |
| `tui` | 界面（主题、按键处理、事件、各页面视图） |
| `system` / `privacy` | 系统集成（打开浏览器）、用户协议文本 |
| `io` / `json` / `text` / `tone` / `model` / `error` | 基础设施（安全写入、JSON 解码、文本宽度、语义色、共享模型、错误类型） |

### 测试

- 单元测试**超过 20 行**者放在被测模块**同级的 `tests/` 目录**，并在源文件末尾以 `#[path]` 引入，而非使用 `mod`：

  ```rust
  // src/json.rs 末尾
  #[cfg(test)]
  #[path = "tests/json_test.rs"]
  mod json_test;
  ```

  例如 `src/json.rs` 的测试放在 `src/tests/json_test.rs`，`src/task/worker.rs` 的测试放在 `src/task/tests/`。
- 测试**全部使用脱敏的固定响应、假 HTTP 客户端与临时目录**，不访问真实学校服务，也不写入真实凭据。HTML 之类的响应样本放在 `tests/fixtures/`，提交前请确认其中不含任何个人信息。
- bug 修复与新功能都必须附带回归测试：测试应当在修复前失败、修复后通过。
- 需要必要的错误处理，使用 `AppError` / `AppResult` 传递错误，在非测试代码中不允许使用 `unwrap()` / `expect()` 处理可预期的失败路径。（注意：clippy 默认不拦截 `unwrap()`/`expect()`，`just check` 不会自动发现，这属于人工审查项。）

### 新增依赖

**增加依赖必须先征得维护者同意**，且依赖必须是可信、必要的。若确有必要，请**先开 issue 说明用途与来源并等待确认**，获同意后再提交修改 `Cargo.toml` 的 PR。

## 安全与隐私

本项目涉及统一认证凭据与个人教务数据，请严格遵守：

- **禁止提交到仓库**：账号、密码、加密口令、cookie、ticket、token、真实服务器响应、私人课程/考勤/作业数据。
- 上述内容同样适用于测试 fixture、日志、PR 描述、截图与录屏。测试证据中**不要填写密钥、cookie 或其它凭据**。
- 不要在 issue 中粘贴含有上述内容的输出；确需展示时请先脱敏。
- **发现安全漏洞请勿公开开 issue**，请通过 [GitHub Security Advisory 的「Report a vulnerability」](https://github.com/tiaofeng/ohmyXJTU/security/advisories/new) 私密披露，我们会私下跟进。

## 社区规范

本项目的 Issues、Pull Requests、Discussions 及评论区由志愿者维护，参与互动即表示您同意以下约定：

- **内容责任**：您发布的内容（文字、代码、图片、链接、评论等）由您自行承担法律责任，不代表维护者的观点、立场或政策；维护者不对用户发布的内容做事前审查。
- **禁止发布**：请勿发布违法违规信息，也请勿发布他人的个人信息或隐私（真实姓名、学号、联系方式、课程记录、成绩、照片等）。同时请遵守 [GitHub 社区准则](https://docs.github.com/zh/site-policy/github-terms/github-community-guidelines)。
- **管理措施**：对违反上述约定的内容，维护者有权删除、隐藏、锁定或限制传播，视情况关闭或锁定相关 Issue / PR、限制参与本社区。
- **举报与处置**：如发现本社区存在违法违规或侵权内容，可通过评论右上角的 **Report abuse**、[开 issue](https://github.com/tiaofeng/ohmyXJTU/issues) 或 [Security Advisory](https://github.com/tiaofeng/ohmyXJTU/security/advisories/new)（涉及隐私时）告知维护者，我们核实后会及时删除或屏蔽，并按需转送通知。
- **响应与准则**：本社区由志愿者维护，响应可能存在延迟，敬请谅解；社区互动同时受 [行为准则](.github/CODE_OF_CONDUCT.md) 约束。

## 提交 Pull Request

- 合并目标为 **`test/alpha`** 分支；一个 PR 只处理一个主题（一个 bug 或一个功能），避免混杂无关改动。
- 按 PR 模板填写「背景 / 原因 / 修复 / 改动文件」，并完成检查单。
- 在 PR 中附上 `just check` 的运行结果；若有无法运行的测试或环境限制，请在「测试证据」中列明。
- 每个 bug 修复 / 功能实现，都必须新增或更新可复现旧问题的回归测试。
- 提交前请确认没有引入任何[安全与隐私](#安全与隐私)中的内容。

### AI 辅助开发

- 本项目允许使用 AI 辅助编写代码与测试（README 中亦已注明本项目由 AI 构建）。
- 无论代码由谁构建，请您理解提交者本人（自然人）须对改动的正确性、安全性负责。
- AI 产出同样须通过 `just check`，并遵守本文全部规范——尤其是脱敏与凭据红线：不得提交 AI 生成或转述的真实凭据与响应。

## 许可证

- 本项目使用 [BSD 3-Clause License](LICENSE) 开源。
- 提交贡献即表示您同意按相同的 BSD 3-Clause 许可授权您的改动。
