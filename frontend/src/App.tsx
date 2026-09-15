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
  clearEntries,
  emptyFacets,
  emptyFilters,
  fetchDetail,
  fetchEntries,
  fetchFacets,
  fetchSysProxy,
  filtersToQuery,
  formatBytes,
  formatTime,
  groupFilterable,
  groupKeyOf,
  groupKeyToFilter,
  groupLabelOf,
  hasAnyFilter,
  optionLabel,
  resourceMeta,
  setSysProxy,
  statusColor,
  tryPrettyJson,
} from './api';
import { applyTheme, getStoredTheme, watchSystemTheme, Theme } from './theme';

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
      <button className={theme === 'light' ? 'active' : ''} onClick={() => pick('light')} title="浅色">☀️</button>
      <button className={theme === 'dark' ? 'active' : ''} onClick={() => pick('dark')} title="深色">🌙</button>
      <button className={theme === 'system' ? 'active' : ''} onClick={() => pick('system')} title="跟随系统">💻</button>
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
    <div className="ms" ref={boxRef} style={{ width }}>
      <button
        type="button"
        className={`ms-btn${selected.length ? ' active' : ''}${open ? ' open' : ''}`}
        onClick={() => setOpen((o) => !o)}
        title={selected.length ? selected.map((v) => shortOf(options, v)).join(', ') : label}
      >
        <span className="ms-text">{summary}</span>
        <span className="ms-caret">▾</span>
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
                    <span className="ms-check">{on ? '✓' : ''}</span>
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
    refreshSysProxy();
    const t = setInterval(refreshSysProxy, 5000);
    return () => clearInterval(t);
  }, [refreshSysProxy]);

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
  };

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
          {live ? '实时连接中' : '未连接'}
        </span>
        <div className="spacer" />
        {pendingCount > 0 && (
          <button className="btn primary" onClick={resume}>
            {pendingCount} 条新记录，点击加载
          </button>
        )}
        <button className={`btn ${paused ? 'active' : ''}`} onClick={() => (paused ? resume() : setPaused(true))}>
          {paused ? '▶ 恢复' : '⏸ 暂停'}
        </button>
        <button className="btn danger" onClick={onClear}>🗑 清空</button>
        {sysProxy?.supported && (
          <button
            className={`btn ${sysProxy.active ? 'primary' : ''}`}
            disabled={sysBusy}
            onClick={toggleSysProxy}
            title={
              sysProxy.active
                ? '点击关闭系统代理并恢复直连'
                : `一键把系统 HTTP/HTTPS 代理指向 127.0.0.1:${sysProxy.port}`
            }
          >
            {sysProxy.active ? '🌐 系统代理 已开启' : '🌐 系统代理 已关闭'}
          </button>
        )}
        <a className="btn" href={`/api/export?format=json&${exportQuery}`} target="_blank" rel="noreferrer">
          导出 JSON
        </a>
        <a className="btn" href={`/api/export?format=har&${exportQuery}`} target="_blank" rel="noreferrer">
          导出 HAR
        </a>
        <a className="btn" href="/api/ca.crt" download="miniproxy-ca.crt" title="下载 CA 证书并安装到系统/浏览器以解密 HTTPS">
          🔐 CA 证书
        </a>
        <ThemeToggle />
      </header>

      <div className="toolbar">
        <div className={`search-wrap${qInput ? ' has-value' : ''}`}>
          <span className="search-icon" aria-hidden="true">🔍</span>
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
              ×
            </button>
          )}
        </div>
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
                ×
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

      <div className="main">
        <section className="list-pane">
          <div className="list-meta">
            共 {entries.length} 条{hasAnyFilter(filters) ? ' · 已筛选' : ''}
            {groupBy !== 'none' &&
              ` · ${GROUP_DIMS.find((d) => d.value === groupBy)?.label ?? ''}（${groups?.length ?? 0} 组）`}
          </div>
          <div className="list-scroll">
            {entries.length === 0 ? (
              <div className="empty-state">
                <div style={{ fontSize: 32 }}>📡</div>
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
                      <span className="gh-caret">{isCollapsed ? '▸' : '▾'}</span>
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
                          {on ? '✓ 已筛选' : '只看此组'}
                        </button>
                      )}
                    </div>
                    {!isCollapsed && <EntryTable items={items} selectedId={selectedId} onSelect={setSelectedId} />}
                  </React.Fragment>
                );
              })
            ) : (
              <EntryTable items={entries} selectedId={selectedId} onSelect={setSelectedId} />
            )}
          </div>
        </section>

        <section className="detail-pane">
          {detail ? (
            <Detail detail={detail} tab={tab} setTab={setTab} />
          ) : (
            <div className="empty-state">
              <div style={{ fontSize: 28 }}>🔍</div>
              <div>选择左侧任意记录查看详情</div>
            </div>
          )}
        </section>
      </div>
    </div>
  );
}

