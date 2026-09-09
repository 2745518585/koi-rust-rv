# 第三方依赖许可证声明

本文件只说明第三方依赖的许可证，不改变本项目自有代码的许可证。项目自有代码采用 MIT 许可证，完整文本见 [LICENSE](LICENSE)。第三方依赖仍按各自的许可证发布，不能因为本项目采用 MIT 就将它们统称为 MIT。

以下清单以当前锁文件为准：

- Rust：`Cargo.lock`，由 workspace 的 `Cargo.toml` 解析得到；
- Web：`web/package-lock.json`，包含 218 个锁定的 npm 包记录；
- 版本或依赖关系变化后，应重新生成或复核本文件。

## 直接依赖

### Rust

下表列出当前实际被 workspace 成员使用的主要直接依赖。`sqlx` 和 `tokio-stream` 目前只是 workspace 预留声明，尚未进入当前锁定依赖图。

| 依赖 | 锁定版本 | 许可证声明 |
| --- | --- | --- |
| `argon2` | 0.5.3 | MIT OR Apache-2.0 |
| `async-trait` | 0.1.92 | MIT OR Apache-2.0 |
| `axum` | 0.8.9 | MIT |
| `bytes` | 1.12.1 | MIT |
| `chrono` | 0.4.45 | MIT OR Apache-2.0 |
| `futures-util` | 0.3.34 | MIT OR Apache-2.0 |
| `rand` | 0.8.8 | MIT OR Apache-2.0 |
| `reqwest` | 0.13.4 | MIT OR Apache-2.0 |
| `serde` | 1.0.229 | MIT OR Apache-2.0 |
| `serde_json` | 1.0.151 | MIT OR Apache-2.0 |
| `thiserror` | 2.0.20 | MIT OR Apache-2.0 |
| `toml` | 0.9.12+spec-1.1.0 | MIT OR Apache-2.0 |
| `tokio` | 1.53.1 | MIT |
| `tokio-tungstenite` | 0.30.0 | MIT |
| `tokio-util` | 0.7.19 | MIT OR Apache-2.0 |
| `tower-http` | 0.7.1 | MIT |
| `tracing` | 0.1.44 | MIT |
| `tracing-appender` | 0.2.5 | MIT |
| `tracing-subscriber` | 0.3.23 | MIT |
| `uuid` | 1.26.0 | Apache-2.0 OR MIT |

workspace 内的 `koi-api`、`koi-core`、`koi-infra`、`koi-console`、`koi-model-proxy` 和 `koi-server` 均声明为 MIT。

### Web

| 依赖 | 锁定版本 | 许可证声明 |
| --- | --- | --- |
| `lucide-react` | 0.468.0 | ISC |
| `react` | 19.2.8 | MIT |
| `react-dom` | 19.2.8 | MIT |
| `react-markdown` | 9.0.1 | MIT |
| `remark-gfm` | 4.0.1 | MIT |
| `@types/react` | 19.2.18 | MIT |
| `@types/react-dom` | 19.2.5 | MIT |
| `@vitejs/plugin-react` | 5.2.0 | MIT |
| `typescript` | 5.8.3 | Apache-2.0 |
| `vite` | 7.3.6 | MIT |

## 需要特别保留的第三方声明

这些依赖没有禁止本项目采用 MIT，但发布构建产物时不能只保留项目自己的 `LICENSE`。

### Rust 运行时依赖

| 依赖 | 许可证 | 处理要求 |
| --- | --- | --- |
| `webpki-roots` 0.26.11、1.0.9；`webpki-root-certs` 1.0.9 | CDLA-Permissive-2.0 | 这些包包含来自 Common CA Database 的根证书数据。共享数据时应同时提供 CDLA-Permissive-2.0 协议文本。参见 [rustls/webpki-roots](https://github.com/rustls/webpki-roots)。 |
| ICU4X 相关包（`icu_*`、`litemap`、`potential_utf`、`tinystr`、`writeable`、`yoke*`、`zerofrom*`、`zerotrie`、`zerovec*`） | Unicode-3.0 | 保留 Unicode 的版权和许可声明。参见 [ICU4X LICENSE](https://github.com/unicode-org/icu4x/blob/main/LICENSE)。 |
| `unicode-ident` 1.0.24 | MIT OR Apache-2.0 AND Unicode-3.0 | 除 MIT/Apache 选项外，仍需保留 Unicode 声明。 |
| `ring` 0.17.14 | Apache-2.0 AND ISC | 保留其 Apache、ISC、BoringSSL 和其他上游声明。 |
| `aws-lc-rs` 1.18.1、`aws-lc-sys` 0.45.0 | Apache-2.0、ISC、MIT、BSD-3-Clause 等组合 | 保留 AWS-LC 包内的许可证和第三方组件声明。 |
| `rustls-webpki` 0.103.15、`untrusted` 0.9.0 | ISC | 保留 ISC 声明。 |
| `subtle` 2.6.1、`matchit` 0.8.4 | BSD-3-Clause（或包含 BSD-3-Clause） | 保留 BSD 版权声明。 |

其他 Rust 依赖还使用 BSD-2-Clause、Zlib、Unlicense、CC0、MIT-0、BSL-1.0 等宽松许可证；当前没有发现 GPL/AGPL/SSPL 依赖。`r-efi` 的表达式包含 LGPL-2.1-or-later 选项，但同时提供 MIT 和 Apache-2.0 选项，当前可选择宽松许可证分支，不构成强制 LGPL 依赖。

### Web 构建依赖

| 依赖 | 许可证 | 处理要求 |
| --- | --- | --- |
| `caniuse-lite` 1.0.30001810 | CC-BY-4.0 | 这是浏览器兼容性数据，需注明来源 `caniuse.com`。该包主要由构建工具间接使用，通常不会进入浏览器运行时 bundle。参见 [caniuse-lite LICENSE 说明](https://github.com/browserslist/caniuse-lite)。 |
| `source-map-js` 1.2.1 | BSD-3-Clause | 保留 BSD 版权声明。 |
| `@ungap/structured-clone`、`electron-to-chromium`、`lru-cache`、`picocolors`、`semver`、`yallist` | ISC | 保留 ISC 声明。 |

## 分发要求

1. 发布源代码时，保留 `LICENSE`、本文件以及两个锁文件。
2. 发布 Rust 二进制或 Docker 镜像时，将本文件放入镜像或发行包，并同时提供依赖包要求的许可证/版权文本，尤其是 Apache-2.0、Unicode-3.0 和 CDLA-Permissive-2.0。
3. 发布包含前端构建工具或 `node_modules` 的包时，保留 npm 依赖的许可证信息，并保留 `caniuse.com` 的数据来源说明。
4. 依赖升级、增加新依赖或改变 Cargo/npm feature 后，重新检查许可证表达式和上游 `LICENSE`/`NOTICE` 文件。

本声明是工程层面的依赖许可证清单，不构成法律意见；若将项目用于商业发行或再分发给大量用户，应在发布前进行一次正式法律复核。
