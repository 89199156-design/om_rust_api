# ECMWF IFS 0.25° 官方逐值验收

本工具只在下列条件全部成立时给出通过：

- 本地静态批次是目标 **00Z**，下载与处理任务已停止；
- 官方 live /v1/ecmwf 在取证前后仍指向同一目标 00Z；
- 500 个确定性点按顺序逐点完成；
- 每点一次本地 JSON POST 同时返回 361 个小时帧和 15 个 GMT 日帧；
- 除根对象的 generationtime_ms 与 location_id 外，JSON 字段、类型和值完全相同。

没有容差、舍入放宽、变量豁免或失败后继续。第一处差异立即停止。

## 验收源与固定窗口

通过判定的唯一官方数据源是：

- 商业：https://customer-api.open-meteo.com/v1/ecmwf
- 仅明确非商业测试：https://api.open-meteo.com/v1/ecmwf

Single Runs API 不能作为通过判定源。它的单周期结果与 live API 的滚动历史、插值及首帧继承行为不同。

目标窗口固定为：

- start_hour = RUN 00:00
- end_hour = RUN + 360h，包含端点，共 361 帧
- start_date = RUN_DATE
- end_date = RUN_DATE + 14d，共 15 个完整 GMT 日帧
- timezone = GMT

官方请求使用 application/json POST，并在所选端点的单请求权重范围内容纳尽可能多的点。Customer API 可一次返回全部 500 点；公共端点按当前 197+65 变量合同需要 3 个成功 POST，这是该端点权重约束下的理论最小值。最终 gate 的 500 点全部使用 cell_selection=land，不传 elevation，因此使用服务端 DEM 高程修正。nearest 与 elevation=nan 只可用于诊断，不计入通过。

本地请求同样使用 application/json POST，但严格限制为一次一个点。197 个小时变量与 65 个日变量必须在同一次请求中返回；不允许因 URL 或响应大小拆成多个请求。

## 变量合同

唯一 canonical 目录是：

- scripts/validation/ecmwf_variable_catalog.py
- scripts/validation/ecmwf_validation_config.json

目录指纹：

- hourly 197：a518f8c0ddfb5e11ac5661da7d6c5d588bbb56f33e5267378631947e3a52669c
- daily 65：87a46a349a767c1e015bf76ab506546865b483365f7e04c543c730d67cd65f33

本次精确复刻审计锁定的 Open-Meteo 官方源码基线为 acfe608b825da1a8b42a755297eb61121986e9da（2026-07-21）。配置加载会硬校验该完整 SHA；更换源码基线必须显式更新 canonical 模块、配置与测试，旧计划会因配置哈希变化而失效。

小时变量由 57 个非海洋 surface 字段与 14 层 × 10 类 pressure 字段组成。压力层为 1000、925、850、700、600、500、400、300、250、200、150、100、50、10 hPa；每层依次为 temperature、relative_humidity、geopotential_height、wind_u_component、wind_v_component、wind_speed、wind_direction、vertical_velocity、dew_point、cloud_cover。

明确排除 ocean_current_velocity、ocean_current_direction、sea_level_height_msl、sea_ice_thickness、showers 系列和 precipitation probability 系列。固定 daily 65 为：