/* ---------------- 列表表格 ---------------- */
function EntryTable({
  items,
  selectedId,
  onSelect,
}: {
  items: EntrySummary[];
  selectedId: number | null;
  onSelect: (id: number) => void;
}) {
  return (
    <table className="entries">
      <colgroup>
        <col className="col-method" />
        <col className="col-status" />
        <col />
        <col className="col-size" />
        <col className="col-time" />
      </colgroup>
      <thead>
        <tr>
          <th>方法</th>
          <th>状态</th>
          <th>URL</th>
          <th>大小</th>
          <th>时间</th>
        </tr>
      </thead>
      <tbody>
        {items.map((e) => (
          <tr
            key={`${e.id}-${e.wsMessages}-${e.done}`}
            className={`row ${selectedId === e.id ? 'selected' : ''}`}
            onClick={() => onSelect(e.id)}
          >
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
            <td title={`↑ ${formatBytes(e.bytesUp)} / ↓ ${formatBytes(e.bytesDown)}`}>
              {e.kind === 'tcp'
                ? `↑${formatBytes(e.bytesUp)} ↓${formatBytes(e.bytesDown)}`
                : formatBytes(e.respSize || e.bytesDown)}
            </td>
            <td>{formatTime(e.ts)}</td>
          </tr>
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

function BodyView({
  text,
  decoded,
  truncated,
  label,
}: {
  text: string | null;
  decoded: string | null;
  truncated: boolean;
  label?: string;
}) {
  const pretty = tryPrettyJson(text);
  const shown = pretty ?? text ?? '';
  return (
    <>
      {label && <div className="sub-title">{label}</div>}
      {(decoded || truncated) && (
        <div style={{ marginBottom: 8 }}>
          {decoded && <span className="badge decode">已解压：{decoded}</span>}
          {truncated && <span className="badge warn">内容过大，已截断</span>}
          {pretty && <span className="badge">JSON 已格式化</span>}
        </div>
      )}
      {text ? <pre className="body-view">{shown}</pre> : <div className="sub-title">（空）</div>}
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
        </div>
        <div className="meta">
          {detail.status != null && (
            <span>状态 <b className={`status-cell ${statusColor(detail.status)}`}>{detail.status}</b></span>
          )}
          {detail.contentType && <span>类型 <b>{detail.contentType.split(';')[0]}</b></span>}
          {detail.decoded && <span className="badge decode">已解压 {detail.decoded}</span>}
          <span>↑ {formatBytes(detail.bytesUp)} / ↓ {formatBytes(detail.bytesDown)}</span>
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
            {kind === 'http' && <BodyView text={detail.reqBody} decoded={null} truncated={detail.reqTruncated} label="请求体" />}
          </>
        )}
        {tab === 'res' && (
          <>
            <div className="sub-title">响应头</div>
            <HeadersTable headers={detail.respHeaders ?? []} />
            <BodyView
              text={detail.respDecoded ?? detail.respBody}
              decoded={detail.decoded}
              truncated={detail.respTruncated}
              label={detail.decoded ? `响应体（已从 ${detail.decoded} 解压）` : '响应体'}
            />
          </>
        )}
        {tab === 'ws' && (
          <>
            {!detail.done && <div className="sub-title" style={{ color: 'var(--accent)' }}>● 连接进行中，消息实时追加…</div>}
            {detail.done && <div className="sub-title">连接已关闭</div>}
            <div className="ws-list">
              {detail.wsMessages.length === 0 && <div className="sub-title">（暂无消息）</div>}
              {detail.wsMessages.map((m, i) => (
                <div key={i} className={`ws-msg ${m.dir}`}>
                  <div className="ws-meta">
                    <span className="dir-tag">{m.dir === 'c2s' ? '↑ 客户端 → 服务器' : '↓ 服务器 → 客户端'}</span>
                    <span>{m.kind}</span>
                    <span>{formatBytes(m.size)}</span>
                    <span>{formatTime(m.ts)}</span>
                  </div>
                  {m.data != null && <pre>{m.data}</pre>}
                </div>
              ))}
            </div>
          </>
        )}
        {tab === 'raw' && (
          <>
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
