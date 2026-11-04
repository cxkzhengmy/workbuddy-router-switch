import { useCallback, useEffect, useMemo, useState } from "react";
import {
  CircleAlert,
  Copy,
  Loader2,
  Network,
  Play,
  Plus,
  RefreshCw,
  Square,
  Trash2,
  Zap,
} from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { StatusDot } from "@/components/product-marks";
import { useVisibleInterval } from "@/lib/use-visible-interval";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type {
  ProxyAccountConfig,
  ProxyAccountPolicy,
  ProxyCredits,
  ProxyKey,
  ProxyModelInfo,
  ProxyOverview,
  ProxyStatus,
} from "@/lib/types";

/**
 * 网关页：内置反代（Rust 原生）。
 *
 * 与旧版页面最大的区别是这里没有「Python 解释器 / hub 目录 / 版本升级 / 面板」
 * 这些概念——服务跑在应用进程内，账号直接用「账号」页的当前激活账号，
 * 切号后下一条请求就生效。页面只回答四个问题：
 * 服务通不通、Key 有哪些、今天用了多少、每个模型多快。
 */
export default function GatewayPage() {
  const [status, setStatus] = useState<ProxyStatus | null>(null);
  const [keys, setKeys] = useState<ProxyKey[]>([]);
  const [models, setModels] = useState<ProxyModelInfo[]>([]);
  const [overview, setOverview] = useState<ProxyOverview | null>(null);
  const [credits, setCredits] = useState<ProxyCredits | null>(null);
  const [accountCfg, setAccountCfg] = useState<ProxyAccountConfig | null>(null);
  const [draft, setDraft] = useState<ProxyAccountPolicy | null>(null);
  const [savingPolicy, setSavingPolicy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);

  const [newName, setNewName] = useState("");
  const [newModels, setNewModels] = useState<string[]>([]);
  // 默认展开：模型是配 Key 时最需要先看一眼的东西，藏在按钮后面等于没列。
  const [showModelPicker, setShowModelPicker] = useState(true);
  const [created, setCreated] = useState<{ name: string; secret: string } | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [editModels, setEditModels] = useState<string[]>([]);

  // force=true 时强制向 upstream 重拉模型目录。
  // 模型走 10 分钟 TTL 的缓存，定时刷新用 false 省一次往返；
  // 用户手点刷新必须为 true，否则按 10 分钟内永远看到旧目录。
  const refresh = useCallback(async (force = false) => {
    setLoading(true);
    try {
      const [s, k, m, ac] = await Promise.all([
        api.proxyStatus(),
        api.proxyListKeys(),
        api.proxyModels(force),
        api.proxyAccountConfig(),
      ]);
      setStatus(s);
      setKeys(k);
      setModels(m.models ?? []);
      setAccountCfg(ac);
      // 只在首次填充，避免刷新把用户正在改的策略冲掉。
      setDraft((prev) => prev ?? ac.policy);
      const o = await api.proxyOverview(7);
      setOverview(o);
    } catch (error) {
      toast.error(`读取网关状态失败：${(error as Error).message}`);
    } finally {
      setLoading(false);
    }
    // 积分要打上游，单独拉，不拖慢首屏。
    api
      .proxyCredits(false)
      .then(setCredits)
      .catch(() => setCredits(null));
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 用量与最近请求每 5 秒自己刷新一次。
  // 只拉 overview，不动 Key / 模型 / 账号策略——那几块重拉会让正在编辑的
  // 输入框闪一下。窗口隐藏时 hook 会自动暂停，不会在后台空转。
  // 只重拉模型目录：整页 refresh() 会把 loading 拉满、整个界面闪一下，
  // 而用户此刻只想更新右下方那份模型清单。
  const [refreshingModels, setRefreshingModels] = useState(false);
  const refreshModels = useCallback(async () => {
    setRefreshingModels(true);
    try {
      const result = await api.proxyModels(true);
      const list = result.models ?? [];
      setModels(list);
      toast.success(`模型目录已刷新（${list.length} 个）`);
    } catch (error) {
      toast.error(`刷新失败：${(error as Error).message}`);
    } finally {
      setRefreshingModels(false);
    }
  }, []);

  const refreshStats = useCallback(() => {
    api
      .proxyOverview(7)
      .then(setOverview)
      .catch(() => {
        /* 轮询失败保持上一次的展示，不打断用户 */
      });
  }, []);
  useVisibleInterval(refreshStats, 5000, !loading);

  const onStart = async () => {
    setBusy("start");
    try {
      const res = await api.proxyStart();
      if (res.warning) toast.warning(res.warning);
      else toast.success(`网关已启动，端口 ${res.port}`);
      await refresh();
    } catch (error) {
      toast.error(`启动失败：${(error as Error).message}`);
    } finally {
      setBusy(null);
    }
  };

  const onStop = async () => {
    setBusy("stop");
    try {
      await api.proxyStop();
      toast.success("网关已停止");
      await refresh();
    } catch (error) {
      toast.error(`停止失败：${(error as Error).message}`);
    } finally {
      setBusy(null);
    }
  };

  /**
   * 开关局域网访问。
   *
   * 开启是不可逆的「风险动作」，先弹一次确认：这个网关没有用户体系，
   * 一旦绑到 0.0.0.0，同网段拿到 Key 的人就能用本机账号的额度。
   * 关闭则不需要确认——收紧权限永远不该被拦。
   */
  const onToggleLan = async (enabled: boolean) => {
    if (enabled) {
      const ok = window.confirm(
        "开启后，同一局域网内的其它设备都能访问本机网关。\n" +
          "网关只靠 API Key 鉴权，Key 泄露就等于账号额度泄露。\n\n" +
          "请只在可信网络下开启。是否继续？",
      );
      if (!ok) return;
    }
    setBusy("lan");
    try {
      const res = await api.proxySetLanAccess(enabled);
      if (!res.ok) {
        toast.error(res.error ?? "切换失败");
      } else if (res.restarted) {
        toast.success(enabled ? "已开启局域网访问，网关已按新地址重启" : "已关闭局域网访问，网关已重启");
      } else if (!res.status?.running) {
        toast.success(enabled ? "已开启局域网访问，下次启动网关时生效" : "已关闭局域网访问");
      } else {
        toast.success(enabled ? "已开启局域网访问" : "已关闭局域网访问");
      }
      if (res.status) setStatus(res.status);
      await refresh();
    } catch (error) {
      toast.error(`切换失败：${(error as Error).message}`);
    } finally {
      setBusy(null);
    }
  };

  const onCreateKey = async () => {
    setBusy("create");
    try {
      const res = await api.proxyCreateKey(newName, newModels);
      setCreated({ name: newName.trim() || "默认 Key", secret: res.secret });
      setNewName("");
      setNewModels([]);
      setShowModelPicker(false);
      await refresh();
    } catch (error) {
      toast.error(`创建失败：${(error as Error).message}`);
    } finally {
      setBusy(null);
    }
  };

  const onReveal = async (key: ProxyKey) => {
    try {
      const res = await api.proxyRevealKey(key.id);
      await copy(res.secret, `已复制「${key.name}」的完整密钥`);
    } catch (error) {
      toast.error(`读取密钥失败：${(error as Error).message}`);
    }
  };

  const onDelete = async (key: ProxyKey) => {
    if (!window.confirm(`删除 API Key「${key.name}」？使用它的客户端会立即失效。`)) return;
    try {
      await api.proxyDeleteKey(key.id);
      toast.success("已删除");
      await refresh();
    } catch (error) {
      toast.error(`删除失败：${(error as Error).message}`);
    }
  };

  const onToggle = async (key: ProxyKey, enabled: boolean) => {
    try {
      await api.proxySetKeyEnabled(key.id, enabled);
      setKeys((prev) => prev.map((k) => (k.id === key.id ? { ...k, enabled } : k)));
    } catch (error) {
      toast.error(`切换失败：${(error as Error).message}`);
    }
  };

  const saveModels = async (id: string) => {
    try {
      await api.proxySetKeyModels(id, editModels.length > 0 ? editModels : null);
      toast.success("模型范围已保存");
      setEditing(null);
      await refresh();
    } catch (error) {
      toast.error(`保存失败：${(error as Error).message}`);
    }
  };

  const savePolicy = async () => {
    if (!draft) return;
    setSavingPolicy(true);
    try {
      const res = await api.proxySaveAccountConfig(draft);
      setAccountCfg(res);
      setDraft(res.policy);
      toast.success("账号调度已保存");
    } catch (error) {
      toast.error(`保存失败：${(error as Error).message}`);
    } finally {
      setSavingPolicy(false);
    }
  };

  const onRefreshCredits = async () => {
    setBusy("credits");
    try {
      setCredits(await api.proxyCredits(true));
      toast.success("积分已刷新");
    } catch (error) {
      toast.error(`刷新失败：${(error as Error).message}`);
    } finally {
      setBusy(null);
    }
  };

  // 后端只返回有记录的日期。补齐成连续 7 天，否则「只有今天」时一根柱子
  // 会被 flex 拉满整行，看起来像「今天暴涨」。
  const dailySeries = useMemo(() => {
    const map = new Map((overview?.daily ?? []).map((point) => [point.date, point]));
    const out: { date: string; tokens: number; requests: number }[] = [];
    const now = new Date();
    for (let offset = 6; offset >= 0; offset -= 1) {
      const day = new Date(now.getFullYear(), now.getMonth(), now.getDate() - offset);
      const key = `${day.getFullYear()}-${String(day.getMonth() + 1).padStart(2, "0")}-${String(
        day.getDate(),
      ).padStart(2, "0")}`;
      const hit = map.get(key);
      out.push({ date: key, tokens: hit?.tokens ?? 0, requests: hit?.requests ?? 0 });
    }
    return out;
  }, [overview]);

  const dailyMax = useMemo(
    () => Math.max(1, ...dailySeries.map((point) => point.tokens)),
    [dailySeries],
  );

  // 逐字段比较。用 JSON.stringify 比较会因为键顺序不同而误报「有未保存改动」。
  const policyDirty = useMemo(() => {
    if (!draft || !accountCfg) return false;
    const saved = accountCfg.policy;
    return (
      draft.mode !== saved.mode ||
      draft.fixed !== saved.fixed ||
      draft.strategy !== saved.strategy ||
      draft.failover !== saved.failover ||
      draft.pool.length !== saved.pool.length ||
      draft.pool.some((ref) => !saved.pool.includes(ref))
    );
  }, [draft, accountCfg]);

  if (loading && !status) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-32 w-full" />
        <Skeleton className="h-48 w-full" />
      </div>
    );
  }

  const today = overview?.today;

  return (
    <div className="space-y-4">
      <div>
        <h1 className="text-xl font-semibold tracking-tight">网关</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          内置反代运行在本应用进程内，使用「账号」页的当前激活账号。应用退出即停止。
        </p>
      </div>

      {/* 运行状态 */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <CardTitle className="flex items-center gap-2 text-base">
              <StatusDot on={!!status?.running} />
              {status?.running ? "运行中" : "未运行"}
            </CardTitle>
            <div className="flex items-center gap-2">
              <Button variant="ghost" size="icon" onClick={() => void refresh(true)} aria-label="刷新">
                <RefreshCw className="size-4" />
              </Button>
              {status?.running ? (
                <Button variant="outline" size="sm" onClick={() => void onStop()} disabled={busy !== null}>
                  {busy === "stop" ? <Loader2 className="size-4 animate-spin" /> : <Square className="size-4" />}
                  停止
                </Button>
              ) : (
                <Button size="sm" onClick={() => void onStart()} disabled={busy !== null}>
                  {busy === "start" ? <Loader2 className="size-4 animate-spin" /> : <Play className="size-4" />}
                  启动
                </Button>
              )}
            </div>
          </div>
          <CardDescription>
            {status?.running
              ? "把下面这个地址填进任意 OpenAI 兼容客户端即可使用。"
              : "启动后即可通过 OpenAI 兼容接口访问当前账号的模型能力。"}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-2">
            <code className="rounded-md border bg-muted/40 px-2 py-1 font-mono text-xs">
              {status?.baseUrl ?? "http://127.0.0.1:8788/v1"}
            </code>
            <Button
              variant="outline"
              size="sm"
              onClick={() => void copy(status?.baseUrl ?? "", "地址已复制")}
            >
              <Copy className="size-3.5" />
              复制
            </Button>
            <Badge variant="secondary">{status?.keyCount ?? 0} 个 Key</Badge>
          </div>

          {/* 局域网访问：默认关闭，纯本机。开启后同网段设备（手机 / 另一台电脑）
              才能连进来，代价是账号能力暴露面变大，所以文案要把代价说清楚。 */}
          <div className="rounded-md border bg-muted/30 px-3 py-2">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="flex items-center gap-2">
                <Network className="size-3.5 text-muted-foreground" />
                <span className="text-xs font-medium">允许局域网访问</span>
                <span className="text-[11px] text-muted-foreground">
                  {status?.lanAccess ? "监听 0.0.0.0，同网段设备可连" : "仅本机 127.0.0.1 可连"}
                </span>
              </div>
              <div className="flex items-center gap-2">
                {busy === "lan" && <Loader2 className="size-3.5 animate-spin text-muted-foreground" />}
                <Switch
                  checked={!!status?.lanAccess}
                  disabled={busy !== null}
                  onCheckedChange={(checked) => void onToggleLan(checked)}
                  aria-label="允许局域网访问"
                />
              </div>
            </div>

            {status?.lanAccess && (
              <div className="mt-2 space-y-2 border-t border-amber-500/30 pt-2">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-[11px] text-muted-foreground">
                    {status.running ? "其它设备填这个地址" : "启动后其它设备填这个地址"}
                  </span>
                  <code className="rounded border bg-background px-2 py-0.5 font-mono text-xs">
                    {status.lanUrl ?? "未识别到局域网 IP，请用 ipconfig 查看"}
                  </code>
                  {status.lanUrl && (
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-6 px-2 text-xs"
                      onClick={() => void copy(status.lanUrl ?? "", "局域网地址已复制")}
                    >
                      <Copy className="size-3" />
                      复制
                    </Button>
                  )}
                </div>
                <p className="text-[11px] leading-relaxed text-amber-600 dark:text-amber-400">
                  同网段任何设备都能访问本网关，仍需要 Bearer Key —— 别把 Key 给不可信的人。
                  首次开启时 Windows 会弹防火墙提示，需勾选「专用网络」并允许，否则其它设备仍然连不上。
                </p>
              </div>
            )}
          </div>

          {!status?.running && (
            <Alert>
              <CircleAlert className="size-4" />
              <AlertTitle>服务未启动</AlertTitle>
              <AlertDescription>
                若启动失败提示端口被占用，通常是旧的 python 网关（wb_proxy）还在运行。
                先结束它，再点「启动」。
              </AlertDescription>
            </Alert>
          )}

          <div className="rounded-md border bg-muted/30 p-3">
            <div className="mb-1.5 flex items-center gap-2 text-xs font-medium text-muted-foreground">
              <Zap className="size-3.5" />
              调用示例
            </div>
            <pre className="overflow-x-auto whitespace-pre-wrap break-all font-mono text-[11px] leading-relaxed text-muted-foreground">
{`curl ${status?.baseUrl ?? "http://127.0.0.1:8788/v1"}/chat/completions \\
  -H "Authorization: Bearer sk-wb-..." \\
  -H "Content-Type: application/json" \\
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}'`}
            </pre>
          </div>
        </CardContent>
      </Card>

      {/* 账号调度 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">账号调度</CardTitle>
          <CardDescription>
            决定网关用哪个账号调用上游。默认跟随官方客户端当前登录的账号。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {draft && (
            <>
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-xs text-muted-foreground">出站身份</span>
                {(
                  [
                    ["workbuddy", "WB", "WorkBuddy 独立桌面客户端（默认）"],
                    ["vscode", "VSC", "官方 VSCode 插件"],
                    ["cli", "CLI", "官方 CodeBuddy CLI"],
                  ] as const
                ).map(([value, label, hint]) => (
                  <Button
                    key={value}
                    size="sm"
                    title={hint}
                    variant={draft.identity === value ? "default" : "outline"}
                    onClick={() => setDraft({ ...draft, identity: value })}
                  >
                    {label}
                  </Button>
                ))}
                <span className="text-xs text-muted-foreground/70">
                  决定上游看到的客户端指纹与接入点
                </span>
              </div>

              <div className="h-px bg-border" />

              <div className="flex flex-wrap gap-2">
                {(
                  [
                    ["current", "跟随当前账号"],
                    ["fixed", "指定账号"],
                    ["pool", "账号池"],
                  ] as const
                ).map(([value, label]) => (
                  <Button
                    key={value}
                    size="sm"
                    variant={draft.mode === value ? "default" : "outline"}
                    onClick={() =>
                      setDraft({
                        ...draft,
                        mode: value,
                        // 切到池模式且池为空时，先带上可用账号，免得用户
                        // 一点「保存」就被「请勾选账号」挡住。
                        pool:
                          value === "pool" && draft.pool.length === 0
                            ? (accountCfg?.accounts ?? [])
                                .filter((item) => item.usable)
                                .slice(0, 2)
                                .map((item) => item.ref)
                            : draft.pool,
                      })
                    }
                  >
                    {label}
                  </Button>
                ))}
              </div>

              {draft.mode === "current" && (
                <p className="text-xs text-muted-foreground">
                  使用「账号管理」里当前激活的账号；在账号页切号后，下一条请求立即生效。
                </p>
              )}

              {draft.mode !== "current" && (
                <div className="max-h-56 space-y-0.5 overflow-auto rounded-md border p-2">
                  {(accountCfg?.accounts.length ?? 0) === 0 && (
                    <p className="px-1 py-2 text-xs text-muted-foreground">账号库为空。</p>
                  )}
                  {accountCfg?.accounts.map((acc) => {
                    const checked =
                      draft.mode === "fixed" ? draft.fixed === acc.ref : draft.pool.includes(acc.ref);
                    return (
                      <label
                        key={acc.ref}
                        className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 text-xs hover:bg-muted/50"
                      >
                        <Checkbox
                          checked={checked}
                          onCheckedChange={(next) => {
                            if (draft.mode === "fixed") {
                              setDraft({ ...draft, fixed: next ? acc.ref : "" });
                            } else {
                              setDraft({
                                ...draft,
                                pool: next
                                  ? [...draft.pool, acc.ref]
                                  : draft.pool.filter((ref) => ref !== acc.ref),
                              });
                            }
                          }}
                        />
                        <span className="truncate">{acc.name}</span>
                        <Badge variant="outline">{acc.variant}</Badge>
                        {acc.isCurrent && <Badge variant="secondary">当前</Badge>}
                        {!acc.usable && <span className="text-amber-500">凭据不可用</span>}
                        {acc.cooling && <span className="text-amber-500">冷却中</span>}
                        <span className="ml-auto tabular-nums text-muted-foreground">
                          今日 {acc.todayRequests} 次
                        </span>
                      </label>
                    );
                  })}
                </div>
              )}

              {draft.mode === "pool" && (
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-xs text-muted-foreground">调度策略</span>
                  {(
                    [
                      ["round_robin", "轮询"],
                      ["least_requests", "今日请求最少优先"],
                    ] as const
                  ).map(([value, label]) => (
                    <Button
                      key={value}
                      size="sm"
                      variant={draft.strategy === value ? "default" : "outline"}
                      onClick={() => setDraft({ ...draft, strategy: value })}
                    >
                      {label}
                    </Button>
                  ))}
                </div>
              )}

              <div className="flex items-center gap-2">
                <Switch
                  checked={draft.failover}
                  onCheckedChange={(checked) => setDraft({ ...draft, failover: checked })}
                />
                <span className="text-xs text-muted-foreground">
                  失败时自动换下一个账号（401 / 429 / 5xx；参数类错误不换号）
                </span>
              </div>

              <div className="flex items-center gap-2">
                <Button size="sm" onClick={() => void savePolicy()} disabled={savingPolicy}>
                  {savingPolicy && <Loader2 className="size-4 animate-spin" />}
                  保存
                </Button>
                {policyDirty && (
                  <span className="text-xs text-amber-500">有未保存的改动</span>
                )}
              </div>
            </>
          )}
        </CardContent>
      </Card>

      {/* 今日用量 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">今日用量</CardTitle>
          <CardDescription>网关自己记的台账，不依赖客户端日志。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
            <Metric label="请求" value={fmtNum(today?.requests ?? 0)} />
            <Metric label="输入" value={fmtNum(today?.promptTokens ?? 0)} />
            <Metric label="输出" value={fmtNum(today?.completionTokens ?? 0)} />
            <Metric label="积分" value={fmtCredit(today?.credit ?? 0)} />
            <Metric label="平均首字" value={fmtMs(today?.avgTtftMs ?? 0)} />
            <Metric
              label="失败"
              value={fmtNum(today?.errors ?? 0)}
              tone={(today?.errors ?? 0) > 0 ? "warn" : undefined}
            />
          </div>

          {dailySeries.length > 0 && (
            <div>
              <div className="mb-2 text-xs font-medium text-muted-foreground">近 7 日 token</div>
              <div className="flex h-20 items-end gap-1.5">
                {dailySeries.map((point) => (
                  <div key={point.date} className="group flex flex-1 flex-col items-center gap-1">
                    <div
                      className="w-full rounded-sm bg-primary/60 transition-colors group-hover:bg-primary"
                      style={{ height: `${Math.max(2, (point.tokens / dailyMax) * 64)}px` }}
                      title={`${point.date}：${fmtNum(point.tokens)} tokens / ${point.requests} 次`}
                    />
                    <span className="text-[10px] text-muted-foreground">{point.date.slice(5)}</span>
                  </div>
                ))}
              </div>
            </div>
          )}
        </CardContent>
      </Card>

      {/* API Key */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">API Key</CardTitle>
          <CardDescription>
            每个 Key 可单独限制可用模型。密钥只在创建时完整显示一次。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {created && (
            <Alert>
              <AlertTitle>已创建「{created.name}」</AlertTitle>
              <AlertDescription className="space-y-2">
                <code className="block break-all rounded-md border bg-muted/40 px-2 py-1.5 font-mono text-xs">
                  {created.secret}
                </code>
                <div className="flex gap-2">
                  <Button size="sm" variant="outline" onClick={() => void copy(created.secret, "已复制")}>
                    <Copy className="size-3.5" />
                    复制
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setCreated(null)}>
                    关闭
                  </Button>
                </div>
              </AlertDescription>
            </Alert>
          )}

          <div className="flex flex-wrap items-end gap-2">
            <div className="min-w-40 flex-1">
              <Label className="mb-1.5 block text-xs">名称</Label>
              <Input
                value={newName}
                placeholder="比如 Cline / Cherry Studio"
                onChange={(event) => setNewName(event.target.value)}
              />
            </div>
            <Button
              variant="outline"
              size="sm"
              onClick={() => setShowModelPicker((v) => !v)}
              className="h-9"
            >
              模型范围{newModels.length > 0 ? `（${newModels.length}）` : "（全部）"}
            </Button>
            <Button size="sm" className="h-9" onClick={() => void onCreateKey()} disabled={busy === "create"}>
              {busy === "create" ? <Loader2 className="size-4 animate-spin" /> : <Plus className="size-4" />}
              新建
            </Button>
          </div>

          {showModelPicker && (
            <ModelPicker
              models={models}
              selected={newModels}
              onChange={setNewModels}
              onRefresh={() => void refreshModels()}
              refreshing={refreshingModels}
            />
          )}

          <div className="space-y-2">
            {keys.length === 0 && (
              <p className="text-sm text-muted-foreground">还没有 Key，先建一个再接入客户端。</p>
            )}
            {keys.map((key) => (
              <div key={key.id} className="rounded-md border p-3">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium">{key.name}</span>
                  <code className="font-mono text-xs text-muted-foreground">{key.preview}…</code>
                  <Badge variant={key.models && key.models.length > 0 ? "outline" : "secondary"}>
                    {key.models && key.models.length > 0 ? `${key.models.length} 个模型` : "全部模型"}
                  </Badge>
                  <span className="text-xs text-muted-foreground">
                    {key.lastUsedAt ? `最近使用 ${fmtTime(key.lastUsedAt)}` : "尚未使用"}
                  </span>
                  <div className="ml-auto flex items-center gap-2">
                    <Switch
                      checked={key.enabled}
                      onCheckedChange={(checked) => void onToggle(key, checked)}
                      aria-label="启用"
                    />
                    <Button variant="ghost" size="sm" onClick={() => void onReveal(key)}>
                      <Copy className="size-3.5" />
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => {
                        setEditing(editing === key.id ? null : key.id);
                        setEditModels(key.models ?? []);
                      }}
                    >
                      模型
                    </Button>
                    <Button variant="ghost" size="sm" onClick={() => void onDelete(key)}>
                      <Trash2 className="size-3.5" />
                    </Button>
                  </div>
                </div>
                {editing === key.id && (
                  <div className="mt-3 space-y-2 border-t pt-3">
                    <ModelPicker
                      models={models}
                      selected={editModels}
                      onChange={setEditModels}
                      onRefresh={() => void refreshModels()}
                      refreshing={refreshingModels}
                    />
                    <div className="flex gap-2">
                      <Button size="sm" onClick={() => void saveModels(key.id)}>
                        保存
                      </Button>
                      <Button size="sm" variant="ghost" onClick={() => setEditing(null)}>
                        取消
                      </Button>
                      <span className="self-center text-xs text-muted-foreground">不勾选 = 不限制</span>
                    </div>
                  </div>
                )}
              </div>
            ))}
          </div>
        </CardContent>
      </Card>

      {/* 模型速度 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">模型速度</CardTitle>
          <CardDescription>首字延迟取 p50 / p95，吞吐按生成阶段折算。</CardDescription>
        </CardHeader>
        <CardContent>
          {(overview?.models?.length ?? 0) === 0 ? (
            <p className="text-sm text-muted-foreground">还没有请求记录。</p>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b text-left text-xs text-muted-foreground">
                    <th className="pb-2 pr-3 font-medium">模型</th>
                    <th className="pb-2 pr-3 text-right font-medium">请求</th>
                    <th className="pb-2 pr-3 text-right font-medium">p50</th>
                    <th className="pb-2 pr-3 text-right font-medium">p95</th>
                    <th className="pb-2 pr-3 text-right font-medium">tok/s</th>
                    <th className="pb-2 pr-3 text-right font-medium">输出</th>
                    <th className="pb-2 text-right font-medium">积分</th>
                  </tr>
                </thead>
                <tbody>
                  {overview!.models.map((row) => (
                    <tr key={row.model} className="border-b border-border/50 last:border-0">
                      <td className="max-w-56 truncate py-2 pr-3 font-mono text-xs">{row.model}</td>
                      <td className="py-2 pr-3 text-right tabular-nums">{fmtNum(row.requests)}</td>
                      <td className="py-2 pr-3 text-right tabular-nums">{fmtMs(row.ttftP50)}</td>
                      <td className="py-2 pr-3 text-right tabular-nums">{fmtMs(row.ttftP95)}</td>
                      <td className="py-2 pr-3 text-right tabular-nums">{row.tokensPerSec || "-"}</td>
                      <td className="py-2 pr-3 text-right tabular-nums">{fmtNum(row.completionTokens)}</td>
                      <td className="py-2 text-right tabular-nums">{fmtCredit(row.credit)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {/* 积分 */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <CardTitle className="text-base">积分</CardTitle>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => void onRefreshCredits()}
              disabled={busy === "credits"}
            >
              {busy === "credits" ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <RefreshCw className="size-3.5" />
              )}
              刷新
            </Button>
          </div>
          <CardDescription>
            {credits?.ok
              ? `当前登录账号：${credits.accountName ?? "—"}${
                  draft && draft.mode !== "current" ? " · 网关按调度使用其他账号时，这里仍显示该账号余额" : ""
                }`
              : credits?.error ?? "读取中…"}
          </CardDescription>
        </CardHeader>
        <CardContent>
          {credits?.ok ? (
            <div className="space-y-3">
              <div className="grid grid-cols-3 gap-3">
                <Metric label="剩余" value={fmtCredit(credits.totalRemaining ?? 0)} />
                <Metric label="总量" value={fmtCredit(credits.totalCapacity ?? 0)} />
                <Metric label="即将过期" value={fmtCredit(credits.expiringSoonRemaining ?? 0)} />
              </div>
              {(credits.resources?.length ?? 0) > 0 && (
                <div className="space-y-1.5">
                  {credits.resources!.slice(0, 6).map((res, index) => (
                    <div key={`${res.packageName ?? "包"}-${index}`} className="text-xs">
                      <div className="flex justify-between text-muted-foreground">
                        <span className="truncate pr-2">{res.packageName ?? "资源包"}</span>
                        <span className="tabular-nums">
                          {fmtCredit(res.remaining)} / {fmtCredit(res.total)}
                        </span>
                      </div>
                      <div className="mt-1 h-1.5 overflow-hidden rounded-full bg-muted">
                        <div
                          className="h-full rounded-full bg-primary/70"
                          style={{
                            width: `${res.total > 0 ? Math.min(100, (res.remaining / res.total) * 100) : 0}%`,
                          }}
                        />
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </div>
          ) : (
            <p className="text-sm text-muted-foreground">暂无可用的积分数据。</p>
          )}
        </CardContent>
      </Card>

      {/* 最近请求 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">最近请求</CardTitle>
          <CardDescription>最近 50 条，每 5 秒自动刷新（窗口隐藏时暂停）。</CardDescription>
        </CardHeader>
        <CardContent>
          {(overview?.recent?.length ?? 0) === 0 ? (
            <p className="text-sm text-muted-foreground">还没有请求。</p>
          ) : (
            <div className="max-h-72 overflow-auto">
              <table className="w-full text-xs">
                <thead className="sticky top-0 bg-card">
                  <tr className="border-b text-left text-muted-foreground">
                    <th className="pb-2 pr-3 font-medium">时间</th>
                    <th className="pb-2 pr-3 font-medium">模型</th>
                    <th className="pb-2 pr-3 text-right font-medium">状态</th>
                    <th className="pb-2 pr-3 text-right font-medium">输入/输出</th>
                    <th className="pb-2 pr-3 text-right font-medium">首字</th>
                    <th className="pb-2 text-right font-medium">总耗时</th>
                  </tr>
                </thead>
                <tbody>
                  {overview!.recent.map((row, index) => (
                    <tr key={`${row.ts}-${index}`} className="border-b border-border/40 last:border-0">
                      <td className="py-1.5 pr-3 text-muted-foreground">{fmtTime(row.ts)}</td>
                      <td className="max-w-40 truncate py-1.5 pr-3 font-mono">{row.model ?? "-"}</td>
                      <td
                        className={cn(
                          "py-1.5 pr-3 text-right tabular-nums",
                          row.status >= 400 ? "text-red-500" : "text-emerald-500",
                        )}
                        title={row.error ?? undefined}
                      >
                        {row.status}
                      </td>
                      <td className="py-1.5 pr-3 text-right tabular-nums">
                        {fmtNum(row.promptTokens)}/{fmtNum(row.completionTokens)}
                      </td>
                      <td className="py-1.5 pr-3 text-right tabular-nums">{fmtMs(row.ttftMs)}</td>
                      <td className="py-1.5 text-right tabular-nums">{fmtMs(row.totalMs)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

function Metric({
  label,
  value,
  tone,
}: {
  label: string;
  value: string;
  tone?: "warn";
}) {
  return (
    <div className="rounded-md border bg-muted/20 px-3 py-2">
      <div className="text-[11px] text-muted-foreground">{label}</div>
      <div
        className={cn(
          "mt-0.5 text-lg font-semibold tabular-nums",
          tone === "warn" && "text-amber-500",
        )}
      >
        {value}
      </div>
    </div>
  );
}

function ModelPicker({
  models,
  selected,
  onChange,
  onRefresh,
  refreshing,
}: {
  models: ProxyModelInfo[];
  selected: string[];
  onChange: (next: string[]) => void;
  onRefresh: () => void;
  refreshing: boolean;
}) {
  // 目录来自 10 分钟 TTL 的缓存，光看列表分不出"是刚拉的"还是"十分钟前的"。
  // 这里给一个显式的刷新入口，省得用户跑去页面顶部找。
  const header = (
    <div className="flex items-center justify-between gap-2 pb-1.5">
      <span className="text-xs text-muted-foreground">
        {models.length > 0 ? `共 ${models.length} 个模型` : "模型目录还没加载出来"}
      </span>
      <Button
        variant="ghost"
        size="sm"
        className="h-6 px-2 text-xs"
        onClick={onRefresh}
        disabled={refreshing}
      >
        {refreshing ? (
          <Loader2 className="size-3 animate-spin" />
        ) : (
          <RefreshCw className="size-3" />
        )}
        刷新
      </Button>
    </div>
  );

  if (models.length === 0) {
    return <div>{header}</div>;
  }
  return (
    <div>
      {header}
      <div className="max-h-52 overflow-auto rounded-md border p-2">
      <div className="grid grid-cols-1 gap-1.5 sm:grid-cols-2">
        {models.map((model) => {
          const checked = selected.includes(model.id);
          return (
            <label
              key={model.id}
              className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 text-xs hover:bg-muted/50"
            >
              <Checkbox
                checked={checked}
                onCheckedChange={(next) =>
                  onChange(next ? [...selected, model.id] : selected.filter((id) => id !== model.id))
                }
              />
              <span className="truncate font-mono">{model.id}</span>
            </label>
          );
        })}
      </div>
      </div>
    </div>
  );
}

async function copy(text: string, message: string) {
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
    toast.success(message);
  } catch {
    toast.error("复制失败，请手动选中复制");
  }
}

function fmtNum(value: number): string {
  return value.toLocaleString("zh-CN");
}

function fmtCredit(value: number): string {
  if (!value) return "0";
  return value < 0.01 ? value.toFixed(4) : value.toFixed(2);
}

function fmtMs(value: number | null | undefined): string {
  if (!value) return "-";
  return value >= 1000 ? `${(value / 1000).toFixed(1)}s` : `${Math.round(value)}ms`;
}

function fmtTime(ms: number): string {
  const date = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}