~~~text
apparent_temperature_max, apparent_temperature_mean, apparent_temperature_min,
cape_max, cape_mean, cape_min,
cloud_cover_max, cloud_cover_mean, cloud_cover_min,
dew_point_2m_max, dew_point_2m_mean, dew_point_2m_min,
et0_fao_evapotranspiration_sum,
growing_degree_days_base_0_limit_50,
precipitation_hours, precipitation_sum,
pressure_msl_max, pressure_msl_mean, pressure_msl_min,
rain_sum,
relative_humidity_2m_max, relative_humidity_2m_mean, relative_humidity_2m_min,
shortwave_radiation_sum,
snowfall_sum, snowfall_water_equivalent_sum,
snow_depth_max, snow_depth_mean, snow_depth_min,
soil_moisture_0_to_7cm_mean,
soil_moisture_7_to_28cm_mean,
soil_moisture_28_to_100cm_mean,
soil_moisture_100_to_255cm_mean,
soil_moisture_0_to_100cm_mean,
soil_temperature_0_to_7cm_mean,
soil_temperature_7_to_28cm_mean,
soil_temperature_28_to_100cm_mean,
soil_temperature_100_to_255cm_mean,
soil_temperature_0_to_100cm_mean,
sunrise, sunset, daylight_duration, sunshine_duration,
surface_pressure_max, surface_pressure_mean, surface_pressure_min,
temperature_2m_max, temperature_2m_mean, temperature_2m_min,
vapor_pressure_deficit_max,
weather_code,
wind_direction_10m_dominant,
wind_gusts_10m_max, wind_gusts_10m_mean, wind_gusts_10m_min,
wind_speed_10m_max, wind_speed_10m_mean, wind_speed_10m_min,
wet_bulb_temperature_2m_max,
wet_bulb_temperature_2m_mean,
wet_bulb_temperature_2m_min,
wind_direction_100m_dominant,
wind_speed_100m_max, wind_speed_100m_mean, wind_speed_100m_min
~~~

## 00Z 首帧前置条件

Open-Meteo live 时序在新周期 hour 0 会保留上一周期值的字段只有六个：

- wind_gusts_10m
- temperature_2m_max
- temperature_2m_min
- shortwave_radiation
- precipitation
- runoff

因此生成待验静态批次时，必须先完整回放前一周期 **18Z**，再处理目标 **00Z**。snowfall 或 snowfall water equivalent 不属于上述直接 skip 列表。rain 等派生字段仍可能因 precipitation 首帧间接受影响。

验证器不会跳过 hour 0；六个字段的索引 0 与所有其他值一样严格比较。未执行 18Z → 00Z 回放时，不应开始官方取证。

主 gate 的 361 帧已经覆盖目标日 16/17Z 与 22/23Z，并会逐字段检查 Hermite 类温度/风、backwards-sum 降水及 solar 短波。目标 00Z 前一日的 16/17Z 与 22/23Z 属于旧 12Z/18Z 右侧插值 stencil 的额外 boundary diagnostic，不改变 RUN00..+360 主合同或通过判定，也不得为了该诊断静默增加官方请求数。

freeze 还会硬校验产品 catalog 的 provenance：旧 12Z 必须公开保留 12/15Z 并以 hidden + right_support + right_lookahead 保留 18/21Z；旧 18Z 必须公开保留 18/21Z，并隐藏保留次日 00/03Z。任一 source_run、valid_time、forecast_hour 或标志缺失都禁止开始 500 点验收。

## 500 点采样

范围固定为 70–140°E、0–58°N，包含 200 个原生 0.25° 网格坐标和 300 个非网格坐标。固定锚点覆盖裁剪边界、沿海、岛屿、青藏高原、喜马拉雅、帕米尔与天山；其余点由固定 seed 生成。

坐标分层用于覆盖插值、3×3 land 搜索与 DEM 修正；所有 pass/fail 请求仍统一采用默认 land+DEM 语义。计划文件内嵌 500 点及自校验哈希，修改坐标或顺序会被拒绝。

## 冻结本地批次

代码和镜像放系统盘；周期原始数据、OM、WebP、发布清单与验收快照放新数据盘。以下路径仅为示例，实际部署路径必须指向已挂载的新盘，不得回落系统盘：

~~~text
/data/om_work/ecmwf/
/data/om_raw/ecmwf_ifs025/coverages/
/data/om_raw/ecmwf_ifs025/current/
/data/om_raw/groups/ecmwf/releases/
/data/om_raw/groups/ecmwf/current/
/data/om_webp/releases/
/data/om_webp/current/
/data/validation/ecmwf/<RUN>/
~~~

Linux CLI 会硬拒绝把 plan、freeze、官方 cache 或本地 validation 输出写到这两个目录之外：/data/om_validation_snapshots、/data/validation/ecmwf。因此即使从 /opt 下运行代码，生成证据也不会回落系统盘。

