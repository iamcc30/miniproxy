// API 类型与请求封装

export interface EntrySummary {
  id: number;
  ts: number;
  kind: 'http' | 'ws' | 'tcp';
  resourceType: string;
  method: string;
  url: string;
  host: string;
  /** 站点（主域名）：api.example.com 与 cdn.example.com 同属 example.com */
  site: string;
  /** 发起请求的客户端进程名（按应用分组/筛选用），可能为 null */
  client: string | null;
  status: number | null;
  contentType: string | null;
  encoding: string | null;
  reqSize: number;
  respSize: number;
  bytesUp: number;
  bytesDown: number;
  wsMessages: number;
  wsClosed: boolean;
  done: boolean;
  error: string | null;
  /** 对端 IP（识别局域网设备，如手机抓包） */
  clientIp?: string;
  /** 关键词仅命中请求/响应内容时由服务端置位（用于列表标注「内容匹配」） */
  qInContent?: boolean;
}

export interface WsMessage {
  dir: 'c2s' | 's2c';
  kind: string;
  size: number;
  data: string | null;
  ts: number;
}

/** 单侧正文的「原始字节」视图（base64 原文 + 原始大小 + 是否因过大被截断）。 */
export interface RawView {
  b64: string;
  size: number;
  truncated: boolean;
}

export interface EntryDetail extends EntrySummary {
  reqHeaders: [string, string][];
  reqBody: string | null;
  reqBodyRaw: RawView | null;
  reqTruncated: boolean;
  respHeaders: [string, string][] | null;
  respBody: string | null;
  respBodyRaw: RawView | null;
  respDecoded: string | null;
  respDecodedRaw: RawView | null;
  decoded: string | null;
  respTruncated: boolean;
  wsMessages: WsMessage[];
  wsClosed: boolean;
  tcpHex: string | null;
  durationMs: number;
}

export interface Filters {
  q: string;
  kinds: string[];
  rtypes: string[];
  methods: string[];
  statuses: string[];
  hosts: string[];
  sites: string[];
  apps: string[];
}

export function emptyFilters(): Filters {
  return {
    q: '',
    kinds: [],
    rtypes: [],
    methods: [],
    statuses: [],
    hosts: [],
    sites: [],
    apps: [],
  };
}

/** 任意筛选组是否有选中值 */
export function hasAnyFilter(f: Filters): boolean {
  return !!(
    f.q ||
    f.kinds.length ||
    f.rtypes.length ||
    f.methods.length ||
    f.statuses.length ||
    f.hosts.length ||
    f.sites.length ||
    f.apps.length
  );
}

export function filtersToQuery(f: Filters): string {
  const p = new URLSearchParams();
  if (f.q) p.set('q', f.q);
  if (f.kinds.length) p.set('kind', f.kinds.join(','));
  if (f.rtypes.length) p.set('type', f.rtypes.join(','));
  if (f.methods.length) p.set('method', f.methods.join(','));
  if (f.statuses.length) p.set('status', f.statuses.join(','));
  if (f.hosts.length) p.set('host', f.hosts.join(','));
  if (f.sites.length) p.set('site', f.sites.join(','));
  if (f.apps.length) p.set('app', f.apps.join(','));
  p.set('limit', '1000');
  return p.toString();
}

/* ---------------- 筛选选项元数据（多选下拉共用） ---------------- */

export interface FilterOption {
  value: string;
  label: string;
  /** 按钮内的紧凑文案（缺省用 label） */
  short?: string;
  count?: number;
}

export const KIND_OPTIONS: FilterOption[] = [
  { value: 'http', label: 'HTTP/HTTPS' },
  { value: 'ws', label: 'WebSocket' },
  { value: 'tcp', label: 'TCP' },
];

export const METHOD_OPTIONS: FilterOption[] = [
  'GET',
  'POST',
  'PUT',
  'DELETE',
  'PATCH',
  'HEAD',
  'OPTIONS',
].map((m) => ({ value: m, label: m }));

export const STATUS_OPTIONS: FilterOption[] = [
  { value: '2', label: '2xx 成功' },
  { value: '3', label: '3xx 重定向' },
  { value: '4', label: '4xx 客户端错误' },
  { value: '5', label: '5xx 服务端错误' },
];

export function optionLabel(options: FilterOption[], value: string): string {
  return options.find((o) => o.value === value)?.label ?? value;
}

