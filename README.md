# PT Manager

PT Manager 是一个面向 PT 站点的桌面保活工具。应用通过 Chrome DevTools Protocol（CDP）启动并管理独立的 Chrome Profile，按计划或手动打开已配置站点，复用登录状态，并可按需完成自动登录、CookieCloud 同步、验证码识别和 Gotify 通知。

当前项目以 Windows 桌面端为主要使用环境，技术栈为 Tauri 2、React 19、TypeScript 和 Rust。

## 主要功能

### 站点保活

- 支持手动立即保活和 Cron 定时保活。
- 每个站点可单独开启或关闭自动保活，也可以批量启用、停用和删除。
- 手动执行时会批量打开所有已启用站点，停留指定时间后关闭标签页。
- 定时执行时会按站点顺序访问，每个页面停留“配置时长 + 0～9 秒随机抖动”，站点之间再随机等待 5～14 秒。
- 定时任务可在 Cron 触发后增加一段随机延迟，降低每天固定时间访问的规律性。
- 任务执行过程中可以手动终止；同一时间只允许一个保活任务运行。

### 专用 Chrome 环境

- 自动检测本机 Google Chrome，执行任务时自动启动，无需手工添加调试参数。
- 使用独立的 Chrome Profile，不影响用户日常使用的 Chrome 数据。
- 优先使用设置中的 CDP 端口；端口冲突时可自动选择可用端口。
- 可提前准备 Chrome 环境，也可在设置中清除专用 Profile 的 Cookie、Local Storage 和缓存。
- 首次使用时可以先在专用 Chrome 中人工登录，后续任务会继续复用该登录状态。

### 自动登录与 2FA

站点开启“需要自动登录”后，应用会在保活过程中检查登录状态，并在需要时填写账号和密码。

当前登录适配规则如下：

| 站点类型 | 识别方式 | 账号密码 | TOTP 2FA | OCR 验证码 |
| --- | --- | --- | --- | --- |
| M-Team | URL 包含 `kp.m-team.cc` | 支持 | 支持 | 不使用 |
| HDKylin | URL 包含 `hdkyl.in` | 支持 | 支持 | 不使用 |
| 通用 NexusPHP | 除上述两类外按 NexusPHP 流程处理 | 支持 | 支持 | 支持 |

补充说明：

- TOTP 密钥应填写 Base32 原始密钥。站点列表会显示当前 6 位验证码及剩余有效时间。
- 通用 NexusPHP 登录会读取页面上的剩余登录尝试次数。
- 当剩余次数小于或等于安全阈值时，应用会停止自动填写、OCR 和登录重试，避免继续消耗次数。
- 站点列表中的“测试”可单独验证自动登录；通用 NexusPHP 站点还可以使用“识别码”识别当前页面验证码。
- 通用流程依赖页面结构，非 NexusPHP 站点不保证能够自动登录，但仍可用于普通页面保活。

### CookieCloud 同步

应用可与浏览器 CookieCloud 插件使用同一组服务地址、UUID 和密码：

- 手动把 CookieCloud 中的数据同步到专用 Chrome。
- 保活前自动同步 Cookie 和 Local Storage。
- 同步完成后刷新已打开的目标站点，使新登录态立即生效。
- 可在同步完成 15 秒后自动关闭为写入 Local Storage 而打开的标签页。
- 保活结束后把专用 Chrome 中最新的站点 Cookie 加密上传回 CookieCloud。
- 只处理与已配置站点域名严格匹配的数据，不会把 CookieCloud 中的全部浏览器数据写入专用 Chrome。

推荐操作顺序：

1. 在浏览器 CookieCloud 插件中填写服务地址、UUID 和密码。
2. 在插件中保存需要同步的域名关键词，并执行一次手动同步。
3. 在 PT Manager 的“设置 -> Cookie 同步”中填写同一组信息并保存。
4. 先点击“同步 Cookie”确认连接和匹配结果，再按需开启保活前同步或保活后上传。

服务地址可填写完整的 `http://` 或 `https://` 地址，也可以填写域名。公网域名会优先尝试 HTTPS，本机服务可以使用 HTTP。

### OCR 验证码识别

OCR 用于通用 NexusPHP 登录页的 6 位英文数字验证码。服务需要提供以下接口：

| 接口 | 方法 | 用途 |
| --- | --- | --- |
| `/status` | GET | 检查服务和模型状态 |
| `/initialize` | POST | 初始化 OCR 模型 |
| `/ocr` | POST | 识别 Base64 图片 |