操作顺序：

1. 确认新盘已按 UUID 挂载且重启后仍生效。
2. 完成 18Z → 00Z 回放、裁剪、派生、OM 和 WebP。
3. 原子发布目标 00Z。
4. 在 1Panel 停止 ECMWF 定时更新任务，并确认没有 downloader/processor 在运行。
5. 保存 1Panel 任务状态、挂载信息和发布清单快照。
6. 再创建 freeze attestation。

PowerShell 示例：

~~~powershell
python scripts/validation/ecmwf_official_compare.py `
  --config scripts/validation/ecmwf_validation_config.json `
  plan `
  --run 2026072300 `
  --output D:\ecmwf-evidence\2026072300\plan.json

python scripts/validation/ecmwf_official_compare.py `
  --config scripts/validation/ecmwf_validation_config.json `
  freeze `
  --run 2026072300 `
  --release-manifest D:\mounted-data\ecmwf\current\release.json `
  --catalog-manifest D:\mounted-data\ecmwf\current\latest.json `
  --confirm-updates-frozen `
  --output D:\ecmwf-evidence\2026072300\freeze.json
~~~

Linux 服务器使用相同参数与数据盘绝对路径。catalog 的 available_hourly_variables 必须逐项等于 197 项，available_daily_variables 必须逐项等于 65 项，available_variables 必须等于二者去重后的 257 项并集；raw inventory 应另放 available_raw_variables。freeze 会记录两个 manifest 的 SHA-256、run identity 和三组精确 API inventory。在每个本地点请求之前和之后都会重新校验；任何变化都是硬失败。

## 官方快照取证

官方取证前后分别保存两个上游探针的原始响应、响应头、请求标识和哈希：

- 时序：https://openmeteo.s3.amazonaws.com/data/ecmwf_ifs025/static/meta.json
- 空间：https://openmeteo.s3.amazonaws.com/data_spatial/ecmwf_ifs025/latest.json

时序探针必须满足 last_run_initialisation_time == TARGET_00Z 且覆盖 +360h。空间探针必须满足 completed=true、reference_time == TARGET_00Z 且 valid_times 包含 +360h。前后 identity 必须完全相同。

商业 key 只能通过环境变量 OPEN_METEO_API_KEY 注入。不要把 key 放在命令行、URL、JSON body、缓存或报告中：

~~~powershell
python scripts/validation/ecmwf_official_compare.py `
  --config scripts/validation/ecmwf_validation_config.json `
  fetch-official `
  --plan D:\ecmwf-evidence\2026072300\plan.json `
  --cache-dir D:\ecmwf-evidence\2026072300\official-cache `
  --allow-network `
  --max-new-requests 4 `
  --retries 3
~~~

Customer API 成功路径的理论最小值和实际成功请求数均为 1；公共端点在当前合同下均为 3。重试次数单独留证，429 严格服从 Retry-After，而且失败批次只会在预先绑定的同一终端重试，不会在失败后临时换终端。

公共端点允许把这 3 个批次在请求前静态分配给多个独立终端。验证器分别累计每个终端的估算请求权重，并硬拒绝任何超过配置中单终端每日上限的分配；成功、429/5xx 和传输失败证据均记录 executor_id、canonical 请求哈希、响应哈希和 Retry-After 决策。终端清单及其批次分配也写入最终自哈希 index。

以下示例使用用户提供的五台独立终端。SSH 目标可以是 `~/.ssh/config` 别名，也可以是 `user@host`；参数值不进入官方 HTTP 请求：

~~~bash
python3 scripts/validation/ecmwf_official_compare.py \
  --config scripts/validation/ecmwf_validation_config.json \
  fetch-official \
  --plan /data/validation/ecmwf/2026072300/plan.json \
  --cache-dir /data/validation/ecmwf/2026072300/official-cache \
  --official-endpoint https://api.open-meteo.com/v1/ecmwf \
  --allow-public-noncommercial \
  --allow-network \
  --max-new-requests 6 \
  --retries 1 \
  --public-ssh-executor terminal-81=ubuntu@81.69.253.110 \
  --public-ssh-executor terminal-161=ubuntu@43.161.255.215 \
  --public-ssh-executor terminal-156=ubuntu@43.156.81.216 \
  --public-ssh-executor terminal-162=ubuntu@43.162.112.201 \
  --public-ssh-executor terminal-128=ubuntu@43.128.154.63