/** 资源类型（文件类型）元数据：过滤下拉与列表标签共用。 */
export const RESOURCE_TYPES: { value: string; label: string; short: string }[] = [
  { value: 'document', label: '文档 (HTML)', short: 'HTML' },
  { value: 'script', label: '脚本 (JS)', short: 'JS' },
  { value: 'css', label: '样式表 (CSS)', short: 'CSS' },
  { value: 'json', label: '数据 (JSON)', short: 'JSON' },
  { value: 'xml', label: '数据 (XML)', short: 'XML' },
  { value: 'text', label: '纯文本', short: 'TXT' },
  { value: 'form', label: '表单', short: 'FORM' },
  { value: 'image', label: '图片', short: 'IMG' },
  { value: 'font', label: '字体', short: 'FONT' },
  { value: 'media', label: '音视频', short: 'MEDIA' },
  { value: 'wasm', label: 'WASM', short: 'WASM' },
  { value: 'ws', label: 'WebSocket', short: 'WS' },
  { value: 'tcp', label: 'TCP 隧道', short: 'TCP' },
  { value: 'other', label: '其他', short: '—' },
];

export function resourceMeta(t: string): { label: string; short: string } {
  return RESOURCE_TYPES.find((r) => r.value === t) ?? { label: '其他', short: '—' };
}

/** 类型选项（供多选下拉使用，short 作为紧凑标签） */
export const RESOURCE_OPTIONS: FilterOption[] = RESOURCE_TYPES.map((r) => ({
  value: r.value,
  label: r.label,
  short: r.short,
}));

export interface TypeInfo {
  type: string;
  count: number;
}

/* ---------------- 分组维度 ---------------- */

/** 分组维度：决定列表按什么归组，同时决定「按组筛选」写到哪个筛选字段。 */
export type GroupDim = 'none' | 'host' | 'site' | 'app' | 'kind' | 'type' | 'status';

export const GROUP_DIMS: FilterOption[] = [
  { value: 'none', label: '不分组' },
  { value: 'host', label: '按域名' },
  { value: 'site', label: '按站点' },
  { value: 'app', label: '按应用' },
  { value: 'kind', label: '按协议' },
  { value: 'type', label: '按文件类型' },
  { value: 'status', label: '按状态码' },
];

/** 维度 -> 筛选字段键（用于「只看此组」按钮） */
export const DIM_FILTER_KEY = {
  host: 'hosts',
  site: 'sites',
  app: 'apps',
  kind: 'kinds',
  type: 'rtypes',
  status: 'statuses',
} as const;

/** 某个条目在指定维度下的分组键 */
export function groupKeyOf(e: EntrySummary, dim: GroupDim): string {
  switch (dim) {
    case 'host':
      return e.host || '（未知域名）';
    case 'site':
      return e.site || '（未知站点）';
    case 'app':
      return e.client || '未知应用';
    case 'kind':
      return e.kind;
    case 'type':
      return e.resourceType;
    case 'status':
      if (e.status == null) return e.error ? 'err' : 'pending';
      return String(Math.floor(e.status / 100));
    default:
      return '';
  }
}

/** 分组键的展示文案 */
export function groupLabelOf(key: string, dim: GroupDim): string {
  switch (dim) {
    case 'kind':
      return KIND_OPTIONS.find((o) => o.value === key)?.label ?? key;
    case 'type':
      return resourceMeta(key).label;
    case 'status':
      if (key === 'err') return '请求失败';
      if (key === 'pending') return '进行中 / 无状态';
      return `${key}xx ${STATUS_OPTIONS.find((o) => o.value === key)?.label.replace(/^\dxx /, '') ?? ''}`.trim();
    default:
      return key;
  }
}

/** 分组键 -> 写入筛选条件的值（域名/应用维度大小写不敏感，统一小写） */
export function groupKeyToFilter(key: string, dim: GroupDim): string {
  return dim === 'host' || dim === 'app' ? key.toLowerCase() : key;
}

/** 该分组键能否直接用于筛选（status 维度的 err/pending、未知应用等不能） */
export function groupFilterable(key: string, dim: GroupDim): boolean {
  if (dim === 'status') return ['2', '3', '4', '5'].includes(key);
  if (key.startsWith('（') || key === '未知应用' || key === '') return false;
  return dim !== 'none';
}

/* ---------------- Facet（各维度候选值 + 计数） ---------------- */

export interface FacetItem {
  value: string;
  count: number;
}

export interface Facets {
  host: FacetItem[];
  site: FacetItem[];
  app: FacetItem[];
  kind: FacetItem[];
  type: FacetItem[];
  status: FacetItem[];
}

export function emptyFacets(): Facets {
  return { host: [], site: [], app: [], kind: [], type: [], status: [] };
}

export async function fetchFacets(): Promise<Facets> {
  const r = await fetch('/api/facets');
  const d = await r.json();
  return d.facets ?? emptyFacets();
}

export async function fetchEntries(f: Filters): Promise<{ items: EntrySummary[]; total: number }> {
  const r = await fetch(`/api/entries?${filtersToQuery(f)}`);
  return r.json();
}

export interface AppInfo {
  name: string;
  apiPort: number;
  proxyPort: number;
  /** 本机局域网出口 IP（用于手机等局域网设备访问） */
  lanIp: string | null;
  /** 供局域网设备下载 CA 证书的完整 URL */
  caUrl: string;
}

