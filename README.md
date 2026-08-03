# om_rust_api

上海和新加坡生产环境共有的 Rust 点位 API 与 WebP 生成仓库。

本仓库只消费已经发布的 Open-Meteo Native/OM 数据，不下载官方 OM，也不把原始 GFS、ECMWF、CAMS 数据转换为 OM：

- `om_rust_api`：共有 Rust API、WebP、部署脚本和跨服务器一致性验证。
- `om_data_raw`：仅新加坡，Swift 将官方原始模型数据生成 OM。
- `om_data_om`：仅上海，直接从官方 Open-Meteo 桶下载 OM 并发布。

三个仓库相互独立，不使用子模块，不复制彼此的生产流水线。

## 组件

- `om_api/`：Rust HTTP API、OM 解码、插值、小时/日聚合、派生变量、数据身份和归属接口。
- `webp/om_webp/`：Rust WebP 渲染器与部署/校验脚本。
- `scripts/`：API 安装、原生解码库构建和官方/双服务器一致性验证。
- `nginx/`：API、来源归档和 WebP 的生产反向代理配置。

仓库不包含下载器、Swift 生产代码、运行时天气数据、编译产物、容器镜像或服务器备份。

## 数据根目录

同一个二进制通过环境变量读取不同来源：

- 新加坡：`OM_DATA_ROOT=/opt/1panel/apps/weather_forecast_server/data/om_producer`
- 上海：`OM_DATA_ROOT=/data/om_raw`
- DEM：`OM_DEM_ROOT=<部署提供的 Copernicus DEM90 根目录>`
- 固定模型高程：`OM_MODEL_STATIC_ROOT=/opt/1panel/apps/weather_om_api`

API 在启动或 HUP 时构建只读快照；生产流水线只在完整批次原子发布后刷新一次。WebP 读取同一批次标识，生成完整不可变 release 后再切换 current。

## 本地验证

```powershell
cd D:\Projects\om_rust_api
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
python -m pytest tests scripts/validation/tests webp/om_webp/tests -q
```

Linux 部署脚本还必须通过：

```bash
find scripts webp/om_webp/scripts -type f -name '*.sh' -print0 | xargs -0 -n1 bash -n
```

## 生产安装

安装脚本拒绝脏工作树，并把精确 Git revision、二进制 SHA-256 和对应源码归档写入部署目录。

```bash
bash scripts/install_om_api.sh
bash webp/om_webp/scripts/install_om_webp.sh
```

完成部署后必须满足：

- GitHub `main`、服务器仓库 `main`、API/WebP `source-revision` 完全相同；
- 两台服务器都只保留生产分支和当前部署版本；
- `/v1/source` 指向同一 revision 的 `om_rust_api-<sha>.tar.gz`；
- `/v1/data-identity` 与 WebP current marker 引用预期模型批次；
- 服务预热后点位请求达到轻量服务器的毫秒级本机响应。

## 验证工具

`scripts/validation/official_200_point_compare.py` 使用已保存的官方免费 API 样本进行三方比较；`scripts/validation/sequential_2000_server_parity.py` 按点严格顺序比较上海与新加坡的网格/非网格、全小时和全部日聚合输出。验证数据和报告是外部测试证据，不进入本仓库。

## 许可与数据来源

源代码许可见 `LICENSE`；第三方软件见 `THIRD_PARTY_NOTICES.md` 和 `LICENSES/`；运行时数据来源、转换和归属见 `DATA_SOURCES.md`。部署与发布前必须复核上游当前条款、署名、隐私和商业使用要求。
