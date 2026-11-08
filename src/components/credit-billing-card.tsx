import { useCallback, useEffect, useMemo, useState, type ReactElement } from "react";
import {
  CircleAlert,
  Loader2,
  RefreshCw,
  RotateCcw,
  Settings2,
  Sigma,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import * as api from "@/lib/api";
import type {
  CreditBilling,
  CreditBillingBucket,
  CreditMultipliers,
  CreditOfficialUsage,
} from "@/lib/types";

type WindowKey = "official" | "30d" | "7d" | "month" | "all";
type Basis = "gross" | "billable";
type Verdict = "high" | "low" | "aligned" | "insufficient" | "free" | "unknown";

interface BucketAgg {
  model: string;
  records: number;
  credit: number;
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

interface RateRow extends BucketAgg {
  billable: number;
  gross: number;
  tokens: number;
  perMillion: number | null;
  cacheHitRate: number | null;
  multiplier: number | null;
  basePrice: number | null;
  verdict: Verdict;
}

interface ReconcileRow {
  key: string;
  label: string;
  officialCredit: number | null;
  localCredit: number | null;
  diff: number | null;
}

const BASIS_OPTIONS: { key: Basis; label: string; hint: string }[] = [
  { key: "gross", label: "全量 token", hint: "输入 + 输出 + 缓存写入；缓存命中按原价计入" },
  { key: "billable", label: "有效 token", hint: "未缓存输入 + 输出；缓存命中不计价" },
];

function dateKey(date: Date): string {
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

function daysAgoKey(days: number): string {
  const date = new Date();
  date.setHours(12, 0, 0, 0);
  date.setDate(date.getDate() - days);
  return dateKey(date);
}

function monthStartKey(): string {
  const date = new Date();
  return dateKey(new Date(date.getFullYear(), date.getMonth(), 1));
}

function windowBounds(
  key: WindowKey,
  official?: CreditOfficialUsage,
): { from: string | null; to: string | null; label: string } {
  switch (key) {
    case "official":
      return {
        from: official?.rangeStart || null,
        to: official?.rangeEnd || null,
        label: official ? `${official.rangeStart} ~ ${official.rangeEnd}` : "官方区间",
      };
    case "7d":
      return { from: daysAgoKey(6), to: daysAgoKey(0), label: "近 7 天" };
    case "month":
      return { from: monthStartKey(), to: daysAgoKey(0), label: "本月" };
    case "all":
      return { from: null, to: null, label: "全部记录" };
    case "30d":
    default:
      return { from: daysAgoKey(29), to: daysAgoKey(0), label: "近 30 天" };
  }
}

/** 官方账单里的模型名与本地日志里的 id 大小写/连字符可能不同，统一成字母数字再对齐。 */
function normalizeModel(name: string): string {
  const trimmed = name.trim().toLowerCase();
  if (!trimmed || trimmed === "—" || trimmed === "未知模型" || trimmed === "unknown") {
    return "unknown";
  }
  return trimmed.replace(/[^a-z0-9]/g, "");
}

function lookupMultiplier(model: string, table: Record<string, number>): number | null {
  if (Object.prototype.hasOwnProperty.call(table, model)) return table[model];
  const key = normalizeModel(model);
  if (key === "unknown") return null;
  for (const [name, rate] of Object.entries(table)) {
    if (normalizeModel(name) === key) return rate;
  }
  return null;
}

function accumulate(map: Map<string, BucketAgg>, row: CreditBillingBucket): void {
  const current = map.get(row.model) ?? {
    model: row.model,
    records: 0,
    credit: 0,
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
  };
  current.records += row.records;
  current.credit += row.credit;
  current.input += row.input;
  current.output += row.output;
  current.cacheRead += row.cacheRead;
  current.cacheWrite += row.cacheWrite;
  map.set(row.model, current);
}

function aggregate(billing: CreditBilling, from: string | null, to: string | null): BucketAgg[] {
  const map = new Map<string, BucketAgg>();
  for (const day of billing.days ?? []) {
    if (from && day.date < from) continue;
    if (to && day.date > to) continue;
    for (const row of day.models ?? []) accumulate(map, row);
  }
  return [...map.values()].sort(
    (left, right) => right.credit - left.credit || left.model.localeCompare(right.model),
  );
}

function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

function formatNumber(value: number | null | undefined, digits = 2): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return "—";
  return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: digits }).format(value);
}

