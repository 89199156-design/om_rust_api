# GFS / ECMWF / CAMS 官方 API 300 点验证

入口：`scripts/validation/official_200_point_compare.py`

验证器使用固定随机种子生成可复现的 300 个随机点：

- 100 个随机共同原生网格点；
- 100 个随机网格邻近非网格点；
- 100 个区域内均匀随机非网格点。

所有请求使用 `cell_selection=nearest`，以明确覆盖精确网格读取与非网格点选择。GFS
和 ECMWF 会直接比较双方共同支持的全部地面小时变量与全部官方日聚合变量，其中包括
`precipitation_probability_max`、`precipitation_probability_min` 和
`precipitation_probability_mean`。压力层字段全部排除，因为本验证针对公共点位预报
契约，而不是比较双方压力层库存。CAMS 官方 API 不提供日字段和中国 AQI 字段，因此
CAMS 只比较官方提供的全部共同小时字段；本服独有的中国 AQI、CAMS 日统计及其他独有
派生输出不参与官方一致性判定。

## 访问与快照

默认使用 Open-Meteo 无 key 的公开 API 免费额度；只有明确使用商业额度时，才允许
通过 `OPEN_METEO_API_KEY` 环境变量注入，并通过 `X-Api-Key` 请求头传输。key 不会
写入请求、响应、元数据或报告。公开出口额度不足时可用
`--official-ssh-hosts <SSH别名列表>` 轮换已有服务器的公网出口，但快照和验证程序始终
留在本机，不复制到服务器。

```bash
python scripts/validation/official_200_point_compare.py capture \
  --output D:/Projects/weather_validation_artifacts/official-300/<批次标识>
```

每个模型按 100 个坐标一组，仅发出 3 次多点 POST；每次请求同时包含完整小时字段
和完整日聚合字段。成功后每批原始响应、无密钥请求体、合并响应和对应 SHA-256
元数据会被不可变地保存在 `<输出目录>/<模型>/official/`。后续修复和复验必须使用
`validate`，不得删除或重新抓取官方快照：

```bash
python scripts/validation/official_200_point_compare.py validate \
  --official-snapshot-root D:/Projects/weather_validation_artifacts/official-300/<批次标识> \
  --output D:/Projects/weather_validation_artifacts/singapore/<批次标识> \
  --local-base http://127.0.0.1:8088 \
  --local-ssh-host singapore
```

`--local-ssh-host` 复用一条持久 SSH 会话，并在真实生产服务器上请求回环地址；不会复制
数据、端口转发或启动影子实例。上海验证使用独立 `--output` 和对应 SSH 别名，二者
共同只读复用同一个 `--official-snapshot-root`，因此官方约 100 MB 样本只保存一份。

本地 API 严格按点串行访问，并在 URL 长度允许时把该模型的全部小时和日字段合并为
同一请求。
请求前会检查 Linux 可用内存、I/O PSI 和本机 `om-api` 进程数：内存或 I/O 暂时
紧张时等待，检测到超过一个生产 `om-api` 时直接拒绝运行。
点间和字段组间默认保留节流间隔，并使用输出目录内的进程锁禁止两个验证器并发。

每次复验的本地响应和成功点回执分别写入独立的尝试目录，不会覆盖官方快照，
也不会与修复前的本地失败响应冲突。遇到首个差异
时立即停止，并把模型、点位、字段组、变量、小时或日期、官方值和本地值写入
`report.json`。修复后再次执行只复用官方快照，必须从第 0 个点重新验证；旧回执仅作审计证据。

官方 API 可能按固定日历窗口返回超出原始模型最后一个小时的连续尾段。GFS、ECMWF
和 CAMS 均只比较双方原始小时轴的共同覆盖；只有本服原始模型最后一小时之后的官方
连续尾段可被豁免，开头缺失、小时轴中断以及共同覆盖内的任何值差异仍立即失败。
日聚合统一排除官方时间轴的最后一日（该日可能不足 24 小时），此前所有日期严格比较。
报告分别统计实际比较值与因上述规则豁免的值，二者不得混计。

可先加 `--point-limit 1` 对当前生产 API 做冒烟测试；报告状态会是 `partial`，不会被
误报为 300 点通过。正式一致性验证必须直接访问生产 API，并使用默认的 300 点。

## 生产批次冻结

数据批次与官方 API 对齐后，只通过 1Panel 面板停用当前生产探测任务
`weather_gfs_probe_cycle`、`weather_ecmwf_probe_cycle` 和
`weather_cams_ecpds_probe_cycle`，并在面板确认没有任务正在执行。不得通过命令修改
1Panel 配置、创建配置备份或用批次锁文件冻结生产现场。

若差异来自 API 计算，部署修正后的同一 Git 修订后复验。若差异来自下载或数据处理，
必须先校验并复用服务器已保留的目标批次；只有文件缺失、校验失败或覆盖范围不完整时
才允许重新下载。发布完成后必须恢复自动选择最新批次的生产配置，再通过 1Panel 任务
开关保持验证现场不变。
