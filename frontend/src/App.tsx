import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  DIM_FILTER_KEY,
  EntryDetail,
  EntrySummary,
  Facets,
  FilterOption,
  Filters,
  GROUP_DIMS,
  GroupDim,
  KIND_OPTIONS,
  METHOD_OPTIONS,
  RESOURCE_OPTIONS,
  STATUS_OPTIONS,
  SysProxyStatus,
  UpstreamStatus,
  RawView,
  b64ToBytes,
  clearEntries,
  detectMediaKind,
  disableUpstream,
  downloadBytes,
  emptyFacets,
  emptyFilters,
  fetchDetail,
  fetchEntries,
  fetchEntryBody,
  fetchFacets,
  fetchInfo,
  fetchSysProxy,
  fetchBypass,
  clearBypass,
  BypassItem,
  fetchRules,
  saveRules,
  RulesState,
  formatMs,
  fetchUpstream,
  fetchVideos,
  formatDuration,
  VideoItem,
  videoDownloadUrl,
  filtersToQuery,
  formatBytes,
  formatTime,
  groupFilterable,
  groupKeyOf,
  groupKeyToFilter,
  groupLabelOf,
  hasAnyFilter,
  looksBinary,
  optionLabel,
  rangeStartOf,
  resourceMeta,
  scanUpstream,
  setSysProxy,
  quitApp,
  setUpstream,
  statusColor,
  stitchEntryBody,
  suggestFilename,
  toHexDump,
  tryPrettyJson,
  upstreamSourceLabel,
  wrapBase64,
} from './api';
import { QRCodeSVG } from 'qrcode.react';
import { applyTheme, getStoredTheme, watchSystemTheme, Theme } from './theme';
import {
  IconAlertTriangle,
  IconArchive,
  IconBraces,
  IconCheck,
  IconChevronDown,
  IconChevronRight,
  IconChevronUp,
  IconClapperboard,
  IconCopy,
  IconDownload,
  IconGlobe,
  IconKey,
  IconLink,
  IconMenu,
  IconMonitor,
  IconMoon,
  IconPause,
  IconPlay,
  IconPower,
  IconRadar,
  IconRefresh,
  IconRoute,
  IconSearch,
  IconShield,
  IconSliders,
  IconSmartphone,
  IconSpinner,
  IconSun,
  IconTrash,
  IconWifi,
  IconX,
} from './icons';

/* ---------------- 主题切换 ---------------- */
function ThemeToggle() {
  const [theme, setTheme] = useState<Theme>(getStoredTheme());
  useEffect(() => watchSystemTheme(() => applyTheme(getStoredTheme())), []);
  const pick = (t: Theme) => {
    setTheme(t);
    applyTheme(t);
  };
  return (
    <div className="theme-toggle" role="radiogroup" aria-label="主题选择">
      <button
        className={theme === 'light' ? 'active' : ''}
        onClick={() => pick('light')}
        title="浅色"
        aria-label="浅色"
      >
        <IconSun size={14} />
      </button>
      <button
        className={theme === 'dark' ? 'active' : ''}
        onClick={() => pick('dark')}
        title="深色"
        aria-label="深色"
      >
        <IconMoon size={14} />
      </button>
      <button
        className={theme === 'system' ? 'active' : ''}
        onClick={() => pick('system')}
        title="跟随系统"
        aria-label="跟随系统"
      >
        <IconMonitor size={14} />
      </button>
    </div>
  );
}

/* ---------------- 上游级联开关 ---------------- */
/* ---------------- 导出 / 证书 下拉 ---------------- */
function ExportMenu({ query }: { query: string }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);
  return (
    <div className="upstream-wrap" ref={ref}>
      <button
        className={`btn ghost${open ? ' soft' : ''}`}
        onClick={() => setOpen((o) => !o)}
        aria-haspopup="menu"
        aria-expanded={open}
        title="导出抓包内容 / 下载 CA 证书"
      >
        <IconDownload />
        <span className="btn-label">导出</span>
        <span className="ms-caret"><IconChevronDown size={10} /></span>
      </button>
      {open && (
        <div className="upstream-pop export-pop" role="menu">
          <a
            className="export-item"
            role="menuitem"
            href={`/api/export?format=json&${query}`}
            target="_blank"
            rel="noreferrer"
            onClick={() => setOpen(false)}
          >
            <IconBraces />
            导出 JSON
          </a>
          <a
            className="export-item"
            role="menuitem"
            href={`/api/export?format=har&${query}`}
            target="_blank"
            rel="noreferrer"
            onClick={() => setOpen(false)}
          >
            <IconArchive />
            导出 HAR（含正文）
          </a>
          <div className="export-sep" />
          <a
            className="export-item"
            role="menuitem"
            href="/api/ca.crt"
            download="miniproxy-ca.crt"
            title="安装到系统/浏览器以解密 HTTPS"
            onClick={() => setOpen(false)}
          >
            <IconKey />
            下载 CA 证书
          </a>
        </div>
      )}
    </div>
  );
}