function formatTokens(value: number): string {
  if (value >= 1e9) return `${(value / 1e9).toFixed(2)}B`;
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  if (value >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
  return formatNumber(value, 0);
}

function verdictBadge(verdict: Verdict): ReactElement {
  switch (verdict) {
    case "high":
      return <Badge variant="destructive">明显偏高</Badge>;
    case "low":
      return <Badge variant="warning">明显偏低</Badge>;
    case "aligned":
      return <Badge variant="success">口径一致</Badge>;
    case "free":
      return <Badge variant="secondary">免费模型</Badge>;
    case "unknown":
      return <Badge variant="outline">倍率未知</Badge>;
    default:
      return <Badge variant="outline">样本不足</Badge>;
  }
}

function Metric({
  label,
  value,
  hint,
}: {
  label: string;
  value: string;
  hint?: string;
}) {
  return (
    <div className="min-w-0 rounded-lg border bg-background/60 px-3 py-2.5">
      <div className="truncate text-[11px] text-muted-foreground" title={hint}>
        {label}
      </div>
      <div className="mt-1 truncate text-[15px] font-semibold tabular-nums">{value}</div>
    </div>
  );
}

/** 积分计费核算：本地实扣积分 ÷ token → 每百万单价，再按官方倍率归一后找离群模型。 */
export function CreditBillingCard({ official }: { official?: CreditOfficialUsage }) {
  const [billing, setBilling] = useState<CreditBilling | null>(null);
  const [rateTable, setRateTable] = useState<CreditMultipliers | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [windowKey, setWindowKey] = useState<WindowKey>(official ? "official" : "30d");
  const [basis, setBasis] = useState<Basis>("gross");
  const [editorOpen, setEditorOpen] = useState(false);
  const [draft, setDraft] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);
  const [editorError, setEditorError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [ledger, rates] = await Promise.all([
        api.getCreditBilling(),
        api.getCreditMultipliers(),
      ]);
      setBilling(ledger);
      setRateTable(rates);
    } catch (cause) {
      setError(api.asError(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // 官方数据后来才同步成功时，默认切到官方区间，便于直接交叉核对。
  useEffect(() => {
    if (official) setWindowKey((current) => (current === "30d" ? "official" : current));
  }, [official]);

  const windowOptions: { key: WindowKey; label: string }[] = [
    ...(official ? [{ key: "official" as const, label: "官方区间" }] : []),
    { key: "7d", label: "近 7 天" },
    { key: "30d", label: "近 30 天" },
    { key: "month", label: "本月" },
    { key: "all", label: "全部" },
  ];

  const bounds = useMemo(() => windowBounds(windowKey, official), [windowKey, official]);
  const multipliers = rateTable?.multipliers ?? {};

  const rows = useMemo<RateRow[]>(() => {
    if (!billing) return [];
    const base = aggregate(billing, bounds.from, bounds.to).map((item): RateRow => {
      const billable = Math.max(0, item.input - item.cacheRead) + item.output;
      const gross = item.input + item.output + item.cacheWrite;
      const tokens = basis === "gross" ? gross : billable;
      const perMillion = tokens > 0 ? (item.credit / tokens) * 1e6 : null;
      const multiplier = lookupMultiplier(item.model, multipliers);
      const basePrice =
        multiplier !== null && multiplier > 0 && perMillion !== null
          ? perMillion / multiplier
          : null;
      return {
        ...item,
        billable,
        gross,
        tokens,
        perMillion,
        cacheHitRate: item.input > 0 ? item.cacheRead / item.input : null,
        multiplier,
        basePrice,
        verdict: "unknown",
      };
    });

    // 只在「有倍率、有消耗、样本量够」的模型之间比较，避免一两条小样本拉偏基准。
    const samples = base
      .filter((row) => row.basePrice !== null && row.credit > 0 && row.records >= 5)
      .map((row) => row.basePrice as number);
    const medianBase = samples.length >= 3 ? median(samples) : null;

    return base.map((row) => {
      let verdict: Verdict;
      if (row.multiplier === 0) verdict = "free";
      else if (row.multiplier === null) verdict = "unknown";
      else if (medianBase === null || row.basePrice === null || row.records < 5 || row.credit <= 0) {
        verdict = "insufficient";
      } else {
        const ratio = row.basePrice / medianBase;
        verdict = ratio >= 2 ? "high" : ratio <= 0.5 ? "low" : "aligned";
      }
      return { ...row, verdict };
    });
  }, [billing, bounds.from, bounds.to, basis, multipliers]);

  const summary = useMemo(() => {
    let credit = 0;
    let tokens = 0;
    let records = 0;
    for (const row of rows) {
      credit += row.credit;
      tokens += row.tokens;
      records += row.records;
    }
    return {
      credit,
      tokens,
      records,
      perMillion: tokens > 0 ? (credit / tokens) * 1e6 : null,
      medianBase: (() => {
        const samples = rows
          .filter((row) => row.basePrice !== null && row.records >= 5 && row.credit > 0)
          .map((row) => row.basePrice as number);
        return samples.length >= 3 ? median(samples) : null;
      })(),
    };
  }, [rows]);

  /** 官方账单 vs 本地记录：严格按官方 rangeStart..rangeEnd（含两端）取同一窗口。 */
  const reconcile = useMemo(() => {
    if (!billing || !official) return null;
    const officialMap = new Map<string, { label: string; credit: number }>();
    for (const item of official.models ?? []) {
      const key = normalizeModel(item.model || "");
      const current = officialMap.get(key) ?? { label: item.model || "未知模型", credit: 0 };
      current.credit += item.credit;
      officialMap.set(key, current);
    }
    const localMap = new Map<string, { label: string; credit: number }>();
    for (const row of aggregate(billing, official.rangeStart || null, official.rangeEnd || null)) {
      const key = normalizeModel(row.model);
      const current = localMap.get(key) ?? { label: row.model, credit: 0 };
      current.credit += row.credit;
      localMap.set(key, current);
    }
    const keys = new Set([...officialMap.keys(), ...localMap.keys()]);
    const items: ReconcileRow[] = [...keys].map((key) => {
      const left = officialMap.get(key);
      const right = localMap.get(key);
      const officialCredit = left ? left.credit : null;
      const localCredit = right ? right.credit : null;
      return {
        key,
        label: left?.label ?? right?.label ?? "未知模型",
        officialCredit,
        localCredit,
        diff:
          officialCredit !== null && localCredit !== null
            ? officialCredit - localCredit
            : null,
      };
    });
    items.sort((a, b) => Math.abs(b.diff ?? 0) - Math.abs(a.diff ?? 0));
    const officialTotal = items.reduce((sum, item) => sum + (item.officialCredit ?? 0), 0);
    const localTotal = items.reduce((sum, item) => sum + (item.localCredit ?? 0), 0);
    return { items, officialTotal, localTotal, diff: officialTotal - localTotal };
  }, [billing, official]);

  const openEditor = useCallback(() => {
    if (!rateTable) return;
    setDraft(
      Object.fromEntries(
        Object.entries(rateTable.multipliers).map(([name, rate]) => [name, String(rate)]),
      ),
    );
    setEditorError(null);
    setEditorOpen((current) => !current);
  }, [rateTable]);

  const saveEditor = useCallback(async () => {
    const table: Record<string, number> = {};
    for (const [name, raw] of Object.entries(draft)) {
      // `__newModel` / `__newRate` 是「新增行」的输入缓冲，不是倍率表条目。
      if (name.startsWith("__new")) continue;
      const key = name.trim();
      if (!key) continue;
      const value = Number(raw);
      if (!Number.isFinite(value) || value < 0 || value > 1000) {
        setEditorError(`「${key}」的倍率需为 0–1000 的数字`);
        return;
      }
      table[key] = value;
    }
    setSaving(true);
    setEditorError(null);
    try {
      const next = await api.saveCreditMultipliers(table);
      setRateTable(next);
      setEditorOpen(false);
    } catch (cause) {
      setEditorError(api.asError(cause));
    } finally {
      setSaving(false);
    }
  }, [draft]);

  // 演示模式下也能打开预览倍率表，只是不允许保存。
  const canEdit = rateTable !== null;

  return (
    <section className="min-w-0 space-y-2.5" aria-labelledby="credit-billing-title">
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
        <h2 id="credit-billing-title" className="text-[13px] font-medium leading-5">
          积分计费核算
        </h2>
        <Badge variant="outline" className="shrink-0">本地账本 · 可复核</Badge>
      </div>

      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardHeader className="gap-2 px-4 pt-3 pb-3 sm:px-5">
          <CardDescription className="text-xs leading-5">
            官方只公布了积分消耗倍率，没给基准单价。这里用本地会话逐条记录的
            <span className="font-medium text-foreground">实扣积分 ÷ token</span>
            反推每百万 token 单价，再按倍率归一，看有没有模型明显偏离同一基准。
          </CardDescription>
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
            <div className="flex max-w-full flex-wrap gap-1 rounded-lg bg-muted p-1" aria-label="核算时间范围">
              {windowOptions.map((option) => (
                <button
                  key={option.key}
                  type="button"
                  className={`cursor-pointer rounded-md px-2.5 py-1.5 text-xs transition-colors ${
                    windowKey === option.key
                      ? "bg-background font-medium text-foreground shadow-sm"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                  onClick={() => setWindowKey(option.key)}
                  aria-pressed={windowKey === option.key}
                >
                  {option.label}
                </button>
              ))}
            </div>
            <div className="flex flex-wrap items-center gap-1.5">
              <div className="flex flex-wrap gap-1 rounded-lg bg-muted p-1" aria-label="计费 token 口径">
                {BASIS_OPTIONS.map((option) => (
                  <button
                    key={option.key}
                    type="button"
                    title={option.hint}
                    className={`cursor-pointer rounded-md px-2.5 py-1.5 text-xs transition-colors ${
                      basis === option.key
                        ? "bg-background font-medium text-foreground shadow-sm"
                        : "text-muted-foreground hover:text-foreground"
                    }`}
                    onClick={() => setBasis(option.key)}
                    aria-pressed={basis === option.key}
                  >
                    {option.label}
                  </button>
                ))}
              </div>
              <Button variant="outline" size="sm" onClick={openEditor} disabled={!canEdit}>
                <Settings2 />
                倍率表
              </Button>
              <Button variant="outline" size="sm" onClick={() => void load()} disabled={loading}>
                {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                重算
              </Button>
            </div>
          </div>
        </CardHeader>

        <CardContent className="space-y-4 px-4 pb-4 sm:px-5">
          {error && (
            <Alert variant="destructive">
              <CircleAlert />
              <AlertTitle>积分账本读取失败</AlertTitle>
              <AlertDescription className="flex flex-wrap items-center gap-3">
                <span>{error}</span>
                <Button size="sm" variant="outline" onClick={() => void load()}>
                  重试
                </Button>
              </AlertDescription>
            </Alert>
          )}

          {loading && !billing ? (
            <div className="flex items-center gap-2 py-10 text-sm text-muted-foreground">
              <Loader2 className="animate-spin" />
              正在扫描本地会话里的实扣积分…
            </div>
          ) : billing ? (
            <>
              <div className="grid grid-cols-2 gap-2.5 sm:grid-cols-4">
                <Metric label={`${bounds.label} · 实扣积分`} value={formatNumber(summary.credit)} />
                <Metric
                  label={basis === "gross" ? "全量 token" : "有效 token"}
                  value={formatTokens(summary.tokens)}
                  hint={BASIS_OPTIONS.find((option) => option.key === basis)?.hint}
                />
                <Metric
                  label="积分 / 百万 token"
                  value={formatNumber(summary.perMillion)}
                  hint="所选窗口全部模型的加权平均"
                />
                <Metric
                  label="归一基准价中位数"
                  value={summary.medianBase === null ? "样本不足" : formatNumber(summary.medianBase)}
                  hint="单价 ÷ 官方倍率；倍率为 1.0 时每百万 token 的隐含积分"
                />
              </div>

              <Alert>
                <Sigma />
                <AlertTitle>当前口径：{basis === "gross" ? "全量 token" : "有效 token"}</AlertTitle>
                <AlertDescription className="text-xs leading-5">
                  {/* AlertDescription 自身是 grid：必须包成一个子节点，否则每个文本片段会各占一行。 */}
                  <span>
                    {basis === "gross"
                      ? "分母 = 输入 + 输出 + 缓存写入，缓存命中按原价计入。"
                      : "分母 = 未缓存输入 + 输出，缓存命中不计价。"}
                    官方没有公布缓存 token 的计价规则，两种口径都保留，切换后单基准价会整体变化；
                    <span className="font-medium">只有同一口径下的横向比较才有意义</span>。
                  </span>
                </AlertDescription>
              </Alert>

              {rows.length === 0 ? (
                <div className="rounded-lg border border-dashed px-4 py-10 text-center text-sm text-muted-foreground">
                  该时间范围内没有带实扣积分的本地记录。
                </div>
              ) : (
                <div className="min-w-0 overflow-x-auto rounded-lg border">
                  <table className="w-full min-w-[820px] border-collapse text-xs">
                    <thead>
                      <tr className="border-b bg-muted/50 text-muted-foreground">
                        <th className="px-3 py-2.5 text-left font-medium">模型</th>
                        <th className="px-3 py-2.5 text-right font-medium">调用</th>
                        <th className="px-3 py-2.5 text-right font-medium">实扣积分</th>
                        <th className="px-3 py-2.5 text-right font-medium">计费 token</th>
                        <th className="px-3 py-2.5 text-right font-medium">积分 / 百万</th>
                        <th className="px-3 py-2.5 text-right font-medium">官方倍率</th>
                        <th className="px-3 py-2.5 text-right font-medium">归一基准价</th>
                        <th className="px-3 py-2.5 text-left font-medium">判定</th>
                      </tr>
                    </thead>
                    <tbody>
                      {rows.map((row) => (
                        <tr key={row.model} className="border-b last:border-b-0">
                          <td className="max-w-[190px] truncate px-3 py-2.5 font-medium" title={row.model}>
                            {row.model}
                          </td>
                          <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                            {formatNumber(row.records, 0)}
                          </td>
                          <td className="px-3 py-2.5 text-right font-medium tabular-nums">
                            {formatNumber(row.credit)}
                          </td>
                          <td
                            className="px-3 py-2.5 text-right tabular-nums text-muted-foreground"
                            title={
                              row.cacheHitRate === null
                                ? undefined
                                : `缓存命中率 ${(row.cacheHitRate * 100).toFixed(1)}% · 有效 ${formatTokens(row.billable)} / 全量 ${formatTokens(row.gross)}`
                            }
                          >
                            {formatTokens(row.tokens)}
                          </td>
                          <td className="px-3 py-2.5 text-right font-semibold tabular-nums">
                            {formatNumber(row.perMillion)}
                          </td>
                          <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                            {row.multiplier === null ? "—" : `${formatNumber(row.multiplier)}x`}
                          </td>
                          <td className="px-3 py-2.5 text-right tabular-nums">
                            {formatNumber(row.basePrice)}
                          </td>
                          <td className="px-3 py-2.5">{verdictBadge(row.verdict)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}

              {editorOpen && rateTable && (
                <div className="space-y-3 rounded-lg border bg-muted/30 p-3">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="text-xs font-medium">
                      官方积分消耗倍率表
                      {rateTable.custom && <Badge variant="outline" className="ml-2">已自定义</Badge>}
                    </div>
                    <div className="flex flex-wrap gap-1.5">
                      <Button
                        size="sm"
                        variant="outline"
                        onClick={() =>
                          setDraft(
                            Object.fromEntries(
                              Object.entries(rateTable.defaults).map(([name, rate]) => [
                                name,
                                String(rate),
                              ]),
                            ),
                          )
                        }
                      >
                        <RotateCcw />
                        恢复默认
                      </Button>
                      <Button size="sm" variant="outline" onClick={() => setEditorOpen(false)}>
                        取消
                      </Button>
                      <Button
                        size="sm"
                        onClick={() => void saveEditor()}
                        disabled={saving || api.isDemoMode()}
                      >
                        {saving ? <Loader2 className="animate-spin" /> : null}
                        保存
                      </Button>
                    </div>
                  </div>
                  {editorError && (
                    <p className="text-xs text-destructive">{editorError}</p>
                  )}
                  <div className="grid max-h-[240px] gap-1.5 overflow-y-auto pr-1 sm:grid-cols-2">
                    {Object.entries(draft)
                      .filter(([name]) => !name.startsWith("__new"))
                      .map(([name, value]) => (
                      <div key={name} className="flex items-center gap-2">
                        <span className="min-w-0 flex-1 truncate text-xs" title={name}>{name}</span>
                        <Input
                          className="h-8 w-24 text-xs"
                          inputMode="decimal"
                          value={value}
                          aria-label={`${name} 倍率`}
                          onChange={(event) =>
                            setDraft((current) => ({ ...current, [name]: event.target.value }))
                          }
                        />
                      </div>
                    ))}
                  </div>
                  <div className="flex items-center gap-2">
                    <Input
                      className="h-8 flex-1 text-xs"
                      placeholder="新增模型 id，如 glm-5.3"
                      value={draft.__newModel ?? ""}
                      onChange={(event) =>
                        setDraft((current) => ({ ...current, __newModel: event.target.value }))
                      }
                    />
                    <Input
                      className="h-8 w-24 text-xs"
                      inputMode="decimal"
                      placeholder="倍率"
                      value={draft.__newRate ?? ""}
                      onChange={(event) =>
                        setDraft((current) => ({ ...current, __newRate: event.target.value }))
                      }
                    />
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() =>
                        setDraft((current) => {
                          const name = (current.__newModel ?? "").trim();
                          const rate = (current.__newRate ?? "").trim();
                          if (!name || !rate) return current;
                          const next = { ...current, [name]: rate };
                          delete next.__newModel;
                          delete next.__newRate;
                          return next;
                        })
                      }
                    >
                      添加
                    </Button>
                  </div>
                  <p className="text-[11px] leading-4 text-muted-foreground">
                    倍率按官方公布的「积分消耗倍率」填写（0 = 免费）。键用本地日志里的模型 id；
                    只影响本页归一计算，不改动任何计费行为。{" "}
                    {api.isDemoMode() && "演示模式下不可保存。"}
                  </p>
                </div>
              )}

              <p className="text-[11px] leading-5 text-muted-foreground">
                判读：归一基准价 = 单价 ÷ 官方倍率。若倍率正确，各模型应收敛到同一基准；
                <span className="font-medium text-foreground">单个模型明显偏高才值得怀疑</span>，
                全体一起偏高更可能是计价口径或官方调价。倍率为 0 的免费模型、倍率未知的模型不参与基准。
                本地账本来自 {formatNumber(billing.filesScanned, 0)} 个会话文件、
                {formatNumber(billing.records, 0)} 条带积分的记录
                {billing.undatedRecords > 0 && `（另有 ${formatNumber(billing.undatedRecords, 0)} 条无时间戳未计入）`}。
              </p>
            </>
          ) : null}
        </CardContent>
      </Card>

      {official && reconcile && (
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-1 px-4 pt-3 pb-2 sm:px-5">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="text-[13px] font-medium">官方账单 vs 本地记录</div>
              <Badge variant={official.status === "complete" ? "success" : "warning"}>
                {official.status === "complete" ? "官方数据完整" : "官方数据部分同步"}
              </Badge>
            </div>
            <CardDescription className="text-xs">
              同一区间 {official.rangeStart} ~ {official.rangeEnd}（含两端），按模型对齐后比较积分消耗。
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3 px-4 pb-4 sm:px-5">
            <div className="grid grid-cols-1 gap-2.5 sm:grid-cols-3">
              <Metric label="官方账单积分" value={formatNumber(reconcile.officialTotal)} />
              <Metric label="本地记录积分" value={formatNumber(reconcile.localTotal)} />
              <Metric
                label="差额（官方 − 本地）"
                value={`${reconcile.diff >= 0 ? "+" : ""}${formatNumber(reconcile.diff)}`}
                hint="负数表示本地记录比官方账单更多"
              />
            </div>
            <div className="min-w-0 overflow-x-auto rounded-lg border">
              <table className="w-full min-w-[520px] border-collapse text-xs">
                <thead>
                  <tr className="border-b bg-muted/50 text-muted-foreground">
                    <th className="px-3 py-2.5 text-left font-medium">模型</th>
                    <th className="px-3 py-2.5 text-right font-medium">官方</th>
                    <th className="px-3 py-2.5 text-right font-medium">本地</th>
                    <th className="px-3 py-2.5 text-right font-medium">差额</th>
                    <th className="px-3 py-2.5 text-right font-medium">差异占比</th>
                  </tr>
                </thead>
                <tbody>
                  {reconcile.items.map((item) => {
                    const percent =
                      item.diff !== null && item.officialCredit && item.officialCredit > 0
                        ? (Math.abs(item.diff) / item.officialCredit) * 100
                        : null;
                    return (
                      <tr key={item.key} className="border-b last:border-b-0">
                        <td className="max-w-[220px] truncate px-3 py-2.5 font-medium" title={item.label}>
                          {item.label}
                        </td>
                        <td className="px-3 py-2.5 text-right tabular-nums">
                          {formatNumber(item.officialCredit)}
                        </td>
                        <td className="px-3 py-2.5 text-right tabular-nums">
                          {formatNumber(item.localCredit)}
                        </td>
                        <td className="px-3 py-2.5 text-right font-medium tabular-nums">
                          {item.diff === null ? "—" : `${item.diff >= 0 ? "+" : ""}${formatNumber(item.diff)}`}
                        </td>
                        <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                          {percent === null ? "—" : `${percent.toFixed(1)}%`}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
            <p className="text-[11px] leading-5 text-muted-foreground">
              官方接口按账号统计（可能包含其他设备、其他客户端的请求），本地账本只覆盖这台机器上的会话日志；
              官方数据部分同步、明细被截断或账号切换都会让差额变大。因此差额用于定位
              <span className="font-medium text-foreground">「漏记 / 口径不一致」</span>，
              不能单独作为「多扣积分」的结论；要结合上面的归一基准价一起看。
            </p>
          </CardContent>
        </Card>
      )}
    </section>
  );
}