保存 OCR 设置或开始识别时，应用会检查服务状态，必要时自动初始化模型。识别前会对图片做灰度和二值化处理，只接受长度为 6 的英数字结果；可配置 1～5 次识别尝试。

不使用 OCR 时，可以将 OCR 服务地址留空。

### Gotify 通知

- 支持配置 Gotify 服务地址和应用 Token。
- 可在设置页发送测试通知。
- 每轮保活结束后发送结果通知。
- 开启自动登录时，通知会汇总登录成功站点和失败原因。

### 其他功能

- 总览页显示 Chrome 状态、站点数量、下一轮任务时间、执行状态和最近日志。
- 日志页支持实时刷新、关键词搜索、点击复制和清空。
- 日志保留条数可配置为 50～5000 条。
- 支持深色、浅色主题。
- 支持开机自启。
- 关闭主窗口时应用会隐藏到系统托盘，定时任务仍会继续运行。
- 托盘菜单可打开主界面、立即执行保活或彻底退出应用。
- 支持检查、下载并安装 GitHub Release 更新；网络受限时可设置 URL 前缀型更新代理。
- 支持导入、导出完整配置。

## 快速开始

### 1. 安装并启动

从项目的 [Releases](https://github.com/Edwinzzzs2/pt-manager-tauri/releases) 页面下载 Windows 安装包并安装。首次运行前请确保系统已安装 Google Chrome；如果未检测到 Chrome，也可以在应用总览页打开官方下载页面。

### 2. 添加站点

进入“站点”页面，填写站点名称和完整 URL，例如：

```text
名称：示例站
URL：https://pt.example.com/
```

新增后可以编辑站点，继续配置：

- 登录用户名和密码；
- 可选的 TOTP Base32 密钥；
- 是否需要自动登录；
- 是否参加自动保活。

只需要保持已有登录态时，无需开启自动登录。

### 3. 首次准备登录状态

点击总览页的“预先准备”或顶部“立即保活”，应用会启动专用 Chrome。首次使用建议在这个 Chrome 窗口中人工完成登录、验证码或安全验证。

专用 Chrome 与日常 Chrome 使用不同的 Profile，因此日常浏览器中已经登录并不代表专用 Chrome 也已登录。需要复用日常浏览器的登录数据时，可以使用 CookieCloud 同步。

### 4. 设置定时任务

默认 Cron 为 `0 9 * * *`，表示每天 09:00 触发。应用支持 5 段或 6 段表达式：

```text
# 5 段：分 时 日 月 周
0 9 * * *

# 6 段：秒 分 时 日 月 周
0 30 8 * * *
```

各字段支持：

- `*`：全部取值；
- `,`：多个取值，例如 `1,3,5`；
- `-`：范围，例如 `1-5`；
- `/`：步长，例如 `*/15`。

星期字段使用 `0` 或 `7` 表示星期日，`1`～`6` 表示星期一到星期六。保存设置后调度器会立即按新配置重新计算下一次执行时间。

## 站点 JSON 导入

“站点 -> 导入 JSON”只追加站点，不覆盖任务设置。文件可以直接使用数组，也可以使用带 `sites` 字段的对象。

```json
[
  {
    "name": "示例站",
    "url": "https://pt.example.com/",
    "username": "user",
    "password": "password",
    "totp_secret": "BASE32SECRET",
    "auto_login": true
  }
]
```

等价的对象格式：

```json
{
  "sites": [
    {
      "name": "示例站",
      "url": "https://pt.example.com/"
    }
  ]
}
```

导入规则：

- `name` 和 `url` 必填，URL 必须以 `http://` 或 `https://` 开头。
- `username`、`password`、`totp_secret`、`auto_login` 可选。
- 与现有站点 URL 重复、名称为空或 URL 无效的记录会被跳过。
- 导入的新站点默认开启自动保活。

## 配置项说明

| 配置项 | 默认值 | 说明 |
| --- | --- | --- |
| Cron 表达式 | `0 9 * * *` | 定时任务触发时间 |
| 随机延迟上限 | 30 分钟 | 仅定时任务使用；`0` 表示准点执行 |
| 页面停留时间 | 30 秒 | 手动批量保活的统一停留时长，也是定时任务的基础停留时长；最少 5 秒 |
| 日志保留条数 | 500 | 可设置 50～5000 条 |
| 开机自启 | 关闭 | 登录系统后自动启动应用 |
| Chrome 调试端口 | 9222 | 优先使用的 CDP 端口，通常无需修改 |
| OCR 服务地址 | `http://192.168.31.80:8060` | 不使用 OCR 时请清空 |
| OCR 尝试次数 | 2 | 可设置 1～5 次 |
| 最低剩余登录次数 | 5 | 达到该阈值时停止自动登录重试 |
| 更新代理地址 | 空 | 留空直连 GitHub；填写后同时代理更新清单和安装包 |
| 更新代理密码 | 空 | 代理启用鉴权时填写，与服务端 `PROXY_AUTH_TOKEN` 保持一致 |

## 数据存储与安全

应用会在本机保存三类数据：

| 数据 | 位置 | 内容 |
| --- | --- | --- |
| 应用配置 | Tauri 应用数据目录下的 `config.json` | 站点、任务设置和第三方服务配置 |
| 运行日志 | `%LOCALAPPDATA%\pt-manager\run.log` | 保活、登录、同步和错误记录 |
| 专用 Chrome Profile | `%LOCALAPPDATA%\pt-manager\chrome-cdp-profile-auto` | Cookie、Local Storage、缓存等浏览器数据 |

> [!WARNING]
> 站点密码、TOTP 密钥、CookieCloud 密码和 Gotify Token 会保存在本地配置中。导出的完整配置 JSON 也包含这些敏感信息，请勿提交到 Git、上传到公开网盘或直接分享给他人。

清除浏览器数据只清理专用 Chrome 的浏览数据，不会删除站点列表和应用设置。导入全部配置则会完全覆盖当前站点及设置，操作前建议先导出备份。

## 界面说明

| 页面 | 功能 |
| --- | --- |
| 总览 | 查看运行状态、准备 Chrome、检查最近结果和实时日志 |
| 站点 | 新增、导入、编辑、批量管理、测试登录和识别验证码 |
| 日志 | 查看、搜索、复制、刷新和清空运行日志 |
| 设置 | 配置更新、任务、CookieCloud、OCR、Gotify、备份和浏览器数据 |

## 本地开发

### 环境要求

- Windows 10/11；
- Node.js 22；
- Rust stable 与 Cargo；
- Visual Studio 2022 Build Tools，并安装 C++ 桌面开发工具链；
- Tauri 2 在 Windows 上所需的 WebView2 运行环境；
- Google Chrome。

### 安装依赖

```powershell
npm install
```

### 启动桌面开发环境

```powershell
npm run desktop
```

只调试前端页面时可运行：

```powershell
npm run dev
```

### 构建 Windows 安装包

```powershell
npm run tauri:build
```

该命令会先查找 Visual Studio Build Tools、初始化 x64 编译环境，再执行 Tauri 构建。安装包目标格式为 NSIS。

## 项目结构

```text
pt-manager-tauri/
├─ src/                       React 前端界面
│  ├─ App.tsx                 页面、交互和 Tauri 命令调用
│  └─ App.css                 应用样式
├─ src-tauri/
│  ├─ src/
│  │  ├─ cdp.rs               Chrome 启动、CDP 控制和站点登录适配
│  │  ├─ scheduler.rs         Cron 调度与保活任务流程
│  │  ├─ commands.rs          前端可调用的 Tauri 命令
│  │  ├─ cookiecloud.rs       CookieCloud 拉取、解密、匹配和上传
│  │  ├─ ocr.rs               OCR 初始化、图片预处理和识别
│  │  ├─ gotify.rs            Gotify 通知
│  │  ├─ auth.rs              TOTP 生成
│  │  ├─ updater.rs           应用更新与代理地址处理
│  │  └─ store.rs             配置和日志持久化
│  ├─ tauri.conf.json         Tauri 窗口、打包和更新配置
│  └─ docs/自动签名.md         更新签名与发布说明
├─ scripts/tauri-build.cmd    Windows 构建环境初始化脚本
└─ .github/workflows/         GitHub Release 自动发布流程
```

## 发布

推送 `v*` 格式的 Git Tag 会触发 GitHub Actions：同步项目版本号、构建 Windows NSIS 安装包、生成更新签名和 `latest.json`，最后创建 GitHub Release。

自动更新签名的密钥配置和常见问题见 [Tauri 自动签名与 GitHub Actions 发布手册](src-tauri/docs/自动签名.md)。