function UpstreamControl() {
  const [st, setSt] = useState<UpstreamStatus | null>(null);
  const [open, setOpen] = useState(false);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const [candidates, setCandidates] = useState<string[]>([]);
  const [msg, setMsg] = useState<{ kind: 'ok' | 'err' | 'warn'; text: string } | null>(null);
  const wrapRef = useRef<HTMLDivElement>(null);

  const refresh = useCallback(() => {
    fetchUpstream().then(setSt).catch(() => {});
  }, []);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 5000);
    return () => clearInterval(t);
  }, [refresh]);

  // 点击浮层外部关闭
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);

  const scan = async () => {
    setBusy(true);
    setMsg(null);
    try {
      const r = await scanUpstream();
      setCandidates(r.candidates.map((c) => c.addr));
      setMsg(
        r.candidates.length
          ? { kind: 'ok', text: `检测到 ${r.candidates.length} 个可用代理，点击任一地址即可启用` }
          : { kind: 'warn', text: '未检测到本机 HTTP 代理。若已运行 Clash/Charles，请手动填写其端口' }
      );
    } finally {
      setBusy(false);
    }
  };

  const apply = async (addr: string) => {
    setBusy(true);
    setMsg(null);
    try {
      const r = await setUpstream(addr);
      if (!r.ok) {
        setMsg({ kind: 'err', text: r.error || '设置失败' });
      } else {
        setMsg({ kind: 'ok', text: r.warning || `已启用上游级联 ${r.addr}` });
        setInput('');
      }
      refresh();
    } finally {
      setBusy(false);
    }
  };

  const turnOff = async () => {
    setBusy(true);
    setMsg(null);
    try {
      await disableUpstream();
      setMsg({ kind: 'ok', text: '已关闭上游级联，出站恢复直连' });
      refresh();
    } finally {
      setBusy(false);
    }
  };

  const enabled = !!st?.enabled;
  return (
    <div className="upstream-wrap" ref={wrapRef}>
      <button
        className={`btn ghost${enabled ? ' soft' : ''}`}
        onClick={() => setOpen((o) => !o)}
        title={
          enabled
            ? `出站流量经上游代理 ${st?.addr} 转发（来源：${upstreamSourceLabel(st?.source)}）`
            : '未启用上游级联：被墙/海外站点的 TLS 握手会失败，点此一键检测本机代理并级联'
        }
      >
        {enabled ? (
          <>
            <IconLink />
            <span className="btn-label">上游</span>
            <span>{st?.addr}</span>
          </>
        ) : (
          <>
            <IconLink />
            <span className="btn-label">上游级联</span>
          </>
        )}
      </button>
      {open && (
        <div className="upstream-pop">
          <div className="upstream-pop-title">上游级联（出站代理）</div>
          <div className="upstream-pop-desc">
            开启后 MiniProxy 到源站的流量先经上游代理转发，HTTPS 仍会被解密抓包。
            适合本机开着 Clash/Charles 又想抓被墙站点的情况。
          </div>
          <div className="upstream-pop-row">
            <button className="btn" disabled={busy} onClick={scan}>
              {busy ? (
                <>
                  <IconSpinner />
                  检测中…
                </>
              ) : (
                <>
                  <IconSearch />
                  自动检测本机代理
                </>
              )}
            </button>
            {enabled && (
              <button className="btn danger" disabled={busy} onClick={turnOff}>
                关闭级联
              </button>
            )}
          </div>
          {candidates.length > 0 && (
            <div className="upstream-candidates">
              {candidates.map((c) => (
                <button key={c} className="chip-btn" disabled={busy} onClick={() => apply(c)}>
                  {c}
                </button>
              ))}
            </div>
          )}
          <div className="upstream-pop-row">
            <input
              className="upstream-input"
              placeholder="手动填写 host:port，如 127.0.0.1:7890"
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && input.trim()) apply(input.trim());
              }}
            />
            <button className="btn" disabled={busy || !input.trim()} onClick={() => apply(input.trim())}>
              启用
            </button>
          </div>
          {msg && <div className={`upstream-msg ${msg.kind}`}>{msg.text}</div>}
          {st?.source === 'env' && st.envAddr && (
            <div className="upstream-msg warn">
              当前由环境变量 MINIPROXY_UPSTREAM_PROXY={st.envAddr} 指定，优先级最高
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/* ---------------- 自动直通名单 ---------------- */
function BypassControl() {
  const [items, setItems] = useState<BypassItem[]>([]);
  const [threshold, setThreshold] = useState(3);
  const [outboundThreshold, setOutboundThreshold] = useState(2);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const wrapRef = useRef<HTMLDivElement>(null);

  const refresh = useCallback(() => {
    fetchBypass()
      .then((d) => {
        setItems(d.items);
        setThreshold(d.threshold);
        setOutboundThreshold(d.outboundThreshold ?? 2);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 5000);
    return () => clearInterval(t);
  }, [refresh]);

  // 点击浮层外部关闭
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);

  const clear = async () => {
    setBusy(true);
    try {
      const r = await clearBypass();
      setMsg(`已清空 ${r.cleared} 个域名的直通记录，后续连接将重新尝试解密`);
      refresh();
    } finally {
      setBusy(false);
    }
  };

  const active = items.filter((i) =>
    i.count >= (i.reason === 'outbound' ? outboundThreshold : threshold)
  );
  return (
    <div className="upstream-wrap" ref={wrapRef}>
      <button
        className={`btn ghost${active.length ? ' soft' : ''}`}
        onClick={() => setOpen((o) => !o)}
        title={
          active.length
            ? `${active.length} 个域名被自动直通（不再解密），点击查看/清空`
            : '没有域名被自动直通，全部正常解密'
        }
      >
        <IconShield />
        <span className="btn-label">自动直通</span>
        {active.length > 0 && <span>{active.length}</span>}
      </button>
      {open && (
        <div className="upstream-pop">
          <div className="upstream-pop-title">自动直通名单</div>
          <div className="upstream-pop-desc">
            两种原因会跳过解密、只记 TCP 隧道：① 客户端拒绝 MITM 证书（累计 {threshold} 次，
            通常是 App 做了证书固定）；② 源站只提供旧式加密套件、rustls 无法协商（累计{' '}
            {outboundThreshold} 次，这类站点只能以隧道方式访问）。清空后都会重新尝试解密。
          </div>
          {active.length === 0 ? (
            <div className="upstream-msg ok">（空）没有域名被自动直通</div>
          ) : (
            <div className="upstream-candidates">
              {active.map((i) => (
                <span
                  key={`${i.reason}-${i.host}`}
                  className="chip-btn"
                  style={{ cursor: 'default' }}
                  title={
                    i.reason === 'outbound'
                      ? `MiniProxy 到该站握手失败 ${i.count} 次（旧式加密套件，无法解密）`
                      : `客户端握手失败 ${i.count} 次（证书固定/拒绝 MITM 证书）`
                  }
                >
                  {i.host} ×{i.count}
                  {i.reason === 'outbound' ? ' · 旧式套件' : ''}
                </span>
              ))}
            </div>
          )}
          {active.length > 0 && (
            <div className="upstream-pop-row">
              <button className="btn danger" disabled={busy} onClick={clear}>
                {busy ? '清空中…' : '清空名单'}
              </button>
            </div>
          )}
          {msg && <div className="upstream-msg ok">{msg}</div>}
        </div>
      )}
    </div>
  );
}

/* ---------------- 退出（打包成 App 后没有终端可 Ctrl+C） ---------------- */
function QuitControl() {
  const [busy, setBusy] = useState(false);

  const quit = async () => {
    if (busy) return;
    if (!window.confirm('退出 MiniProxy？\n开启中的系统代理会自动恢复为之前的设置。')) return;
    setBusy(true);
    try {
      await quitApp();
    } catch {
      // 进程退出时连接会被掐断，这里的报错不用理会
    }
    setBusy(false);
  };

  return (
    <button
      className="btn ghost danger"
      disabled={busy}
      onClick={quit}
      title="结束 MiniProxy 进程（自动恢复系统代理）"
    >
      {busy ? <IconSpinner /> : <IconPower />}
      <span className="btn-label">{busy ? '退出中…' : '退出'}</span>
    </button>
  );
}

/* ---------------- 分流规则（跳过代理 / 走代理） ---------------- */
function RulesControl() {
  const [st, setSt] = useState<RulesState | null>(null);
  const [open, setOpen] = useState(false);
  const [directText, setDirectText] = useState('');
  const [proxiedText, setProxiedText] = useState('');
  const [noMitm, setNoMitm] = useState(true);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ kind: 'ok' | 'err' | 'warn'; text: string } | null>(null);
  const wrapRef = useRef<HTMLDivElement>(null);

  const apply = useCallback((d: RulesState) => {
    setSt(d);
    setDirectText(d.direct.join('\n'));
    setProxiedText(d.proxied.join('\n'));
    setNoMitm(d.directNoMitm);
  }, []);

  useEffect(() => {
    fetchRules().then(apply).catch(() => {});
  }, [apply]);

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);

  const parseLines = (s: string) =>
    s
      .split('\n')
      .map((l) => l.trim())
      .filter(Boolean);

  const save = async () => {
    setBusy(true);
    setMsg(null);
    try {
      const r = await saveRules({
        direct: parseLines(directText),
        proxied: parseLines(proxiedText),
        directNoMitm: noMitm,
      });
      if (!r.ok) {
        setMsg({ kind: 'err', text: r.error || '保存失败' });
      } else if (r.warning) {
        setMsg({ kind: 'warn', text: r.warning });
      } else {
        setMsg({
          kind: 'ok',
          text: r.systemProxyOn
            ? `已保存并已同步到系统代理 bypass（${r.directCount ?? 0} 条），对新连接立即生效`
            : `已保存，对新连接立即生效（系统代理未开启，未写入系统 bypass）`,
        });
      }
      const d = await fetchRules();
      apply(d);
    } finally {
      setBusy(false);
    }
  };

  const nDirect = st?.direct.length ?? 0;
  const nProxied = st?.proxied.length ?? 0;
  return (
    <div className="upstream-wrap" ref={wrapRef}>
      <button
        className={`btn ghost${nDirect || nProxied ? ' soft' : ''}`}
        onClick={() => setOpen((o) => !o)}
        title={
          nDirect || nProxied
            ? `分流规则：跳过代理 ${nDirect} 条 / 强制走代理 ${nProxied} 条`
            : '设置哪些域名/IP 不经过代理（直连），哪些强制走代理'
        }
      >
        <IconRoute />
        <span className="btn-label">{nDirect || nProxied ? '分流' : '分流规则'}</span>
        {nDirect || nProxied ? <span>{nDirect}/{nProxied}</span> : null}
      </button>
      {open && (
        <div className="upstream-pop rules-pop">
          <div className="upstream-pop-title">分流规则（跳过代理 / 走代理）</div>
          <div className="upstream-pop-desc">
            每行一条，支持域名、通配符与 IP：
            <code>example.com</code>（含子域）、<code>*.foo.com</code>、<code>1.2.3.4</code>、
            <code>192.168.1.*</code>、<code>10.0.0.0/8</code>。规则保存后持久化到
            <code>~/.miniproxy/config.json</code>，重启后沿用。
          </div>
          <div className="rules-field">
            <label>跳过代理（直连源站，不经上游）</label>
            <textarea
              className="rules-textarea"
              rows={5}
              spellCheck={false}
              placeholder={'例如：\n*.company.internal\n192.168.0.0/16\n10.1.2.3'}
              value={directText}
              onChange={(e) => setDirectText(e.target.value)}
            />
          </div>
          <div className="rules-field">
            <label>强制走代理（优先级最高，覆盖上面的直连与内置直连段）</label>
            <textarea
              className="rules-textarea"
              rows={3}
              spellCheck={false}
              placeholder={'例如：\n*.githubusercontent.com\n1.2.3.4'}
              value={proxiedText}
              onChange={(e) => setProxiedText(e.target.value)}
            />
          </div>
          <label className="rules-check">
            <input type="checkbox" checked={noMitm} onChange={(e) => setNoMitm(e.target.checked)} />
            直连的域名不做解密（推荐：内网/自签证书站点不报证书错，但仍会记录为隧道）
          </label>
          <div className="upstream-pop-row">
            <button className="btn primary" disabled={busy} onClick={save}>
              {busy ? '保存中…' : '保存'}
            </button>
            <span className="rules-hint">
              {st?.systemProxyOn
                ? `系统代理已开启，保存后同步 bypass（当前 ${st.systemBypass.length} 条）`
                : '系统代理未开启：规则只作用于 MiniProxy 自身的出站'}
            </span>
          </div>
          {msg && <div className={`upstream-msg ${msg.kind}`}>{msg.text}</div>}
          <div className="upstream-pop-desc">
            内置直连（无需配置）：{st?.builtinDirect.join('、')}
          </div>
        </div>
      )}
    </div>
  );
}

/* ---------------- 通用小组件 ---------------- */
function MethodBadge({ method }: { method: string }) {
  const cls = method === 'WS' ? 'ws' : method === 'TUNNEL' ? 'tcp' : '';
  return <span className={`method-badge ${cls}`}>{method}</span>;
}

