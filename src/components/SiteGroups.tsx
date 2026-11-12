import { useEffect, useRef, useState, type FormEvent } from "react";
import { Eye, EyeOff, XCircle } from "lucide-react";
import { CronField, CronPreviewText, useCronPreview } from "./CronField";

type ProxyEndpoint = {
  server_url: string;
  username: string;
  password: string;
};

export type BrowserProxyConfig = ProxyEndpoint & {
  enabled: boolean;
  first_hop?: ProxyEndpoint | null;
};

function splitSocks5ProxyLink(value: string) {
  const input = value.trim();
  if (!/^socks5h?:\/\//i.test(input)) return null;

  let url: URL;
  try {
    url = new URL(input);
  } catch {
    throw new Error("SOCKS5 链接格式不正确，请检查代理地址和端口");
  }
  if (!url.hostname || !url.port || url.port === "0" || !["", "/"].includes(url.pathname) || url.search) {
    throw new Error("SOCKS5 链接需要填写有效的地址和端口，地址不能带路径或查询参数");
  }
  const server_url = `${url.protocol}//${url.host}`;
  if (!url.username && !url.password) return { server_url };
  try {
    return {
      server_url,
      username: decodeURIComponent(url.username),
      password: decodeURIComponent(url.password),
    };
  } catch {
    throw new Error("SOCKS5 链接里的账号或密码编码无效，请分开填写");
  }
}

export function normalizeBrowserProxyLinks(proxy: BrowserProxyConfig): BrowserProxyConfig {
  if (!proxy.enabled) return proxy;
  const first = proxy.first_hop;
  return {
    ...proxy,
    ...splitSocks5ProxyLink(proxy.server_url),
    first_hop: first ? { ...first, ...splitSocks5ProxyLink(first.server_url) } : null,
  };
}

export function BrowserProxyFields({
  proxy, onChange, onError, disabled, required,
}: {
  proxy: BrowserProxyConfig;
  onChange: (proxy: BrowserProxyConfig) => void;
  onError: (message: string) => void;
  disabled?: boolean;
  required?: boolean;
}) {
  const first = proxy.first_hop;
  const mode = !proxy.enabled ? "direct" : first ? "chain" : "single";
  return (
    <>
      <label>
        <span>网络出口</span>
        <select disabled={disabled} value={mode} onChange={(event) => {
          const value = event.target.value;
          onChange({
            ...proxy,
            enabled: value !== "direct",
            first_hop: value === "chain" ? first ?? { server_url: "", username: "", password: "" }
              : value === "single" ? null : first,
          });
        }}>
          <option value="direct">直连</option>
          <option value="single">单代理</option>
          <option value="chain">两跳 SOCKS5</option>
        </select>
      </label>
      {proxy.enabled && first && (
        <ProxyEndpointFields endpoint={first} label="① 前置代理" socksOnly disabled={disabled} required={required}
          onChange={(first_hop) => onChange({ ...proxy, first_hop })} onError={onError} />
      )}
      {proxy.enabled && (
        <ProxyEndpointFields endpoint={proxy} label={first ? "② 最终出口" : "代理"} socksOnly={!!first}
          disabled={disabled} required={required} onChange={(endpoint) => onChange({ ...proxy, ...endpoint })}
          onError={onError} />
      )}
    </>
  );
}

function ProxyEndpointFields({ endpoint, label, socksOnly, onChange, onError, disabled, required }: {
  endpoint: ProxyEndpoint;
  label: string;
  socksOnly: boolean;
  onChange: (endpoint: ProxyEndpoint) => void;
  onError: (message: string) => void;
  disabled?: boolean;
  required?: boolean;
}) {
  const [showPassword, setShowPassword] = useState(false);

  function fillProxyLink(value: string) {
    try {
      const fields = splitSocks5ProxyLink(value);
      if (!fields) return false;
      if ("username" in fields) setShowPassword(false);
      if (fields.server_url !== endpoint.server_url || "username" in fields) {
        onChange({ ...endpoint, ...fields });
      }
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
    return true;
  }

  return (
    <>
      <label className="proxy-address-field">
        <span>{label}地址</span>
        <input
          autoComplete="off"
          disabled={disabled}
          inputMode="url"
          onChange={(event) => onChange({ ...endpoint, server_url: event.target.value })}
          onBlur={(event) => fillProxyLink(event.currentTarget.value)}
          onPaste={(event) => {
            if (fillProxyLink(event.clipboardData.getData("text"))) {
              event.preventDefault();
            }
          }}
          placeholder={socksOnly ? "socks5://proxy.example.com:1080" : "http://proxy.example.com:3128"}
          required={required}
          value={endpoint.server_url}
        />
        <small>粘贴完整 SOCKS5 链接，或输入后离开地址框，即可自动拆分地址、用户名和密码。</small>
      </label>
      <label>
        <span>{label}用户名 <small>选填</small></span>
        <input autoComplete="off" disabled={disabled} value={endpoint.username}
          onChange={(event) => onChange({ ...endpoint, username: event.target.value })} />
      </label>
      <label>
        <span>{label}密码 <small>选填</small></span>
        <div className="password-field">
          <input autoComplete="new-password" disabled={disabled} type={showPassword ? "text" : "password"}
            value={endpoint.password} onChange={(event) => onChange({ ...endpoint, password: event.target.value })} />
          <button type="button" aria-label={`${showPassword ? "隐藏" : "显示"}${label}密码`}
            onClick={() => setShowPassword((value) => !value)}>
            {showPassword ? <EyeOff size={16} /> : <Eye size={16} />}
          </button>
        </div>
      </label>
    </>
  );
}

export type CookieCloudConfig = {
  server_url: string;
  uuid: string;
  password: string;
};

export type BrowserEnvironment = {
  timezone: string;
  language: string;
};

export function normalizeBrowserEnvironment(settings: BrowserEnvironment): BrowserEnvironment {
  const timezone = settings.timezone.trim();
  const language = settings.language.trim();
  try {
    if (timezone) new Intl.DateTimeFormat(undefined, { timeZone: timezone });
  } catch {
    throw new Error("浏览器时区无效，请填写 America/New_York 这类时区名称");
  }
  try {
    return { timezone, language: language ? Intl.getCanonicalLocales(language)[0] : "" };
  } catch {
    throw new Error("浏览器语言无效，请填写 en-US 这类语言代码");
  }
}

export function BrowserEnvironmentFields({ settings, onChange, disabled, hintClassName }: {
  settings: BrowserEnvironment;
  onChange: (settings: BrowserEnvironment) => void;
  disabled?: boolean;
  hintClassName: string;
}) {
  return (
    <>
      <label>
        <span>浏览器时区</span>
        <input autoComplete="off" disabled={disabled} spellCheck={false} placeholder="例如 America/New_York"
          value={settings.timezone} onChange={(event) => onChange({ ...settings, timezone: event.target.value })} />
      </label>
      <label>
        <span>浏览器语言</span>
        <input autoComplete="off" disabled={disabled} spellCheck={false} placeholder="例如 en-US"
          value={settings.language} onChange={(event) => onChange({ ...settings, language: event.target.value })} />
      </label>
      <p className={hintClassName}>
        应用于本组专用浏览器，包括手动打开的标签页和窗口。留空时使用浏览器默认环境。
        PT Manager 运行期间持续应用；保活计划按本机时区执行。
      </p>
    </>
  );
}

export type SiteGroup = {
  id: string;
  name: string;
  cdp_port: number;
  browser_proxy: BrowserProxyConfig;
  browser_environment: BrowserEnvironment;
  cookie_mode: "manual" | "inherit" | "custom";
  cookiecloud: CookieCloudConfig;
  cookie_auto_import: boolean;
  cookie_auto_upload: boolean;
  scheduled_keepalive: boolean;
  task?: GroupTaskConfig | null;
};

export type GroupTaskConfig = {
  cron: string;
  cron_offset_minutes: number;
  visit_duration: number;
};

export const cookieModeLabels: Record<SiteGroup["cookie_mode"], string> = {
  manual: "关闭同步",
  inherit: "沿用默认设置",
  custom: "自定义同步",
};

const emptyCookieCloud: CookieCloudConfig = { server_url: "", uuid: "", password: "" };

export function SiteGroupOptions({ groups }: { groups: SiteGroup[] }) {
  return (
    <>
      <option value="">默认分组</option>
      {groups.map((group) => <option key={group.id} value={group.id}>{group.name}</option>)}
    </>
  );
}

export function SiteGroupTabs({ groups, value, defaultLabel, label, onChange }: {
  groups: SiteGroup[];
  value: string;
  defaultLabel: string;
  label: string;
  onChange: (id: string) => void;
}) {
  const tabs = [{ id: "", name: defaultLabel }, ...groups];
  return (
    <div className="group-tabs" role="group" aria-label={label}>
      {tabs.map(({ id, name }) => (
        <button
          key={id}
          aria-pressed={value === id}
          onClick={() => { if (id !== value) onChange(id); }}
          title={name}
          type="button"
        >
          {name}
        </button>
      ))}
    </div>
  );
}

export function SiteGroupDialog({
  groups,
  initialId,
  startupGroupId,
  defaultPort,
  defaultTask,
  defaultScheduleEnabled,
  countSites,
  onSave,
  onRemove,
  onClose,
}: {
  groups: SiteGroup[];
  initialId: string;
  startupGroupId: string | null;
  defaultPort: number;
  defaultTask: GroupTaskConfig;
  defaultScheduleEnabled: boolean;
  countSites: (id: string) => number;
  onSave: (group: SiteGroup, showOnStartup: boolean) => Promise<void>;
  onRemove: (id: string) => Promise<void>;
  onClose: () => void;
}) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const [draft, setDraft] = useState(() => groupDraft(initialId));
  const [showOnStartup, setShowOnStartup] = useState(initialId === startupGroupId);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  function groupDraft(id: string): SiteGroup {
    const existing = groups.find((group) => group.id === id);
    if (existing) return existing;
    const used = new Set([defaultPort, ...groups.map((group) => group.cdp_port)]);
    let port = 9222;
    while (used.has(port)) port += 1;
    return {
      id: "",
      name: "",
      cdp_port: port,
      browser_proxy: { enabled: false, server_url: "", username: "", password: "" },
      browser_environment: { timezone: "", language: "" },
      cookie_mode: "manual",
      cookiecloud: emptyCookieCloud,
      cookie_auto_import: false,
      cookie_auto_upload: false,
      scheduled_keepalive: true,
      task: null,
    };
  }

  useEffect(() => {
    const dialog = dialogRef.current;
    if (dialog && !dialog.open) dialog.showModal();
  }, []);

  function close() {
    dialogRef.current?.close();
    onClose();
  }

  async function apply(action: () => Promise<void>) {
    if (saving) return;
    setSaving(true);
    setError(null);
    try {
      await action();
      close();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setSaving(false);
    }
  }

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const group = draft.cookie_mode === "custom"
      ? draft
      : { ...draft, cookiecloud: emptyCookieCloud, cookie_auto_import: false, cookie_auto_upload: false };
    void apply(() => {
      const browser_environment = normalizeBrowserEnvironment(group.browser_environment);
      const browser_proxy = normalizeBrowserProxyLinks(group.browser_proxy);
      setDraft((current) => ({ ...current, browser_proxy, browser_environment }));
      return onSave({ ...group, browser_proxy, browser_environment }, showOnStartup);
    });
  }

  function cookieField(key: keyof CookieCloudConfig, value: string) {
    setDraft((current) => ({ ...current, cookiecloud: { ...current.cookiecloud, [key]: value } }));
  }

  const siteCount = draft.id ? countSites(draft.id) : 0;
  const task = draft.task;
  const cronPreview = useCronPreview(task?.cron ?? defaultTask.cron);

  return (
    <dialog
      ref={dialogRef}
      aria-labelledby="site-group-title"
      className="app-dialog"
      onCancel={(event) => {
        event.preventDefault();
        if (!saving) close();
      }}
    >
      <form onSubmit={submit}>
        <header className="app-dialog-heading">
          <h2 id="site-group-title">{draft.id ? "编辑分组" : "新增分组"}</h2>
          <button aria-label="关闭站点分组" className="icon-button" disabled={saving} onClick={close} type="button">
            <XCircle size={18} aria-hidden="true" />
          </button>
        </header>
        <div className="site-add-body">
          <fieldset className="site-edit-fields" disabled={saving}>
            <label className="site-secret-field">
              <span>新建或选择要编辑的分组</span>
              <select value={draft.id} onChange={(event) => {
                const id = event.target.value;
                setDraft(groupDraft(id));
                setShowOnStartup(id === startupGroupId);
                setError(null);
              }}>
                <option value="">新增分组</option>
                {groups.map((group) => (
                  <option key={group.id} value={group.id}>
                    {group.name}{group.id === startupGroupId ? "（启动时显示）" : ""}
                  </option>
                ))}
              </select>
            </label>
            <label>
              <span>分组名称</span>
              <input required value={draft.name} placeholder="例如：代理账号" onChange={(event) => setDraft({ ...draft, name: event.target.value })} />
            </label>
            <label>
              <span>浏览器调试端口</span>
              <input required type="number" min={1} max={65535} value={draft.cdp_port || ""} onChange={(event) => setDraft({ ...draft, cdp_port: Number(event.target.value) })} />
            </label>
            <label className="switch-row site-secret-field">
              <span>启动时显示此分组</span>
              <input type="checkbox" checked={showOnStartup} onChange={(event) => setShowOnStartup(event.target.checked)} />
            </label>
            <label className="site-secret-field">
              <span>保活计划</span>
              <select value={draft.scheduled_keepalive ? task ? "custom" : "inherit" : "off"} onChange={(event) => {
                const mode = event.target.value;
                if (mode === "off") {
                  setDraft({ ...draft, scheduled_keepalive: false });
                  return;
                }
                const savedTask = groups.find((group) => group.id === draft.id)?.task;
                const task = mode === "custom" ? { ...(draft.task ?? savedTask ?? defaultTask) } : null;
                setDraft({ ...draft, scheduled_keepalive: true, task });
              }}>
                <option value="inherit">沿用默认设置的计划</option>
                <option value="custom">单独设置计划</option>
                <option value="off">关闭定时保活</option>
              </select>
            </label>
            {!draft.scheduled_keepalive ? (
              <p className="site-group-hint site-secret-field">本组不会按计划自动执行，仍可手动保活。</p>
            ) : task ? (
              <>
                <CronField
                  className="site-secret-field"
                  value={task.cron}
                  preview={cronPreview}
                  onChange={(cron) => setDraft({ ...draft, task: { ...task, cron } })}
                />
                <label>
                  <span>随机延迟上限（分钟）</span>
                  <input required type="number" min={0} max={1440} value={task.cron_offset_minutes} onChange={(event) =>
                    setDraft({ ...draft, task: { ...task, cron_offset_minutes: Number(event.target.value) } })
                  } />
                </label>
                <label>
                  <span>保活停留时间（秒）</span>
                  <input required type="number" min={5} value={task.visit_duration} onChange={(event) =>
                    setDraft({ ...draft, task: { ...task, visit_duration: Number(event.target.value) } })
                  } />
                </label>
              </>
            ) : !defaultScheduleEnabled ? (
              <p className="site-group-hint site-secret-field">默认定时保活已关闭。可单独为本组设置计划。</p>
            ) : (
              <p className="site-group-hint site-secret-field">
                <CronPreviewText preview={cronPreview} /> · 随机延迟最多 {defaultTask.cron_offset_minutes} 分钟 · 停留 {defaultTask.visit_duration} 秒
              </p>
            )}
            <BrowserProxyFields required disabled={saving} proxy={draft.browser_proxy}
              onChange={(browser_proxy) => {
                setDraft((current) => ({ ...current, browser_proxy }));
                setError(null);
              }}
              onError={setError} />
            <BrowserEnvironmentFields settings={draft.browser_environment}
              hintClassName="site-group-hint site-secret-field"
              onChange={(browser_environment) => {
                setDraft((current) => ({ ...current, browser_environment }));
                setError(null);
              }} />
            <label className="site-secret-field">
              <span>CookieCloud</span>
              <select value={draft.cookie_mode} onChange={(event) => setDraft({ ...draft, cookie_mode: event.target.value as SiteGroup["cookie_mode"] })}>
                <option value="manual">{cookieModeLabels.manual}</option>
                <option value="inherit">{cookieModeLabels.inherit}</option>
                <option value="custom">{cookieModeLabels.custom}</option>
              </select>
            </label>
            {draft.cookie_mode === "manual" && (
              <p className="site-group-hint site-secret-field">本组停用 CookieCloud 导入和上传。站点自动登录仍由各站点的开关控制。</p>
            )}
            {draft.cookie_mode === "inherit" && (
              <p className="site-group-hint site-secret-field">使用「默认设置」里的 CookieCloud 连接和自动同步开关。</p>
            )}
            {draft.cookie_mode === "custom" && (
              <>
                <label className="site-secret-field">
                  <span>CookieCloud 服务地址</span>
                  <input required type="url" placeholder="https://cookiecloud.example.com" value={draft.cookiecloud.server_url} onChange={(event) => cookieField("server_url", event.target.value)} />
                </label>
                <label>
                  <span>独立 UUID</span>
                  <input required autoComplete="off" value={draft.cookiecloud.uuid} onChange={(event) => cookieField("uuid", event.target.value)} />
                </label>
                <label>
                  <span>CookieCloud 密码</span>
                  <input required type="password" autoComplete="new-password" value={draft.cookiecloud.password} onChange={(event) => cookieField("password", event.target.value)} />
                </label>
                <label className="switch-row">
                  <span>保活前自动导入</span>
                  <input
                    type="checkbox"
                    checked={draft.cookie_auto_import}
                    onChange={(event) => setDraft({ ...draft, cookie_auto_import: event.target.checked })}
                  />
                </label>
                <label className="switch-row">
                  <span>保活后自动上传</span>
                  <input
                    type="checkbox"
                    checked={draft.cookie_auto_upload}
                    onChange={(event) => setDraft({ ...draft, cookie_auto_upload: event.target.checked })}
                  />
                </label>
              </>
            )}
            <p className="site-group-hint site-secret-field">
              每组使用独立的浏览器数据。同一站点的不同账号请放到不同组。默认分组使用「默认设置」里的浏览器配置。
            </p>
          </fieldset>
        </div>
        {error && <p className="app-dialog-error site-group-error" role="alert">{error}</p>}
        <footer className="app-dialog-footer site-group-footer">
          {draft.id && (
            <button
              className="danger-action"
              disabled={saving || siteCount > 0}
              onClick={() => void apply(() => onRemove(draft.id))}
              title={siteCount > 0 ? `请先移走本组的 ${siteCount} 个站点` : "删除空分组"}
              type="button"
            >
              删除分组
            </button>
          )}
          <button className="ghost" disabled={saving} onClick={close} type="button">取消</button>
          <button className="primary-action" disabled={saving || (draft.scheduled_keepalive && !!task && cronPreview.state !== "valid")} type="submit">{saving ? "保存中…" : "保存分组"}</button>
        </footer>
      </form>
    </dialog>
  );
}