~~~

分配是确定性的：相同计划、配置和终端顺序得到相同绑定。某终端返回 429 时，验证器遵守 Retry-After 并留在原终端；若最终失败则停止，不会把该批次转给其他终端。

每个官方批次会把 canonical JSON request body 单独保存并哈希。若公共诊断因单请求权重被分批，每批重复同一 sentinel；同一语义的 sentinel 361 小时 + 15 日规范化哈希必须一致，否则整份快照无效。

SSH 执行器使用标准 `Accept-Encoding: gzip` 接收大响应，并在执行器内完整解压后再计算响应字节数、SHA-256 和回传原始 JSON。压缩仅作用于 HTTP 传输，不改变 canonical 请求、落盘响应或逐值比较语义。

## 串行本地比对

~~~powershell
python scripts/validation/ecmwf_official_compare.py `
  --config scripts/validation/ecmwf_validation_config.json `
  validate `
  --plan D:\ecmwf-evidence\2026072300\plan.json `
  --cache-dir D:\ecmwf-evidence\2026072300\official-cache `
  --freeze-attestation D:\ecmwf-evidence\2026072300\freeze.json `
  --local-endpoint http://127.0.0.1:8088/v1/ecmwf `
  --output-dir D:\ecmwf-evidence\2026072300\local-validation
~~~

执行顺序固定为 point 0 → point 499。每点流程为：检查 freeze、保存本地 POST body、请求完整小时与日数据、完成严格比较、再次检查 freeze、写 response/meta/receipt，然后才进入下一点。

严格 JSON 规则包括：

- 1 与 1.0 不同；
- true 与 1 不同；
- 0.0 与 -0.0 不同；
- null、缺字段、多字段、单位、时间轴或数组长度变化均失败；
- 只忽略每个响应根对象的 generationtime_ms 与 location_id；
- 不忽略嵌套同名字段。

第一处差异报告包含 JSON path、官方/本地值与类型、点坐标、原始请求、原始响应、manifest 前后哈希及官方行哈希。已通过点生成不可变 receipt；中断后只能从连续 receipt 前缀恢复。缓存、计划、freeze 或合同哈希不一致时必须使用新目录，不能覆盖旧证据。

## 无 Customer key 的 dry-run

没有商业 key 时可以安全执行：

1. canonical 目录与配置加载；
2. plan 生成；
3. 本地 freeze；
4. 离线单元测试；
5. 对已存在且哈希完整的官方 cache 做纯离线复验；
6. loopback mock 仅通过隐藏测试开关执行，不可出现在生产报告中。

Single Runs API 不能替代 live ECMWF 通过判定源。没有 Customer key 时，可按上一节通过多个独立公共终端做静态分片；每个终端都必须在其公开配额内，且必须保留验证器生成的完整分配与请求证据。

## 测试

~~~powershell
python -m py_compile `
  scripts/validation/ecmwf_variable_catalog.py `
  scripts/validation/ecmwf_official_compare.py

python -m unittest discover `
  -s scripts/validation/tests `
  -p 'test_ecmwf_official_compare.py' `
  -v
~~~

离线测试覆盖 canonical 197/65/257 指纹、land+DEM 500 点合同、旧 run 隐藏右侧 stencil、公共批次静态分片与单终端额度、官方最小 POST、executor 成功/重试留证、key 不落盘、本地逐点 POST、完整通过、日帧首差即停、六个 hour-0 字段、严格 JSON 类型、429 留证、sentinel 漂移和 manifest 点间变化。

CLI 返回码：

- 0：命令成功，或全部 500 点完全一致；
- 1：发现第一处官方/本地差异；
- 2：配置、快照、冻结状态、额度、网络或证据完整性错误。