/** 多选/单选筛选下拉：按钮 + 勾选面板（含搜索、清空、全选）。 */
function MultiSelect({
  label,
  options,
  selected,
  onChange,
  width = 124,
  searchable = false,
  panelWidth = 240,
  single = false,
}: {
  label: string;
  options: FilterOption[];
  selected: string[];
  onChange: (next: string[]) => void;
  width?: number;
  searchable?: boolean;
  panelWidth?: number;
  /** 单选模式：点击即选中并关闭面板 */
  single?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [kw, setKw] = useState('');
  const boxRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (boxRef.current && !boxRef.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDoc);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  const toggle = (v: string) =>
    onChange(selected.includes(v) ? selected.filter((s) => s !== v) : [...selected, v]);

  const shown =
    searchable && kw.trim()
      ? options.filter((o) => o.label.toLowerCase().includes(kw.trim().toLowerCase()))
      : options;

  const summary =
    selected.length === 0
      ? label
      : selected.length <= 2
      ? selected.map((v) => shortOf(options, v)).join('、')
      : `${label} · ${selected.length}`;

  return (
    <div className="ms" ref={boxRef} style={{ '--ms-w': width ? `${width}px` : undefined } as React.CSSProperties}>
      <button
        type="button"
        className={`ms-btn${selected.length ? ' active' : ''}${open ? ' open' : ''}`}
        onClick={() => setOpen((o) => !o)}
        title={selected.length ? selected.map((v) => shortOf(options, v)).join(', ') : label}
      >
        <span className="ms-text">{summary}</span>
        <span className="ms-caret"><IconChevronDown size={10} /></span>
      </button>
      {open && (
        <div className="ms-panel" style={{ width: panelWidth }}>
          {searchable && (
            <input
              className="ms-search"
              placeholder="输入以筛选…"
              value={kw}
              onChange={(e) => setKw(e.target.value)}
              autoFocus
            />
          )}
          <div className="ms-list">
            {shown.map((o) => {
              const on = selected.includes(o.value);
              if (single) {
                return (
                  <button
                    type="button"
                    key={o.value}
                    className={`ms-item${on ? ' on' : ''}`}
                    onClick={() => {
                      onChange([o.value]);
                      setOpen(false);
                    }}
                  >
                    <span className="ms-check">{on ? <IconCheck size={11} /> : ''}</span>
                    <span className="ms-item-label" title={o.label}>{o.label}</span>
                    {o.count != null && <span className="ms-item-count">{o.count}</span>}
                  </button>
                );
              }
              return (
                <label key={o.value} className={`ms-item${on ? ' on' : ''}`}>
                  <input type="checkbox" checked={on} onChange={() => toggle(o.value)} />
                  <span className="ms-item-label" title={o.label}>{o.label}</span>
                  {o.count != null && <span className="ms-item-count">{o.count}</span>}
                </label>
              );
            })}
            {shown.length === 0 && <div className="ms-empty">无匹配项</div>}
          </div>
          {!single && (
            <div className="ms-footer">
              <button type="button" className="ms-link" onClick={() => onChange([])} disabled={!selected.length}>
                清空
              </button>
              <button
                type="button"
                className="ms-link"
                onClick={() => onChange(Array.from(new Set([...selected, ...shown.map((o) => o.value)])))}
              >
                全选
              </button>
              <span className="ms-spacer" />
              <button type="button" className="ms-link primary" onClick={() => setOpen(false)}>
                完成
              </button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/** 按钮内的紧凑文案：优先用短标签，其次用原值。 */
function shortOf(options: FilterOption[], value: string): string {
  const o = options.find((x) => x.value === value);
  if (!o) return value;
  return o.short ?? o.label;
}

function statusText(s: EntrySummary): string {
  if (s.kind === 'ws' && s.status === 101) return '101';
  if (s.kind === 'tcp') return s.done ? 'TCP' : '···';
  if (s.status == null) return s.error ? 'ERR' : '···';
  return String(s.status);
}

/* ---------------- 应用 ---------------- */
type FilterKey = 'kinds' | 'rtypes' | 'methods' | 'statuses' | 'hosts' | 'sites' | 'apps';

/** facet 条目 -> 下拉选项。`lowerValue` 用于值大小写不敏感的维度（域名/应用）：筛选值统一小写，展示保留原样。 */
function facetOptions(
  items: { value: string; count: number }[],
  lowerValue = false
): FilterOption[] {
  return items.map((it) => ({
    value: lowerValue ? it.value.toLowerCase() : it.value,
    label: it.value,
    short: it.value.length > 16 ? `${it.value.slice(0, 15)}…` : it.value,
    count: it.count,
  }));
}

export default function App() {
  const [filters, setFilters] = useState<Filters>(emptyFilters());
  const [entries, setEntries] = useState<EntrySummary[]>([]);
  const [facets, setFacets] = useState<Facets>(emptyFacets());
  const [selectedId, setSelectedId] = useState<number | null>(null);
  // 详情面板默认收起：点左侧记录才展开，可通过关闭按钮 / Esc 收起
  const [panelOpen, setPanelOpen] = useState(false);
  const [detail, setDetail] = useState<EntryDetail | null>(null);
  const [groupBy, setGroupBy] = useState<GroupDim>('none');
  const [qInput, setQInput] = useState('');
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [live, setLive] = useState(false);
  const [paused, setPaused] = useState(false);
  const pendingRef = useRef<EntrySummary[]>([]);
  const [pendingCount, setPendingCount] = useState(0);
  const [tab, setTab] = useState<'req' | 'res' | 'ws' | 'raw'>('req');
  const [sysProxy, setSysProxyState] = useState<SysProxyStatus | null>(null);
  const [sysBusy, setSysBusy] = useState(false);
  const [info, setInfo] = useState<Awaited<ReturnType<typeof fetchInfo>> | null>(null);
  const [lanDismissed, setLanDismissed] = useState(false);
  // 视频下载器面板
  const [videoOpen, setVideoOpen] = useState(false);
  const [videos, setVideos] = useState<VideoItem[] | null>(null);
  const [videoErr, setVideoErr] = useState<string | null>(null);
  // 局域网帮助默认收起：点提示条才展开说明与二维码
  const [lanHelpOpen, setLanHelpOpen] = useState(false);
  const esRef = useRef<EventSource | null>(null);
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  const filtersRef = useRef(filters);
  filtersRef.current = filters;

  // 加载列表（过滤条件变化时）
  const reload = useCallback(async (f: Filters) => {
    const d = await fetchEntries(f);
    setEntries(d.items);
    setPendingCount(0);
    pendingRef.current = [];
  }, []);

  useEffect(() => {
    reload(filters);
    fetchFacets().then(setFacets);
  }, [filters, reload]);

  // 关键词防抖：正文搜索是服务端全量扫描，逐键触发会浪费；250ms 后再查
  useEffect(() => {
    const t = setTimeout(() => {
      setFilters((f) => (f.q === qInput ? f : { ...f, q: qInput }));
    }, 250);
    return () => clearTimeout(t);
  }, [qInput]);

  // SSE 实时推送
  useEffect(() => {
    const es = new EventSource('/api/stream');
    esRef.current = es;
    es.onopen = () => setLive(true);
    es.onerror = () => setLive(false);
    es.onmessage = (ev) => {
      try {
        const entry: EntrySummary = JSON.parse(ev.data);
        if (pausedRef.current) {
          pendingRef.current.push(entry);
          setPendingCount((c) => c + 1);
          return;
        }
        ingest(entry);
      } catch {
        /* ignore */
      }
    };
    return () => es.close();
  }, []);

  /**
   * 搜索词激活时，「新记录是否命中」取决于请求/响应正文，只有服务端能看到；
   * 因此改为防抖重查，而不是本地丢弃（否则正文命中的记录会漏掉）。
   */
  const reloadTimer = useRef<number | null>(null);
  const scheduleReload = useCallback(() => {
    if (reloadTimer.current != null) return;
    reloadTimer.current = window.setTimeout(() => {
      reloadTimer.current = null;
      reload(filtersRef.current);
    }, 700);
  }, [reload]);

  const ingest = useCallback(
    (entry: EntrySummary) => {
      const f = filtersRef.current;
      const ok =
        (f.kinds.length === 0 || f.kinds.includes(entry.kind)) &&
        (f.rtypes.length === 0 || f.rtypes.includes(entry.resourceType)) &&
        (f.hosts.length === 0 || f.hosts.includes(entry.host)) &&
        (f.sites.length === 0 || f.sites.includes(entry.site)) &&
        (f.apps.length === 0 ||
          f.apps.includes((entry.client ?? '').toLowerCase())) &&
        (f.methods.length === 0 ||
          f.methods.includes(entry.method.toUpperCase())) &&
        (f.statuses.length === 0 ||
          (entry.status != null &&
            f.statuses.some((st) => String(entry.status).startsWith(st))));
      if (!ok) return;
      if (f.q) {
        const hit = `${entry.url} ${entry.host} ${entry.site} ${entry.client ?? ''} ${entry.method}`
          .toLowerCase()
          .includes(f.q.toLowerCase());
        if (hit) setEntries((list) => [entry, ...list].slice(0, 1000));
        else scheduleReload();
        return;
      }
      setEntries((list) => [entry, ...list].slice(0, 1000));
    },
    [scheduleReload]
  );

  // 各维度候选值（带条数）；域名/应用的筛选值统一小写（与后端匹配规则一致），展示保留原样
  const hostOptions = useMemo<FilterOption[]>(() => facetOptions(facets.host, true), [facets.host]);
  const siteOptions = useMemo<FilterOption[]>(() => facetOptions(facets.site, true), [facets.site]);
  const appOptions = useMemo<FilterOption[]>(() => facetOptions(facets.app, true), [facets.app]);
  const kindOptions = useMemo<FilterOption[]>(
    () => KIND_OPTIONS.map((o) => ({ ...o, count: facets.kind.find((f) => f.value === o.value)?.count })),
    [facets.kind]
  );
  const typeOptions = useMemo<FilterOption[]>(
    () =>
      RESOURCE_OPTIONS.map((o) => ({
        ...o,
        count: facets.type.find((f) => f.value === o.value)?.count,
      })),
    [facets.type]
  );
  const statusOptions = useMemo<FilterOption[]>(
    () => STATUS_OPTIONS.map((o) => ({ ...o, count: facets.status.find((f) => f.value === o.value)?.count })),
    [facets.status]
  );

  // 已选条件（chips）
  const activeChips = useMemo(() => {
    const groups: { key: FilterKey; label: string; options: FilterOption[] }[] = [
      { key: 'kinds', label: '协议', options: kindOptions },
      { key: 'rtypes', label: '类型', options: typeOptions },
      { key: 'methods', label: '方法', options: METHOD_OPTIONS },
      { key: 'statuses', label: '状态', options: statusOptions },
      { key: 'hosts', label: '域名', options: hostOptions },
      { key: 'sites', label: '站点', options: siteOptions },
      { key: 'apps', label: '应用', options: appOptions },
    ];
    const out: { key: FilterKey; group: string; value: string; label: string }[] = [];
    for (const g of groups) {
      for (const v of filters[g.key]) {
        out.push({ key: g.key, group: g.label, value: v, label: optionLabel(g.options, v) });
      }
    }
    return out;
  }, [filters, hostOptions, siteOptions, appOptions, kindOptions, typeOptions, statusOptions]);

  const removeChip = (key: FilterKey, value: string) =>
    setFilters({ ...filters, [key]: filters[key].filter((v) => v !== value) });

  // 刷新各维度统计（节流）
  useEffect(() => {
    const t = setInterval(() => fetchFacets().then(setFacets), 5000);
    return () => clearInterval(t);
  }, []);

  // 切换分组维度时清空折叠状态，避免不同维度的同名分组互相影响
  useEffect(() => {
    setCollapsed(new Set());
  }, [groupBy]);

  // 系统代理状态
  const refreshSysProxy = useCallback(() => {
    fetchSysProxy().then(setSysProxyState).catch(() => {});
  }, []);
  useEffect(() => {
    fetchInfo().then(setInfo).catch(() => {});
    refreshSysProxy();
    const t = setInterval(refreshSysProxy, 5000);
    return () => clearInterval(t);
  }, [refreshSysProxy]);

  // 局域网设备（手机抓包）提示：任一条目来自非本机 IP 即显示
  const lanDeviceIp = useMemo(() => {
    for (const e of entries) {
      const ip = e.clientIp;
      if (ip && ip !== '127.0.0.1' && ip !== '::1') return ip;
    }
    return null;
  }, [entries]);

  // 局域网证书下载地址：优先后端给的完整 URL；否则按检测到的设备 IP / 本机局域网 IP 推导（泛解析 xip 风格域名）
  const lanCaUrl = useMemo(() => {
    if (info && !info.caUrl.startsWith('/')) return info.caUrl;
    const ip = lanDeviceIp ?? info?.lanIp ?? null;
    return ip ? `http://${ip.split('.').slice(0, 3).join('.')}.x:${info?.apiPort ?? 9000}/api/ca.crt` : null;
  }, [info, lanDeviceIp]);

  const toggleSysProxy = useCallback(async () => {
    if (!sysProxy?.supported || sysBusy) return;
    const next = !sysProxy.active;
    if (
      next &&
      !window.confirm(
        `将把系统 HTTP/HTTPS 代理指向 MiniProxy（127.0.0.1:${sysProxy.port}），所有系统流量将经过本工具。\n\n确定开启？`
      )
    ) {
      return;
    }
    setSysBusy(true);
    try {
      const s = await setSysProxy(next);
      setSysProxyState(s);
      if (s.error) alert(`操作失败：${s.error}`);
    } finally {
      setSysBusy(false);
    }
  }, [sysProxy, sysBusy]);

  // 视频下载器：打开/刷新时拉取聚合列表
  const loadVideos = useCallback(async () => {
    setVideoErr(null);
    setVideos(null);
    try {
      setVideos(await fetchVideos());
    } catch (e) {
      setVideoErr(e instanceof Error ? e.message : String(e));
    }
  }, []);
  const openVideos = useCallback(() => {
    setVideoOpen(true);
    loadVideos();
  }, [loadVideos]);

  // 详情：选中即拉取；ws 进行中每 1.5s 刷新
  useEffect(() => {
    if (selectedId == null) return;
    setDetail(null);
    let alive = true;
    const load = () => fetchDetail(selectedId).then((d) => alive && setDetail(d));
    load();
    const t = setInterval(() => {
      if (detailRef.current?.kind === 'ws' && !detailRef.current?.done) load();
    }, 1500);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [selectedId]);

  const detailRef = useRef(detail);
  detailRef.current = detail;

  // 详情 tab 默认值
  useEffect(() => {
    if (detail?.kind === 'ws') setTab('ws');
    else if (detail?.kind === 'tcp') setTab('raw');
    else setTab('req');
  }, [detail?.id, detail?.kind]);

  const exportQuery = useMemo(() => filtersToQuery(filters), [filters]);

  // 按所选维度分组（保留「最新在前」的出现顺序）
  const groups = useMemo(() => {
    if (groupBy === 'none') return null;
    const m = new Map<string, EntrySummary[]>();
    for (const e of entries) {
      const k = groupKeyOf(e, groupBy);
      if (!m.has(k)) m.set(k, []);
      m.get(k)!.push(e);
    }
    return Array.from(m.entries());
  }, [entries, groupBy]);

  /** 分组标题上的「只看此组」：把该分组加入/移出对应筛选维度 */
  const toggleGroupFilter = (key: string) => {
    if (groupBy === 'none') return;
    const fk = DIM_FILTER_KEY[groupBy];
    const cur = filters[fk];
    const val = groupKeyToFilter(key, groupBy);
    setFilters({
      ...filters,
      [fk]: cur.includes(val) ? cur.filter((v) => v !== val) : [...cur, val],
    });
  };

  const resume = () => {
    setPaused(false);
    const pending = pendingRef.current;
    pendingRef.current = [];
    setEntries((list) => [...pending.reverse(), ...list].slice(0, 1000));
    setPendingCount(0);
  };

  const onClear = async () => {
    await clearEntries();
    setEntries([]);
    setSelectedId(null);
    setDetail(null);
    setPanelOpen(false);
  };

  /** 点左侧记录：记录选中并展开右侧详情面板 */
  const handleSelect = useCallback((id: number) => {
    setSelectedId(id);
    setPanelOpen(true);
  }, []);

  /* ---------------- 详情面板宽度：可拖拽分隔条 ---------------- */
  const mainRef = useRef<HTMLDivElement>(null);
  const detailPctRef = useRef(0.44);
  const [detailPct, setDetailPct] = useState(0.44);

  const onSplitDown = useCallback((e: React.PointerEvent) => {
    const rect = mainRef.current?.getBoundingClientRect();
    if (!rect || rect.width <= 0) return;
    e.preventDefault();
    const move = (ev: PointerEvent) => {
      const pct = 1 - (ev.clientX - rect.left) / rect.width;
      detailPctRef.current = Math.min(0.78, Math.max(0.22, pct));
      setDetailPct(detailPctRef.current);
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
    };
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  }, []);

  const onSplitReset = useCallback(() => {
    detailPctRef.current = 0.44;
    setDetailPct(0.44);
  }, []);

  /* ---------------- 窄屏顶栏「更多」抽屉 ---------------- */
  const actionsRef = useRef<HTMLDivElement>(null);
  const menuBtnRef = useRef<HTMLButtonElement>(null);
  const [menuOpen, setMenuOpen] = useState(false);
  const [filtersOpen, setFiltersOpen] = useState(false);

  const activeFilterCount =
    filters.kinds.length +
    filters.rtypes.length +
    filters.methods.length +
    filters.statuses.length +
    filters.hosts.length +
    filters.sites.length +
    filters.apps.length;

  useEffect(() => {
    if (!menuOpen) return;
    const onDoc = (e: MouseEvent) => {
      const t = e.target as Node;
      if (actionsRef.current?.contains(t) || menuBtnRef.current?.contains(t)) return;
      setMenuOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMenuOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDoc);
      document.removeEventListener('keydown', onKey);
    };
  }, [menuOpen]);

  // 窗口变宽（回到桌面布局）时收起抽屉，避免状态残留
  useEffect(() => {
    const onResize = () => {
      if (window.innerWidth > 1100) setMenuOpen(false);
    };
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);

  // 面板打开时按 Esc 收起
  useEffect(() => {
    if (!panelOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setPanelOpen(false);
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [panelOpen]);

  return (
    <div className="app">
      <header className="header">
        <div className="brand">
          <span className="logo">M</span>
          <span>MiniProxy</span>
          <span className="sub">抓包工具 · HTTP / HTTPS / WS / TCP</span>
        </div>
        <span className={`connection-dot ${live ? 'live' : ''}`}>
          <span className="dot" />
          <span className="dot-text">{live ? '实时连接中' : '未连接'}</span>
        </span>
        <div className="spacer" />
        <div className={`header-actions${menuOpen ? ' open' : ''}`} ref={actionsRef}>
          {pendingCount > 0 && (
            <button className="btn primary" onClick={resume}>
              {pendingCount} 条新记录，点击加载
            </button>
          )}
          <div className="btn-group">
            <button
              className={`btn ghost${paused ? ' soft' : ''}`}
              onClick={() => (paused ? resume() : setPaused(true))}
              title={paused ? '继续接收新记录' : '暂停刷新列表（抓包仍在继续）'}
            >
              {paused ? <IconPlay /> : <IconPause />}
              <span className="btn-label">{paused ? '恢复' : '暂停'}</span>
            </button>
            <button className="btn ghost danger" onClick={onClear} title="清空当前所有抓包记录">
              <IconTrash />
              <span className="btn-label">清空</span>
            </button>
            <button className="btn ghost" onClick={openVideos} title="列出抓到的完整视频，点击即可下载">
              <IconClapperboard />
              <span className="btn-label">视频</span>
            </button>
          </div>
          <span className="header-sep" />
          <div className="btn-group">
            {sysProxy?.supported && (
              <button
                className={`btn ${sysProxy.active ? 'primary' : 'ghost'}`}
                disabled={sysBusy}
                onClick={toggleSysProxy}
                title={
                  sysProxy.active
                    ? '点击关闭系统代理并恢复直连'
                    : `一键把系统 HTTP/HTTPS 代理指向 127.0.0.1:${sysProxy.port}`
                }
              >
                {sysProxy.active ? (
                  <>
                    <IconGlobe />
                    <span className="btn-label">系统代理</span>
                    <span>已开启</span>
                  </>
                ) : (
                  <>
                    <IconGlobe />
                    <span className="btn-label">系统代理</span>
                    <span>已关闭</span>
                  </>
                )}
              </button>
            )}
            <UpstreamControl />
            <RulesControl />
            <BypassControl />
          </div>
          <span className="header-sep" />
          <div className="btn-group">
            <QuitControl />
            <ExportMenu query={exportQuery} />
            <ThemeToggle />
          </div>
        </div>
        <button
          type="button"
          className="header-menu-btn"
          ref={menuBtnRef}
          onClick={() => setMenuOpen((o) => !o)}
          aria-label="更多操作"
          aria-expanded={menuOpen}
          title="更多操作"
        >
          <IconMenu size={17} />
          {pendingCount > 0 && <span className="menu-badge">{pendingCount}</span>}
        </button>
      </header>

      <div className="toolbar">
        <div className={`search-wrap${qInput ? ' has-value' : ''}`}>
          <IconSearch size={13} strokeWidth={2.4} className="search-icon" />
          <input
            type="text"
            className="search"
            placeholder="搜索 URL / 域名 / 请求与响应内容…"
            title="关键词匹配范围：URL、域名、站点、应用、请求头与请求体、响应头与响应体（含解压后内容）、WebSocket 消息"
            value={qInput}
            onChange={(e) => setQInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') setQInput('');
            }}
          />
          {qInput && (
            <button type="button" className="search-clear" title="清空搜索" onClick={() => setQInput('')}>
              <IconX size={12} />
            </button>
          )}
        </div>
        <button
          type="button"
          className={`filters-toggle${filtersOpen ? ' open' : ''}`}
          onClick={() => setFiltersOpen((o) => !o)}
          aria-expanded={filtersOpen}
          title="展开/收起筛选条件"
        >
          <IconSliders />
          <span>筛选</span>
          {activeFilterCount > 0 && <span className="ft-count">{activeFilterCount}</span>}
          <span className="ms-caret">
            {filtersOpen ? <IconChevronUp size={10} /> : <IconChevronDown size={10} />}
          </span>
        </button>
        <div className={`toolbar-filters${filtersOpen ? ' open' : ''}`}>
          <div className="filter-bar">
          <MultiSelect
            label="全部协议"
            options={kindOptions}
            selected={filters.kinds}
            onChange={(v) => setFilters({ ...filters, kinds: v })}
            width={118}
          />
        <MultiSelect
          label="全部类型"
          options={typeOptions}
          selected={filters.rtypes}
          onChange={(v) => setFilters({ ...filters, rtypes: v })}
          width={122}
          panelWidth={230}
        />
        <MultiSelect
          label="全部方法"
          options={METHOD_OPTIONS}
          selected={filters.methods}
          onChange={(v) => setFilters({ ...filters, methods: v })}
          width={106}
        />
        <MultiSelect
          label="全部状态"
          options={statusOptions}
          selected={filters.statuses}
          onChange={(v) => setFilters({ ...filters, statuses: v })}
          width={112}
        />
        <MultiSelect
          label="全部域名"
          options={hostOptions}
          selected={filters.hosts}
          onChange={(v) => setFilters({ ...filters, hosts: v })}
          width={148}
          panelWidth={290}
          searchable
        />
        <MultiSelect
          label="全部站点"
          options={siteOptions}
          selected={filters.sites}
          onChange={(v) => setFilters({ ...filters, sites: v })}
          width={140}
          panelWidth={260}
          searchable
        />
        <MultiSelect
          label="全部应用"
          options={appOptions}
          selected={filters.apps}
          onChange={(v) => setFilters({ ...filters, apps: v })}
            width={132}
            panelWidth={250}
            searchable
          />
          </div>
        </div>
        <div className="toolbar-right">
          <div className="divider" />
          <span className="tb-label">分组</span>
          <MultiSelect
            label="分组方式"
            options={GROUP_DIMS}
            selected={[groupBy]}
            onChange={(v) => setGroupBy((v[0] ?? 'none') as GroupDim)}
            width={126}
            single
          />
        </div>
      </div>

      {activeChips.length > 0 && (
        <div className="chips">
          {activeChips.map((c) => (
            <span key={`${c.group}-${c.value}`} className="chip">
              <span className="chip-group">{c.group}</span>
              <span className="chip-label" title={c.label}>{c.label}</span>
              <button
                type="button"
                className="chip-x"
                title="移除该条件"
                onClick={() => removeChip(c.key, c.value)}
              >
                <IconX size={11} />
              </button>
            </span>
          ))}
          <button
            type="button"
            className="chips-clear"
            onClick={() => {
              setQInput('');
              setFilters(emptyFilters());
            }}
          >
            清空全部
          </button>
        </div>
      )}

      {!lanDismissed && (
        <div className="lan-banner">
          <div className="lan-bar">
            <span className="lan-hint">
              <IconSmartphone size="1em" />{' '}
              {lanDeviceIp
                ? `检测到局域网设备 ${lanDeviceIp} 正在使用代理`
                : '想用手机 / 局域网设备抓包？让手机代理指向本机即可，首次使用需安装 CA 证书'}
            </span>
            {info?.lanIp && (
              <CopyButton
                text={`${info.lanIp}:${info.proxyPort}`}
                label={
                  <>
                    <IconWifi size={13} />
                    代理 {info.lanIp}:{info.proxyPort}
                  </>
                }
                title="点击复制手机 Wi-Fi 代理要填的地址"
              />
            )}
            <button
              type="button"
              className={`lan-help-btn${lanHelpOpen ? ' open' : ''}`}
              onClick={() => setLanHelpOpen((o) => !o)}
              title="展开/收起手机抓包证书配置帮助"
            >
              {lanHelpOpen ? (
                <>
                  收起帮助 <IconChevronUp size={11} />
                </>
              ) : (
                <>
                  证书配置帮助 <IconChevronDown size={11} />
                </>
              )}
            </button>
            <button type="button" className="lan-close" title="不再提示" onClick={() => setLanDismissed(true)}>
              <IconX size={13} />
            </button>
          </div>
          {lanHelpOpen && (
            <div className="lan-detail-row">
              <div className="lan-text">
                <div className="lan-steps">
                  {lanDeviceIp
                    ? 'TLS 握手失败是因为该设备尚未信任 MiniProxy CA 证书。安装步骤：'
                    : '手机等局域网设备走本代理抓包 HTTPS，需先安装并信任 MiniProxy CA 证书。步骤：'}
                  <ol>
                    <li>
                      {info?.lanIp ? (
                        <>
                          手机 Wi-Fi 代理设为手动：服务器 <code>{info.lanIp}</code>，端口{' '}
                          <code>{info.proxyPort}</code>
                        </>
                      ) : (
                        <>手机与电脑连同一 Wi-Fi，代理指向电脑的局域网 IP:{info?.proxyPort ?? 34567}</>
                      )}
                    </li>
                    <li>
                      {lanCaUrl ? (
                        <>
                          手机浏览器打开 <code>{lanCaUrl}</code> 下载证书
                          <button
                            type="button"
                            className="lan-copy"
                            onClick={() => {
                              navigator.clipboard?.writeText(lanCaUrl);
                            }}
                            title="复制证书下载地址"
                          >
                            复制地址
                          </button>
                        </>
                      ) : (
                        <>浏览器访问 <code>/api/ca.crt</code> 下载证书</>
                      )}
                    </li>
                    <li>
                      <b>iOS</b>：设置 → 通用 → VPN与设备管理 → 安装描述文件，再到「设置 → 通用 →
                      关于本机 → <b>证书信任设置</b>」开启完全信任（关键，漏掉这步仍会握手失败）
                    </li>
                    <li>
                      <b>Android</b>：设置 → 安全 → 更多安全设置 → 加密与凭据 → 安装 CA 证书
                      （安卓 7+ 多数 App 默认不信任用户证书，仅浏览器等可用）
                    </li>
                    <li>个别 App 有证书固定（pinning），装了证书也无法解密，属正常现象</li>
                  </ol>
                </div>
              </div>
              {lanCaUrl && (
                <div className="lan-qr" title="手机扫码打开证书下载页">
                  <QRCodeSVG value={lanCaUrl} size={72} />
                  <span>扫码下载证书</span>
                </div>
              )}
            </div>
          )}
        </div>
      )}

      <div className={`main${panelOpen ? ' with-detail' : ''}`} ref={mainRef}>
        <section className="list-pane">
          <div className="list-meta">
            共 {entries.length} 条{hasAnyFilter(filters) ? ' · 已筛选' : ''}
            {groupBy !== 'none' &&
              ` · ${GROUP_DIMS.find((d) => d.value === groupBy)?.label ?? ''}（${groups?.length ?? 0} 组）`}
          </div>
          <div className="list-scroll">
            {entries.length === 0 ? (
              <div className="empty-state">
                <IconRadar size={34} className="empty-icon" />
                <div>暂无抓包数据</div>
                <div>
                  将代理设置为 <code>http://127.0.0.1:34567</code> 或点击右上角「系统代理」一键开启，
                  <br />
                  或用 curl 测试：<code>curl -x http://127.0.0.1:34567 http://example.com</code>
                </div>
              </div>
            ) : groups ? (
              groups.map(([key, items]) => {
                const isCollapsed = collapsed.has(key);
                const label = groupLabelOf(key, groupBy);
                const fk = groupBy === 'none' ? null : DIM_FILTER_KEY[groupBy];
                const filterVal = groupKeyToFilter(key, groupBy);
                const on = !!fk && filters[fk].includes(filterVal);
                const canFilter = groupFilterable(key, groupBy);
                return (
                  <React.Fragment key={key}>
                    <div
                      className="group-header"
                      onClick={() =>
                        setCollapsed((s) => {
                          const n = new Set(s);
                          if (n.has(key)) n.delete(key);
                          else n.add(key);
                          return n;
                        })
                      }
                    >
                      <span className="gh-caret">
                        {isCollapsed ? <IconChevronRight size={11} /> : <IconChevronDown size={11} />}
                      </span>
                      <span className="gh-label" title={label}>{label}</span>
                      <span className="cnt">{items.length}</span>
                      {canFilter && (
                        <button
                          type="button"
                          className={`gh-filter${on ? ' on' : ''}`}
                          title={on ? `取消筛选「${label}」` : `只看「${label}」`}
                          onClick={(e) => {
                            e.stopPropagation();
                            toggleGroupFilter(key);
                          }}
                        >
                          {on ? (
                            <>
                              <IconCheck size={10} />
                              已筛选
                            </>
                          ) : (
                            '只看此组'
                          )}
                        </button>
                      )}
                    </div>
                    {!isCollapsed && (
                      <EntryTable items={items} selectedId={selectedId} onSelect={handleSelect} grouped />
                    )}
                  </React.Fragment>
                );
              })
            ) : (
              <EntryTable items={entries} selectedId={selectedId} onSelect={handleSelect} />
            )}
          </div>
        </section>

        {panelOpen && (
          <>
            <div
              className="splitter"
              onPointerDown={onSplitDown}
              onDoubleClick={onSplitReset}
              title="拖动调整宽度，双击恢复默认"
              role="separator"
              aria-orientation="vertical"
            />
            <section className="detail-pane" style={{ flex: `0 0 ${(detailPct * 100).toFixed(2)}%` }}>
              <button
                type="button"
                className="detail-close"
                title="关闭详情面板（Esc）"
                onClick={() => setPanelOpen(false)}
              >
                <IconX size={14} />
              </button>
              {detail ? (
                <Detail detail={detail} tab={tab} setTab={setTab} />
              ) : (
                <div className="empty-state">
                  <IconSpinner size={28} className="empty-icon" />
                  <div>正在加载详情…</div>
                </div>
              )}
            </section>
          </>
        )}
      </div>

      {videoOpen && (
        <div className="modal-mask" onClick={() => setVideoOpen(false)}>
          <div className="video-modal" onClick={(e) => e.stopPropagation()} role="dialog" aria-label="视频下载器">
            <div className="video-modal-head">
              <span
                className="sub-title"
                style={{ margin: 0, display: 'inline-flex', alignItems: 'center', gap: 6 }}
              >
                <IconClapperboard size={13} />
                视频下载
              </span>
              <span className="video-hint">
                来自本次抓包 · YouTube 走 SABR 私有协议（拿不到直链），下载时会自动补拉缺失的分段
              </span>
              <div className="spacer" />
              <button type="button" className="btn" onClick={loadVideos}>
                <IconRefresh />
                刷新
              </button>
              <button type="button" className="btn" onClick={() => setVideoOpen(false)} title="关闭">
                <IconX size={14} />
              </button>
            </div>
            <div className="video-modal-body">
              {videoErr && (
                <div className="empty-state">
                  <IconAlertTriangle size={28} className="empty-icon" />
                  <div>{videoErr}</div>
                </div>
              )}
              {!videoErr && videos === null && (
                <div className="empty-state">
                  <IconSpinner size={28} className="empty-icon" />
                  <div>正在扫描抓包记录…</div>
                </div>
              )}
              {!videoErr && videos !== null && videos.length === 0 && (
                <div className="empty-state">
                  <IconClapperboard size={34} className="empty-icon" />
                  <div>还没抓到可下载的完整视频</div>
                  <div>播放一次视频（直播除外）让分段被抓全，再点「刷新」</div>
                  <div className="sabr-gap" style={{ marginTop: 6 }}>
                    YouTube 需从 0 开始完整播一遍：初始化段只在开头下发一次，缺了它就拼不成片
                  </div>
                </div>
              )}
              {videos !== null && videos.length > 0 && (
                <div className="video-list">
                  {videos.map((v) => (
                    <div key={`${v.kind}-${v.entryId}`} className="video-row">
                      <span className={`video-kind kind-${v.kind}`}>
                        {v.kind === 'hls'
                          ? 'HLS'
                          : v.kind === 'dash'
                            ? v.rangeGroup
                              ? '分块'
                              : '分段'
                            : v.kind === 'sabr'
                              ? 'SABR'
                              : '直链'}
                      </span>
                      <div className="video-main">
                        <div className="video-name" title={v.url}>{v.name}</div>
                        <div className="video-meta">
                          <span title="来源域名">{v.host}</span>
                          {v.resolution && <span title="分辨率">{v.resolution}</span>}
                          {v.durationSec != null && (
                            <span title={v.kind === 'sabr' ? '已抓到时长 / 视频全长' : '时长'}>
                              {v.kind === 'sabr' && v.capturedSec != null
                                ? `已抓 ${formatDuration(v.capturedSec)} / 全长 ${formatDuration(v.durationSec)}`
                                : formatDuration(v.durationSec)}
                            </span>
                          )}
                          {v.size != null && (
                            <span title={v.sizeExact ? '精确大小' : '按已捕获分段估算'}>
                              {v.sizeExact ? '' : '≈'}{formatBytes(v.size)}
                            </span>
                          )}
                          {v.segments != null && <span title="已捕获分段数">{v.segments} 段</span>}
                          {v.kind === 'sabr' && v.complete === false && (
                            <span
                              className="sabr-gap"
                              title="sequence_number 不连续：浏览器没有请求中间某些分段，成片会缺一小段"
                            >
                              分段不连续
                            </span>
                          )}
                        </div>
                      </div>
                      <a
                        className="btn primary video-dl"
                        href={videoDownloadUrl(v)}
                        download
                        title={
                          v.kind === 'sabr'
                            ? '重组 YouTube 分段（SABR/UMP）并用 ffmpeg 合并 · 缺失分段会自动向服务端补拉，缺口大时可能需要 1-3 分钟'
                            : v.audioEntryId
                              ? '分别拉取音视频轨并用 ffmpeg 合并为一个 mp4'
                              : v.kind === 'dash' && !v.rangeGroup
                                ? '拼接已捕获的分段并下载'
                                : '从源站重新拉取完整视频'
                        }
                      >
                        <IconDownload />
                        下载
                      </a>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/* ---------------- 列表表格 ---------------- */
/* 单行 memo：SSE 持续追加记录时，只有变化的那几行会重渲染 */
const EntryRow = React.memo(function EntryRow({
  e,
  selected,
  onSelect,
}: {
  e: EntrySummary;
  selected: boolean;
  onSelect: (id: number) => void;
}) {
  return (
    <tr className={`row ${selected ? 'selected' : ''}`} onClick={() => onSelect(e.id)}>
      <td><MethodBadge method={e.kind === 'http' ? e.method : e.kind === 'ws' ? 'WS' : 'TUNNEL'} /></td>
      <td className={`status-cell ${statusColor(e.status)}`}>{statusText(e)}</td>
      <td className="url-cell" title={e.url}>
        <span
          className={`type-tag tt-${e.resourceType}`}
          title={`类型：${resourceMeta(e.resourceType).label}${e.contentType ? `\nContent-Type: ${e.contentType}` : ''}`}
        >
          {resourceMeta(e.resourceType).short}
        </span>
        {e.url.replace(/^https?:\/\//, '')}
        {e.encoding && <span className="encoding-tag">{e.encoding}</span>}
        {e.qInContent && (
          <span className="badge hit" title="关键词命中请求/响应内容（URL 未命中）">
            内容匹配
          </span>
        )}
        {e.error && <span className="badge err" title={e.error}>!</span>}
      </td>
      <td
        className="size-cell"
        title={`请求体: ${formatBytes(e.reqSize)} · 响应体: ${formatBytes(e.respSize || e.bytesDown)}\n网络流量: ↑ ${formatBytes(e.bytesUp)} / ↓ ${formatBytes(e.bytesDown)}`}
      >
        {e.kind === 'tcp' ? (
          <>↑{formatBytes(e.bytesUp)} ↓{formatBytes(e.bytesDown)}</>
        ) : (
          <>
            <span className="size-up">↑{formatBytes(e.reqSize)}</span>{' '}
            <span className="size-down">↓{formatBytes(e.respSize || e.bytesDown)}</span>
          </>
        )}
      </td>
      <td
        className={`dur-cell ${e.durationMs >= 3000 ? 'dur-very-slow' : e.durationMs >= 800 ? 'dur-slow' : ''}`}
        title={`耗时 ${formatMs(e.durationMs)}${
          e.kind === 'tcp' || e.kind === 'ws' ? '（连接存续时长）' : ''
        }`}
      >
        {e.done || e.kind === 'http' ? formatMs(e.durationMs) : '···'}
      </td>
      <td>{formatTime(e.ts)}</td>
    </tr>
  );
});

function EntryTable({
  items,
  selectedId,
  onSelect,
  grouped = false,
}: {
  items: EntrySummary[];
  selectedId: number | null;
  onSelect: (id: number) => void;
  /** 处于分组列表中：列头吸顶要下移，给分组标题让位 */
  grouped?: boolean;
}) {
  return (
    <table className={`entries${grouped ? ' grouped' : ''}`}>
      <colgroup>
        <col className="col-method" />
        <col className="col-status" />
        <col />
        <col className="col-size" />
        <col className="col-dur" />
        <col className="col-time" />
      </colgroup>
      <thead>
        <tr>
          <th>方法</th>
          <th>状态</th>
          <th>URL</th>
          <th title="↑ 请求体大小 · ↓ 响应体大小">大小</th>
          <th title="从收到请求到响应体读完的耗时">耗时</th>
          <th>时间</th>
        </tr>
      </thead>
      <tbody>
        {items.map((e) => (
          <EntryRow
            key={`${e.id}-${e.wsMessages}-${e.done}`}
            e={e}
            selected={selectedId === e.id}
            onSelect={onSelect}
          />
        ))}
      </tbody>
    </table>
  );
}

/* ---------------- 详情面板 ---------------- */
function HeadersTable({ headers }: { headers: [string, string][] }) {
  if (!headers?.length) return <div className="sub-title">（无）</div>;
  return (
    <table className="headers">
      <tbody>
        {headers.map(([k, v], i) => (
          <tr key={i}>
            <td className="hk">{k}</td>
            <td className="hv">{v}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

type BodyViewMode = 'text' | 'hex' | 'base64' | 'preview';

/** 小号复制按钮：点击复制文本，1.5 秒内反馈「已复制」。 */
function CopyButton({
  text,
  disabled,
  label,
  title,
}: {
  text: string;
  disabled?: boolean;
  /** 按钮文案，可传 JSX 以带图标；不传时用内置的「复制」样式 */
  label?: React.ReactNode;
  title?: string;
}) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      className="mini-copy"
      disabled={disabled}
      title={title ?? '复制内容'}
      onClick={async (e) => {
        e.stopPropagation();
        try {
          await navigator.clipboard.writeText(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        } catch {
          setCopied(false);
        }
      }}
    >
      {copied ? (
        <>
          <IconCheck size={12} />
          已复制
        </>
      ) : (
        label ?? (
          <>
            <IconCopy size={12} />
            复制
          </>
        )
      )}
    </button>
  );
}

/**
 * 正文查看器：预览 / 文本 / 十六进制 / Base64 四种视图，支持复制与下载原始字节。
 * 图片、音视频、PDF 等多媒体内容默认直接内嵌预览；
 * 二进制或压缩内容在文本视图里会显示为乱码，此时自动切到十六进制并给出提示。
 */
function BodyView({
  text,
  raw,
  decoded,
  truncated,
  label,
  url,
  contentType,
  entryId,
  side = 'resp',
  bodyTruncated,
  rangeStart,
}: {
  text: string | null;
  /** 与 text 对应的原始字节（base64），用于十六进制 / Base64 / 下载 */
  raw: RawView | null;
  decoded: string | null;
  truncated: boolean;
  label?: string;
  /** 用于推导下载文件名 */
  url?: string;
  /** 响应 Content-Type，用于判断是否可内嵌预览 */
  contentType?: string | null;
  /** 条目 ID：视图数据被 256 KB 截断时，预览/下载会改从完整正文接口拉全量 */
  entryId?: number;
  /** 拉取哪一侧的完整正文 */
  side?: 'req' | 'resp';
  /** 抓包时正文本身就被截断（超 4 MB），完整播放本就不可用 */
  bodyTruncated?: boolean;
  /** 请求 Range 起始字节：>0 说明该条目是 Range 分块抓取的一部分 */
  rangeStart?: number | null;
}) {
  const bytes = useMemo(() => (raw ? b64ToBytes(raw.b64) : new Uint8Array(0)), [raw]);
  const binary = looksBinary(text);
  const [mode, setMode] = useState<BodyViewMode>('text');
  const [copied, setCopied] = useState(false);

  // 可预览的多媒体：Content-Type 优先，缺失时按魔数嗅探
  const media = useMemo(
    () => (raw && bytes.length > 0 ? detectMediaKind(contentType, bytes) : null),
    [contentType, raw, bytes],
  );

  // 视图数据被截断但完整正文可取时，预览/下载前先拉全量
  const needsFull = !!(media && raw?.truncated && entryId != null);
  const [full, setFull] = useState<Uint8Array | null>(null);
  const [fullErr, setFullErr] = useState(false);
  useEffect(() => {
    setFull(null);
    setFullErr(false);
    if (!needsFull) return;
    let cancel = false;
    fetchEntryBody(entryId!, side).then((r) => {
      if (cancel) return;
      if (r) setFull(r.bytes);
      else setFullErr(true);
    });
    return () => {
      cancel = true;
    };
  }, [needsFull, entryId, side]);

  // 预览用的 Blob URL，随内容变化重建并释放旧的
  const mediaUrl = useMemo(() => {
    if (!media) return null;
    const b = full ?? bytes;
    if (b.length === 0) return null;
    const ab = new ArrayBuffer(b.byteLength);
    new Uint8Array(ab).set(b);
    return URL.createObjectURL(new Blob([ab], { type: media.mime }));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [media, raw, full]);

  useEffect(() => () => { if (mediaUrl) URL.revokeObjectURL(mediaUrl); }, [mediaUrl]);

  // 分段视频（DASH .m4s / Range 分块）：请求后端拼接后播放
  const isSegment = media?.kind === 'segment';
  const isRangeChunk = media?.kind === 'video' && (rangeStart ?? 0) > 0;
  type StitchState =
    | { state: 'idle' }
    | { state: 'loading' }
    | { state: 'ok'; url: string; note: string | null }
    | { state: 'err'; error: string };
  const [stitch, setStitch] = useState<StitchState>({ state: 'idle' });

  const ensureStitch = useCallback(() => {
    if (entryId == null || stitch.state === 'loading') return;
    setStitch({ state: 'loading' });
    stitchEntryBody(entryId, side).then((r) => {
      if (r.ok) {
        const ab = new ArrayBuffer(r.bytes.byteLength);
        new Uint8Array(ab).set(r.bytes);
        const url = URL.createObjectURL(new Blob([ab], { type: 'video/mp4' }));
        setStitch({ state: 'ok', url, note: r.note });
      } else {
        setStitch({ state: 'err', error: r.error });
      }
    });
  }, [entryId, side, stitch.state]);

  useEffect(() => {
    setStitch({ state: 'idle' });
  }, [isSegment, isRangeChunk, entryId, side]);

  useEffect(() => {
    // DASH 分段自动尝试拼接；Range 分块等用户点击
    if (isSegment) ensureStitch();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isSegment, entryId, side]);

  useEffect(
    () => () => {
      if (stitch.state === 'ok') URL.revokeObjectURL(stitch.url);
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [stitch.state === 'ok' ? stitch.url : null],
  );

  // 切换到另一条记录时，依据内容类型重置默认视图
  useEffect(() => {
    setMode(media ? 'preview' : looksBinary(text) ? 'hex' : 'text');
    setCopied(false);
  }, [text, media]);

  const pretty = mode === 'text' ? tryPrettyJson(text) : null;
  const shown =
    mode === 'text'
      ? (pretty ?? text ?? '')
      : mode === 'hex'
        ? toHexDump(bytes)
        : wrapBase64(raw?.b64 ?? '');

  const hasBody = !!text || bytes.length > 0;
  const downloadable = bytes.length > 0;

  // 音视频 / m3u8：可从源站重新拉取完整文件（m3u8 自动拼接全部分段）
  const ctLow = (contentType ?? '').toLowerCase();
  const canFullVideo =
    entryId != null &&
    (ctLow.split(';')[0].startsWith('video/') ||
      ctLow.split(';')[0].startsWith('audio/') ||
      ctLow.includes('mpegurl') ||
      (url ?? '').toLowerCase().includes('.m3u8'));

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(shown);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      setCopied(false);
    }
  };

  return (
    <>
      <div className="body-toolbar">
        {label && <span className="sub-title" style={{ margin: 0 }}>{label}</span>}
        <div className="view-switch" role="group" aria-label="正文视图">
          {(
            [
              ...(media ? ([['preview', '预览']] as [BodyViewMode, string][]) : []),
              ['text', '文本'],
              ['hex', '十六进制'],
              ['base64', 'Base64'],
            ] as [BodyViewMode, string][]
          ).map(([m, l]) => (
            <button
              key={m}
              type="button"
              className={mode === m ? 'active' : ''}
              disabled={m !== 'text' && !raw}
              onClick={() => setMode(m)}
            >
              {l}
            </button>
          ))}
        </div>
        <div className="body-actions">
          {raw && (
            <span className="raw-size" title="原始字节数">
              {formatBytes(raw.size)}
              {raw.truncated && ' · 视图已截断'}
            </span>
          )}
          <button type="button" onClick={copy} disabled={!hasBody}>
            {copied ? '已复制' : '复制'}
          </button>
          {canFullVideo && (
            <button
              type="button"
              title="不依赖抓包碎片，直接从源站重新拉取完整文件（m3u8 会自动解析并拼接全部分段）"
              onClick={() => {
                window.location.href = `/api/entries/${entryId}/fullvideo`;
              }}
            >
              下载完整视频
            </button>
          )}
          <button
            type="button"
            disabled={!downloadable}
            title="下载原始字节（视图被截断时自动拉取完整正文）"
            onClick={async () => {
              if (entryId != null) {
                // 走标准 http 下载：后端 dl=1 会带上 attachment 头，浏览器和 App 外壳
                // 都能落盘；blob + <a download> 在 App 的 WKWebView 里不可靠
                window.location.href = `/api/entries/${entryId}/body?side=${side}&dl=1`;
                return;
              }
              let b = full;
              if (!b && entryId != null && raw?.truncated) {
                const r = await fetchEntryBody(entryId, side);
                if (r) {
                  setFull(r.bytes);
                  b = r.bytes;
                }
              }
              downloadBytes(b ?? bytes, suggestFilename(url ?? ''));
            }}
          >
            下载原始数据
          </button>
        </div>
      </div>
      {(decoded || truncated || binary) && mode !== 'preview' && (
        <div style={{ marginBottom: 8 }}>
          {decoded && <span className="badge decode">已解压：{decoded}</span>}
          {truncated && <span className="badge warn">内容过大，已截断</span>}
          {pretty && <span className="badge">JSON 已格式化</span>}
          {binary && (
            <span className="badge warn">
              内容非文本（二进制 / 压缩），已默认显示原始字节
            </span>
          )}
          {mode !== 'text' && (
            <span className="badge">{mode === 'hex' ? '十六进制视图' : 'Base64 视图'}</span>
          )}
        </div>
      )}
      {mode === 'preview' && mediaUrl && media && (
        <div style={{ marginBottom: 8 }}>
          {media.kind === 'segment' && stitch.state === 'ok' && (
            <span className="badge">分段视频已拼接{stitch.note ? ` · ${stitch.note}` : ''}</span>
          )}
          {media.kind === 'segment' && stitch.state !== 'ok' && (
            <span className="badge">DASH 分段（.m4s）· 需拼接后播放</span>
          )}
          {isRangeChunk && (
            <span className="badge">Range 分块（起点 {rangeStart}）· 非完整文件</span>
          )}
          {bodyTruncated && media.kind !== 'image' && (
            <span className="badge warn">
              源内容超过 4 MB，抓包时已截断，完整播放可能不可用（建议用「下载原始数据」）
            </span>
          )}
          {!bodyTruncated && raw?.truncated && !full && !fullErr && (
            <span className="badge">正在加载完整内容…</span>
          )}
          {fullErr && raw?.truncated && !full && (
            <span className="badge warn">完整内容加载失败，预览基于前 256 KB</span>
          )}
          {bodyTruncated && media.kind === 'image' && (
            <span className="badge warn">源内容超过 4 MB 已截断，图片可能不完整</span>
          )}
          {media.kind === 'image' && (
            <span className="badge">图片预览 · {media.mime}</span>
          )}
        </div>
      )}
      {mode === 'preview' && media && (isSegment || isRangeChunk) ? (
        <>
          <div className="media-preview mp-video">
            {stitch.state === 'ok' ? (
              <video src={stitch.url} controls preload="metadata" />
            ) : stitch.state === 'loading' ? (
              <div className="sub-title">正在拼接分段视频…</div>
            ) : stitch.state === 'err' ? (
              <div className="stitch-err">{stitch.error}</div>
            ) : (
              <button type="button" className="stitch-btn" onClick={ensureStitch}>
                拼接同一 URL 的 Range 分块并播放
              </button>
            )}
          </div>
        </>
      ) : mode === 'preview' && mediaUrl && media ? (
        <div className={`media-preview mp-${media.kind}`}>
          {media.kind === 'image' && <img src={mediaUrl} alt="响应内容预览" />}
          {media.kind === 'video' && (
            <video src={mediaUrl} controls preload="metadata" />
          )}
          {media.kind === 'audio' && (
            <audio src={mediaUrl} controls preload="metadata" />
          )}
          {media.kind === 'pdf' && <iframe src={mediaUrl} title="PDF 预览" />}
        </div>
      ) : hasBody ? (
        <pre className={`body-view${mode === 'hex' ? ' hex-view' : ''}`}>{shown}</pre>
      ) : (
        <div className="sub-title">（空）</div>
      )}
    </>
  );
}

function Detail({
  detail,
  tab,
  setTab,
}: {
  detail: EntryDetail;
  tab: 'req' | 'res' | 'ws' | 'raw';
  setTab: (t: 'req' | 'res' | 'ws' | 'raw') => void;
}) {
  const kind = detail.kind;
  const tabs: { id: typeof tab; label: string }[] =
    kind === 'ws'
      ? [{ id: 'req', label: '握手' }, { id: 'ws', label: `消息 (${detail.wsMessages.length})` }]
      : kind === 'tcp'
        ? [{ id: 'raw', label: '原始数据' }]
        : [{ id: 'req', label: '请求' }, { id: 'res', label: '响应' }];

  return (
    <>
      <div className="detail-head">
        <div className="url">
          {kind === 'http' && <MethodBadge method={detail.method} />} {detail.url}
          <CopyButton
            text={detail.url}
            label={
              <>
                <IconCopy size={12} />
                复制 URL
              </>
            }
            title="复制完整 URL"
          />
        </div>
        <div className="meta">
          {detail.status != null && (
            <span>状态 <b className={`status-cell ${statusColor(detail.status)}`}>{detail.status}</b></span>
          )}
          {detail.contentType && <span>类型 <b>{detail.contentType.split(';')[0]}</b></span>}
          {detail.decoded && <span className="badge decode">已解压 {detail.decoded}</span>}
          <span>↑ {formatBytes(detail.bytesUp)} / ↓ {formatBytes(detail.bytesDown)}</span>
          <span title="从收到请求到响应体读完的耗时">
            耗时 <b>{formatMs(detail.durationMs)}</b>
          </span>
          <span>ID #{detail.id}</span>
          {detail.error && <span className="badge err">{detail.error}</span>}
        </div>
      </div>
      <div className="tabs">
        {tabs.map((t) => (
          <button key={t.id} className={tab === t.id ? 'active' : ''} onClick={() => setTab(t.id)}>
            {t.label}
          </button>
        ))}
      </div>
      <div className="detail-body">
        {tab === 'req' && (
          <>
            <div className="sub-title">请求头</div>
            <HeadersTable headers={detail.reqHeaders} />
            {kind === 'http' && (
              <BodyView
                text={detail.reqBody}
                raw={detail.reqBodyRaw}
                decoded={null}
                truncated={detail.reqTruncated}
                label="请求体"
                url={detail.url}
                contentType={detail.contentType}
                entryId={detail.id}
                side="req"
                bodyTruncated={detail.reqTruncated}
              />
            )}
          </>
        )}
        {tab === 'res' && (
          <>
            <div className="sub-title">响应头</div>
            <HeadersTable headers={detail.respHeaders ?? []} />
            <BodyView
              text={detail.respDecoded ?? detail.respBody}
              raw={detail.respDecodedRaw ?? detail.respBodyRaw}
              decoded={detail.decoded}
              truncated={detail.respTruncated}
              label={detail.decoded ? `响应体（已从 ${detail.decoded} 解压）` : '响应体'}
              url={detail.url}
              contentType={detail.contentType}
              entryId={detail.id}
              side="resp"
              bodyTruncated={detail.respTruncated}
              rangeStart={rangeStartOf(detail.reqHeaders)}
            />
          </>
        )}
        {tab === 'ws' && (
          <>
            {!detail.done && <div className="sub-title" style={{ color: 'var(--accent)' }}>● 连接进行中，消息实时追加…</div>}
            {detail.done && <div className="sub-title">连接已关闭</div>}
            <div className="ws-toolbar">
              <span className="sub-title" style={{ margin: 0 }}>
                共 {detail.wsMessages.length} 条消息
              </span>
              <CopyButton
                label="复制全部"
                title="按顺序复制全部消息（含方向、类型、大小）"
                disabled={detail.wsMessages.length === 0}
                text={detail.wsMessages
                  .map(
                    (m) =>
                      `[${formatTime(m.ts)}] ${m.dir === 'c2s' ? '↑ 客户端 → 服务器' : '↓ 服务器 → 客户端'} ` +
                      `${m.kind} · ${m.size} B${m.truncated ? '（内容已截断）' : ''}\n${m.data ?? ''}`,
                  )
                  .join('\n\n')}
              />
            </div>
            <div className="ws-list">
              {detail.wsMessages.length === 0 && <div className="sub-title">（暂无消息）</div>}
              {detail.wsMessages.map((m, i) => (
                <div key={i} className={`ws-msg ${m.dir}`}>
                  <div className="ws-meta">
                    <span className="dir-tag">{m.dir === 'c2s' ? '↑ 客户端 → 服务器' : '↓ 服务器 → 客户端'}</span>
                    <span>{m.kind}</span>
                    <span title="payload 原始字节数">{formatBytes(m.size)}</span>
                    {m.data != null && (
                      <span title="可展示文本的字符数（一个汉字算 1 字符）">{`${m.data.length} 字符`}</span>
                    )}
                    {m.truncated && (
                      <span style={{ color: 'var(--accent)' }} title="连接文本额度已用尽，本条只保留了前面部分">
                        内容已截断
                      </span>
                    )}
                    <span>{formatTime(m.ts)}</span>
                    <CopyButton
                      text={m.data ?? ''}
                      disabled={m.data == null}
                      title={m.data == null ? '该帧没有文本内容（二进制 / 控制帧）' : '复制本条消息文本'}
                    />
                  </div>
                  {m.data != null && <pre>{m.data}</pre>}
                </div>
              ))}
            </div>
          </>
        )}
        {tab === 'raw' && (
          <>
            {kind === 'tcp' && detail.reqHeaders.length > 0 && (
              <>
                <div className="sub-title">连接信息</div>
                <HeadersTable headers={detail.reqHeaders} />
              </>
            )}
            <div className="sub-title">首包十六进制预览</div>
            <pre className="body-view hex-view">{detail.tcpHex ?? '（无数据）'}</pre>
            <div className="sub-title">流量统计</div>
            <div className="sub-title">
              上行 <b>{formatBytes(detail.bytesUp)}</b> · 下行 <b>{formatBytes(detail.bytesDown)}</b>
            </div>
          </>
        )}
      </div>
    </>
  );
}
