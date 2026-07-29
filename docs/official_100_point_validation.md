# GFS / ECMWF / CAMS 官方 API 100 点验证

入口：`scripts/validation/official_100_point_compare.py`

验证器使用固定随机种子生成可复现的 100 个随机点：

- 35 个随机共同原生网格点；
- 35 个随机网格邻近非网格点；
- 30 个区域内均匀随机非网格点。

所有请求使用 `cell_selection=nearest`，以明确覆盖精确网格读取与非网格点选择。GFS
和 ECMWF 会直接比较双方共同支持的全部地面小时变量与全部官方日聚合变量，其中包括
`precipitation_probability_max`、`precipitation_probability_min` 和
`precipitation_probability_mean`。压力层字段全部排除，因为本验证针对公共点位预报
契约，而不是比较双方压力层库存。CAMS 官方 API 不提供日字段和中国 AQI 字段，因此
CAMS 只比较官方提供的全部共同小时字段；本服独有的中国 AQI、CAMS 日统计及其他独有
派生输出不参与官方一致性判定。

## 访问与快照

商业 API key 只允许通过 `OPEN_METEO_API_KEY` 环境变量注入，并通过
`X-Api-Key` 请求头传输。key 不会写入请求、响应、元数据或报告。

```bash
python3 scripts/validation/official_100_point_compare.py capture \
  --output /data/validation/official-100/<批次标识>
```

每个模型只发出一次包含 100 个坐标的官方多点 POST。成功后原始响应、请求体和
SHA-256 元数据会被不可变地保存在 `<输出目录>/<模型>/official/`。后续修复和复验
必须使用 `validate`，不得删除或重新抓取官方快照：

```bash
python3 scripts/validation/official_100_point_compare.py validate \
  --output /data/validation/official-100/<批次标识> \
  --local-base http://127.0.0.1:8088
```

本地 API 严格按点串行访问，并默认把小时和日字段分别按 12 个一组拆成小请求。
请求前会检查 Linux 可用内存、I/O PSI 和本机 `om-api` 进程数：内存或 I/O 暂时
紧张时等待，检测到超过两个 `om-api`（生产实例加一个验证探针）时直接拒绝运行。
点间和字段组间默认保留节流间隔，并使用输出目录内的进程锁禁止两个验证器并发。

每次复验的本地响应写入独立 `local/attempts/<尝试标识>/`，不会覆盖官方快照，
也不会与修复前的本地失败响应冲突。每个成功点生成一份不可变回执；遇到首个差异
时立即停止，并把模型、点位、字段组、变量、小时或日期、官方值和本地值写入
`report.json`。修复后再次执行会复用官方快照和已通过点位的回执。

在小内存生产服务器上需要验证另一数据根时，必须使用资源隔离运行器，不能手工
无限制地再启动一个探针 API：

```bash
scripts/validation/run_safe_official_100_validation.sh \
  --models ec \
  --output /data/validation/official-100/<批次标识> \
  --data-root /data/validation/<EC 混合批次数据根>
```

可先加 `--point-limit 1` 做受控冒烟测试；报告状态会是 `partial`，不会被误报为
100 点通过。正式一致性验证必须使用默认的 100 点。

运行器只读取已经保存的官方快照，不访问官方 API。它会：

- 用文件锁保证全机只有一个官方验证任务；
- 若已有第二个非生产 `om-api`、下载器或 WebP 进程则拒绝启动；
- 将探针限制为 `MemoryHigh=1100M`、`MemoryMax=1400M`、单核 CPU 和低 I/O 权重；
- 将 Python 验证器限制为 384 MiB、25% 单核 CPU 和低 I/O 权重；
- 无论验证成功或失败都停止探针 API。

## 生产批次冻结

数据批次与官方 API 对齐后，通过 1Panel 计划任务的状态开关停用
`OM_GFS_DOWNLOAD`、`OM_ECMWF_DOWNLOAD` 和 `OM_CAMS_DOWNLOAD`，并确认没有任务
正在执行。不得用批次锁文件冻结生产现场。

若差异来自 API 计算，部署修正后的同一 Git 修订后复验。若差异来自下载或数据
处理，可临时把下载器配置固定到目标批次并重下载；下载和发布完成后必须恢复自动
选择最新批次的生产配置，再通过 1Panel 任务开关保持验证现场不变。