export async function fetchInfo(): Promise<AppInfo> {
  const r = await fetch('/api/info');
  return r.json();
}

export async function fetchDetail(id: number): Promise<EntryDetail> {
  const r = await fetch(`/api/entries/${id}`);
  return r.json();
}

export async function clearEntries(): Promise<void> {
  await fetch('/api/entries', { method: 'DELETE' });
}

export interface SysProxyService {
  name: string;
  http: [string, number] | null;
  https: [string, number] | null;
  socks: [string, number] | null;
}

export interface SysProxyStatus {
  supported: boolean;
  active?: boolean;
  port?: number;
  services?: SysProxyService[];
  error?: string;
  hint?: string;
}

export async function fetchSysProxy(): Promise<SysProxyStatus> {
  const r = await fetch('/api/system-proxy');
  return r.json();
}

export async function setSysProxy(enable: boolean): Promise<SysProxyStatus> {
  const r = await fetch(`/api/system-proxy/${enable ? 'enable' : 'disable'}`, {
    method: 'POST',
  });
  return r.json();
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(2)} MB`;
}

export function formatTime(ts: number): string {
  const d = new Date(ts);
  const p = (x: number, l = 2) => String(x).padStart(l, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

export function tryPrettyJson(text: string | null): string | null {
  if (!text) return null;
  const t = text.trim();
  if (!(t.startsWith('{') || t.startsWith('['))) return null;
  try {
    return JSON.stringify(JSON.parse(t), null, 2);
  } catch {
    return null;
  }
}

export function statusColor(status: number | null): string {
  if (status == null) return 'st-pending';
  if (status < 300) return 'st-2xx';
  if (status < 400) return 'st-3xx';
  if (status < 500) return 'st-4xx';
  return 'st-5xx';
}

/* ---------------- 原始数据视图（hex / base64 / 下载） ---------------- */

/** base64 原文 -> 字节数组 */
export function b64ToBytes(b64: string): Uint8Array {
  try {
    const bin = atob(b64);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  } catch {
    return new Uint8Array(0);
  }
}

/** 字节数组 -> 经典 hex dump（偏移量 + 16 字节一行 + ASCII 列） */
export function toHexDump(bytes: Uint8Array, maxBytes = 128 * 1024): string {
  const n = Math.min(bytes.length, maxBytes);
  const pad = (s: string, w: number) => s.padEnd(w, ' ');
  const lines: string[] = [];
  for (let off = 0; off < n; off += 16) {
    const chunk = bytes.subarray(off, Math.min(off + 16, n));
    const hexParts: string[] = [];
    let ascii = '';
    for (let i = 0; i < 16; i++) {
      if (i < chunk.length) {
        const b = chunk[i];
        hexParts.push(b.toString(16).padStart(2, '0'));
        ascii += b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.';
      } else {
        hexParts.push('  ');
      }
      if (i === 7) hexParts.push('');
    }
    lines.push(`${off.toString(16).padStart(8, '0')}  ${pad(hexParts.join(' '), 51)} |${ascii}|`);
  }
  if (bytes.length > n) {
    lines.push(`… 共 ${bytes.length} 字节，此处仅显示前 ${n} 字节`);
  }
  return lines.join('\n');
}

/** base64 原文按列宽折行，便于阅读与复制 */
export function wrapBase64(b64: string, width = 76): string {
  const out: string[] = [];
  for (let i = 0; i < b64.length; i += width) out.push(b64.slice(i, i + width));
  return out.join('\n');
}

/**
 * 判断文本视图是否已是「乱码」：二进制内容经 UTF-8 lossy 转码会得到大量替换字符（U+FFFD），
 * 这类内容应默认走原始数据（十六进制）视图。
 */
export function looksBinary(text: string | null): boolean {
  if (!text) return false;
  if (text.includes('\u0000')) return true;
  const len = text.length;
  if (len < 8) return false;
  let bad = 0;
  let ctrl = 0;
  for (let i = 0; i < len; i++) {
    const c = text.charCodeAt(i);
    if (c === 0xfffd) bad++;
    else if (c < 0x09 || (c > 0x0d && c < 0x20)) ctrl++;
  }
  return bad / len > 0.02 || ctrl / len > 0.05;
}

/** 触发浏览器下载原始字节 */
export function downloadBytes(bytes: Uint8Array, filename: string): void {
  const ab = new ArrayBuffer(bytes.byteLength);
  new Uint8Array(ab).set(bytes);
  const url = URL.createObjectURL(new Blob([ab], { type: 'application/octet-stream' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** 由 URL 推导一个安全的下载文件名 */
export function suggestFilename(url: string, fallback = 'body.bin'): string {
  try {
    const u = new URL(url);
    const last = u.pathname.split('/').filter(Boolean).pop();
    const name = last ? decodeURIComponent(last) : u.host;
    return name.replace(/[\\/:*?"<>|]/g, '_').slice(0, 80) || fallback;
  } catch {
    return fallback;
  }
}
