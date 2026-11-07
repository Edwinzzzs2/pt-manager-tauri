import { useEffect, useRef, useState, type FormEvent } from "react";
import { XCircle } from "lucide-react";

export type BrowserProxyConfig = {
  enabled: boolean;
  server_url: string;
  username: string;
  password: string;
};

export type CookieCloudConfig = {
  server_url: string;
  uuid: string;
  password: string;
};

export type SiteGroup = {
  id: string;
  name: string;
  cdp_port: number;
  browser_proxy: BrowserProxyConfig;
  cookie_mode: "manual" | "inherit" | "custom";
  cookiecloud: CookieCloudConfig;
  cookie_auto_import: boolean;
  cookie_auto_upload: boolean;
};

const emptyCookieCloud: CookieCloudConfig = { server_url: "", uuid: "", password: "" };

export function SiteGroupOptions({
  groups,
  includeUngrouped = true,
}: {
  groups: SiteGroup[];
  includeUngrouped?: boolean;
}) {
  return (
    <>
      {includeUngrouped && <option value="">未分组</option>}
      {groups.map((group) => <option key={group.id} value={group.id}>{group.name}</option>)}
    </>
  );
}

export function SiteGroupDialog({
  groups,
  initialId,
  startupGroupId,
  defaultPort,
  countSites,
  onSave,
  onRemove,
  onClose,
}: {
  groups: SiteGroup[];
  initialId: string;
  startupGroupId: string | null;
  defaultPort: number;
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
      cookie_mode: "manual",
      cookiecloud: emptyCookieCloud,
      cookie_auto_import: false,
      cookie_auto_upload: false,
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
    void apply(() => onSave(group, showOnStartup));
  }

  function proxyField(key: "server_url" | "username" | "password", value: string) {
    setDraft((current) => ({ ...current, browser_proxy: { ...current.browser_proxy, [key]: value } }));
  }

  function cookieField(key: keyof CookieCloudConfig, value: string) {
    setDraft((current) => ({ ...current, cookiecloud: { ...current.cookiecloud, [key]: value } }));
  }

  const siteCount = draft.id ? countSites(draft.id) : 0;

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
          <h2 id="site-group-title">{draft.id ? "编辑分组" : "新建分组"}</h2>
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
                <option value="">新建分组</option>
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
              <span>网络出口</span>
              <select value={draft.browser_proxy.enabled ? "proxy" : "direct"} onChange={(event) => setDraft({
                ...draft, browser_proxy: { ...draft.browser_proxy, enabled: event.target.value === "proxy" },
              })}>
                <option value="direct">直连</option>
                <option value="proxy">指定代理</option>
              </select>
            </label>
            {draft.browser_proxy.enabled && (
              <>
                <label className="site-secret-field">
                  <span>代理地址</span>
                  <input required type="url" placeholder="http://proxy.example.com:8080" value={draft.browser_proxy.server_url} onChange={(event) => proxyField("server_url", event.target.value)} />
                </label>
                <label>
                  <span>代理用户名 <small>选填</small></span>
                  <input autoComplete="off" value={draft.browser_proxy.username} onChange={(event) => proxyField("username", event.target.value)} />
                </label>
                <label>
                  <span>代理密码 <small>选填</small></span>
                  <input type="password" autoComplete="new-password" value={draft.browser_proxy.password} onChange={(event) => proxyField("password", event.target.value)} />
                </label>
              </>
            )}
            <label className="site-secret-field">
              <span>CookieCloud</span>
              <select value={draft.cookie_mode} onChange={(event) => setDraft({ ...draft, cookie_mode: event.target.value as SiteGroup["cookie_mode"] })}>
                <option value="manual">手动</option>
                <option value="inherit">默认（跟随设置）</option>
                <option value="custom">自定义</option>
              </select>
            </label>
            {draft.cookie_mode === "manual" && (
              <p className="site-group-hint site-secret-field">登录状态保存在本组浏览器中，不参与 CookieCloud 同步。</p>
            )}
            {draft.cookie_mode === "inherit" && (
              <p className="site-group-hint site-secret-field">使用设置页的 CookieCloud 连接和自动同步开关。</p>
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
              每组使用独立的浏览器数据。同一站点的不同账号请放到不同组。未分组站点沿用设置页的配置。
            </p>
          </fieldset>
          {error && <p className="app-dialog-error" role="alert">{error}</p>}
        </div>
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
          <button className="primary-action" disabled={saving} type="submit">{saving ? "保存中…" : "保存分组"}</button>
        </footer>
      </form>
    </dialog>
  );
}
