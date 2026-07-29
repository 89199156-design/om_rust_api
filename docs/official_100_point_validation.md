# GFS / ECMWF / CAMS 官方 API 100 点验证

入口：`scripts/validation/official_100_point_compare.py`

验证器使用固定随机种子生成可复现的 100 个随机点：

- 35 个随机共同原生网格点；
- 35 个随机网格邻近非网格点；
- 30 个区域内均匀随机非网格点。

所有请求使用 `cell_selection=nearest`，以明确覆盖精确网格读取与空间插值。GFS 和
ECMWF 会直接比较双方共同支持的全部官方小时变量与全部官方日聚合变量，其中包括
`precipitation_probability_max`、`precipitation_probability_min` 和
`precipitation_probability_mean`。压力层只包含官方同模型实际提供的共同层级和变量。
CAMS 官方 API 不提供日字段和中国 AQI 字段，因此 CAMS 只比较官方提供的全部共同
小时字段；本服独有的中国 AQI、CAMS 日统计及其他独有派生输出不参与官方一致性判定。

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

本地 API 严格按点串行访问。每个成功点生成一份不可变回执；遇到首个差异时立即
停止，并把模型、点位、变量、小时或日期、官方值和本地值写入 `report.json`。
修复后再次执行相同命令会复用官方快照和已通过的点位回执。

## 生产批次冻结

数据批次与官方 API 对齐后，通过 1Panel 计划任务的状态开关停用
`OM_GFS_DOWNLOAD`、`OM_ECMWF_DOWNLOAD` 和 `OM_CAMS_DOWNLOAD`，并确认没有任务
正在执行。不得用批次锁文件冻结生产现场。

若差异来自 API 计算，部署修正后的同一 Git 修订后复验。若差异来自下载或数据
处理，可临时把下载器配置固定到目标批次并重下载；下载和发布完成后必须恢复自动
选择最新批次的生产配置，再通过 1Panel 任务开关保持验证现场不变。
