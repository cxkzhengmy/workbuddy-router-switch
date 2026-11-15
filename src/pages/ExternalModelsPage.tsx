import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Activity,
  CircleAlert,
  CircleCheck,
  ExternalLink,
  Info,
  Loader2,
  LogIn,
  LogOut,
  Plus,
  RefreshCw,
  Save,
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
import * as api from "@/lib/api";
import type { ProxyExternalConfig, ProxyExternalStatus } from "@/lib/types";

/**
 * 外部模型页：把内置网关接到 Trae（trae.cn / trae.ai）的模型上。
 *
 * 和豆包那条通道最大的区别是**没有独立服务要启动**。豆包靠本机 9090 的 Python
 * 进程托着浏览器登录态，要扫码、要过风控；Trae 这边是我们自己按 IDE 的协议
 * 去登录和请求，只有「登录」「令牌过期」两种失败。
 *
 * 登录走官方授权页：点登录会打开 trae.cn 的 authorization，登录完页面自己回调到
 * 本机 127.0.0.1 的随机端口，后端接住就把账号存下来。**回调必须是 127.0.0.1**，
 * Trae 只往那里回传令牌。
 */

/** 用户本地时区下的相对时间，用来说明令牌还剩多久。 */
function humanLeft(minutes: number): string {
  if (minutes <= 0) return "已过期";
  if (minutes < 60) return `${minutes} 分钟`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时 ${minutes % 60} 分`;
  return `${Math.floor(hours / 24)} 天 ${hours % 24} 小时`;
}

export default function ExternalModelsPage() {
  const [cfg, setCfg] = useState<ProxyExternalConfig | null>(null);
  const [status, setStatus] = useState<ProxyExternalStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [fetching, setFetching] = useState(false);
  const [signing, setSigning] = useState(false);
  const [signInUrl, setSignInUrl] = useState("");
  const [remote, setRemote] = useState<string[] | null>(null);
  const [manual, setManual] = useState("");

  const [enabled, setEnabled] = useState(false);
  const [realm, setRealm] = useState("trae-cn");
  const [models, setModels] = useState<string[]>([]);

  const pollRef = useRef<number | null>(null);
  const readOnly = api.isDemoMode();

  const stopPolling = useCallback(() => {
    if (pollRef.current !== null) {
      window.clearInterval(pollRef.current);
      pollRef.current = null;
    }
  }, []);

  const load = useCallback(async () => {
    try {
      const view = await api.proxyExternalConfig();
      setCfg(view);
      setEnabled(view.enabled);
      setRealm(view.realm);
      setModels(view.models);
    } catch (error) {
      toast.error(`读取配置失败：${(error as Error).message}`);
    }
  }, []);

  const checkStatus = useCallback(async () => {
    try {
      setStatus(await api.proxyExternalStatus());
    } catch (error) {
      setStatus(null);
      toast.error(`探测账号状态失败：${(error as Error).message}`);
    }
  }, []);

  useEffect(() => {
    void (async () => {
      setLoading(true);
      await Promise.all([load(), checkStatus()]);
      setLoading(false);
    })();
    return stopPolling;
  }, [load, checkStatus, stopPolling]);

  /** 轮询登录进度：后端接住回调后 state 会变成 done / failed。 */
  const startPolling = useCallback(() => {
    stopPolling();
    pollRef.current = window.setInterval(async () => {
      try {
        const st = await api.proxyExternalSignInState();
        if (st.state === "done") {
          stopPolling();
          setSigning(false);
          setSignInUrl("");
          toast.success(st.message || "登录成功");
          await Promise.all([load(), checkStatus()]);
        } else if (st.state === "failed") {
          stopPolling();
          setSigning(false);
          toast.error(st.message || "登录没有完成");
          await checkStatus();
        }
      } catch {
        // 单次轮询失败不影响，下一拍再试。
      }
    }, 1200);
  }, [checkStatus, load, stopPolling]);

  const signIn = async () => {
    setSigning(true);
    try {
      const res = await api.proxyExternalSignIn(realm);
      setSignInUrl(res.url);
      // 桌面窗里 window.open 可能被拦，所以下面同时给一个可点的链接。
      window.open(res.url, "_blank", "noopener,noreferrer");
      startPolling();
    } catch (error) {
      setSigning(false);
      toast.error(`发起登录失败：${(error as Error).message}`);
    }
  };

  const signOut = async () => {
    stopPolling();
    setSigning(false);
    setSignInUrl("");
    try {
      const view = await api.proxyExternalSignOut();
      setCfg(view);
      setEnabled(view.enabled);
      toast.success("已登出");
      await checkStatus();
    } catch (error) {
      toast.error(`登出失败：${(error as Error).message}`);
    }
  };

  const save = async (patch?: { enabled?: boolean; realm?: string }) => {
    setSaving(true);
    try {
      const view = await api.proxySaveExternalConfig({
        enabled: patch?.enabled ?? enabled,
        realm: patch?.realm ?? realm,
        models,
      });
      setCfg(view);
      setEnabled(view.enabled);
      setRealm(view.realm);
      setModels(view.models);
      toast.success("已保存");
      void checkStatus();
    } catch (error) {
      toast.error(`保存失败：${(error as Error).message}`);
      // 失败时把开关拨回去，别让界面显示一个没存下的状态。
      setEnabled(cfg?.enabled ?? false);
    } finally {
      setSaving(false);
    }
  };

  const pullModels = async () => {
    setFetching(true);
    try {
      const res = await api.proxyExternalModels();
      if (!res.ok) {
        toast.error(res.error ?? "拉取失败");
        setRemote(null);
      } else {
        setRemote(res.models);
        toast.success(`拿到 ${res.models.length} 个模型`);
      }
    } catch (error) {
      toast.error(`拉取失败：${(error as Error).message}`);
    } finally {
      setFetching(false);
    }
  };

  const meta = useMemo(() => {
    const map = new Map<string, { name: string; context: number; sees: boolean }>();
    for (const m of cfg?.fallbackModels ?? []) {
      map.set(m.id, { name: m.name, context: m.context, sees: m.sees });
    }
    return map;
  }, [cfg]);

  const candidates = useMemo(
    () => Array.from(new Set([...(remote ?? []), ...models])).sort(),
    [remote, models],
  );

  const toggle = (id: string, on: boolean) => {
    setModels((prev) => (on ? Array.from(new Set([...prev, id])) : prev.filter((x) => x !== id)));
  };

  const addManual = () => {
    const id = manual.trim();
    if (!id) return;
    toggle(id, true);
    setManual("");
  };

  if (loading) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-40 w-full" />
        <Skeleton className="h-56 w-full" />
      </div>
    );
  }

  const signedIn = Boolean(cfg?.signedIn) || Boolean(status?.signedIn);
  const accountLabel = status?.name || cfg?.accountName || "";
  const leftMin = status?.expiresInMin ?? null;
  const statusBadge = !status?.ok
    ? signedIn
      ? { variant: "secondary" as const, text: "已登录" }
      : { variant: "destructive" as const, text: "未登录" }
    : leftMin !== null && leftMin <= 0
      ? { variant: "destructive" as const, text: "令牌过期" }
      : { variant: "default" as const, text: "可用" };

  return (
    <div className="space-y-4">
      <div>
        <h1 className="text-xl font-semibold tracking-tight">外部模型</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          把内置网关接到 Trae 的模型上。模型会以{" "}
          <code className="font-mono">{cfg?.prefix ?? "trae/"}&lt;id&gt;</code> 的形式出现在{" "}
          <code className="font-mono">/v1/models</code> 里，与 WorkBuddy 自己的模型并存。
        </p>
      </div>

      <Alert className="border-sky-500/30 bg-sky-500/5">
        <Info className="size-4" />
        <AlertTitle>两步走：登录 Trae → 勾选模型</AlertTitle>
        <AlertDescription className="text-xs leading-relaxed">
          <span>
            没有独立服务要启动。点「登录」会打开{" "}
            <code className="font-mono">{realm === "trae-global" ? "www.trae.ai" : "www.trae.cn"}</code>{" "}
            的官方授权页，登录完页面自动回调到本机 <code className="font-mono">127.0.0.1</code>，
            账号就存下来了。网关只转发 <code className="font-mono">/v1/chat/completions</code>，
            请求会按 IDE 的私有协议翻译后再发给 Trae。
          </span>
        </AlertDescription>
      </Alert>

      <Card>
        <CardHeader>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-2">
              <CardTitle className="text-base">账号</CardTitle>
              <Badge variant={statusBadge.variant}>{statusBadge.text}</Badge>
            </div>
            <div className="flex items-center gap-2">
              <Button variant="outline" size="sm" onClick={() => void checkStatus()}>
                <RefreshCw className="size-3.5" />
                刷新状态
              </Button>
              {signedIn ? (
                <Button variant="outline" size="sm" onClick={() => void signOut()} disabled={readOnly}>
                  <LogOut className="size-3.5" />
                  登出
                </Button>
              ) : (
                <Button size="sm" onClick={() => void signIn()} disabled={signing || readOnly}>
                  {signing ? <Loader2 className="size-3.5 animate-spin" /> : <LogIn className="size-3.5" />}
                  {signing ? "等待浏览器登录…" : "登录"}
                </Button>
              )}
            </div>
          </div>
          <CardDescription>
            用 Trae 自己的账号登录，免费额度同样可用。CN 账号不能登国际版，反之亦然。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          {signedIn ? (
            <div className="flex items-start gap-2 text-sm">
              <CircleCheck className="mt-0.5 size-4 shrink-0 text-emerald-600" />
              <div className="min-w-0">
                <p>{status?.message ?? `${accountLabel || "账号"} 已登录`}</p>
                <div className="mt-1.5 flex flex-wrap gap-1.5 text-[11px] text-muted-foreground">
                  <Badge variant="outline">{status?.realm || cfg?.accountRealm || realm}</Badge>
                  {accountLabel && <Badge variant="outline">{accountLabel}</Badge>}
                  {leftMin !== null && <Badge variant="outline">令牌剩余 {humanLeft(leftMin)}</Badge>}
                  {(status?.deviceId || cfg?.deviceId) && (
                    <Badge variant="outline">设备 {status?.deviceId || cfg?.deviceId}</Badge>
                  )}
                </div>
              </div>
            </div>
          ) : (
            <div className="flex items-start gap-2 text-sm">
              <CircleAlert className="mt-0.5 size-4 shrink-0 text-amber-600" />
              <div className="min-w-0">
                <p>{status?.message ?? "还没有登录。点右上角「登录」开始。"}</p>
              </div>
            </div>
          )}

          {signing && (
            <div className="flex items-start gap-2 rounded-md border bg-muted/30 p-3 text-xs">
              <Loader2 className="mt-0.5 size-3.5 shrink-0 animate-spin" />
              <div className="min-w-0 space-y-1">
                <p>已经打开授权页，登录完成后这里会自动变成「已登录」。</p>
                {signInUrl && (
                  <a className="inline-flex items-center gap-1 text-sky-600 underline" href={signInUrl} target="_blank" rel="noreferrer">
                    <ExternalLink className="size-3" />
                    浏览器没打开？点这里手动打开授权页
                  </a>
                )}
              </div>
            </div>
          )}

          <div className="rounded-md border bg-muted/30 p-3 text-[11px] leading-relaxed text-muted-foreground">
            <p className="font-medium text-foreground">为什么登录要开浏览器</p>
            <p className="mt-1">
              授权页只接受回调到 <code className="font-mono">http://127.0.0.1:&lt;端口&gt;/authorize</code>，
              令牌就通过那个回调直接交给本机，不经过剪贴板。登录时会生成一份本机设备指纹
              （设备 id + machine id + P-256 密钥）并持久化——Trae 按设备认人，每次换新设备会被拒。
            </p>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <CardTitle className="text-base">接入配置</CardTitle>
            <div className="flex items-center gap-2">
              <span className="text-xs text-muted-foreground">{enabled ? "已启用" : "未启用"}</span>
              <Switch
                checked={enabled}
                disabled={readOnly || saving}
                onCheckedChange={(next) => {
                  setEnabled(next);
                  void save({ enabled: next });
                }}
              />
            </div>
          </div>
          <CardDescription>关掉后带前缀的模型会从网关列表里消失，相关请求返回 404。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div>
            <Label className="mb-1.5 block text-xs">Trae 部署</Label>
            <div className="flex flex-wrap gap-2">
              {(cfg?.realms ?? []).map((item) => (
                <Button
                  key={item.id}
                  variant={realm === item.id ? "default" : "outline"}
                  size="sm"
                  disabled={readOnly || saving}
                  onClick={() => {
                    setRealm(item.id);
                    void save({ realm: item.id });
                  }}
                >
                  {item.name}
                </Button>
              ))}
            </div>
            <p className="mt-1 text-[11px] text-muted-foreground">
              两个部署是各自独立的服务：域名、客户端、模型清单都不同，账号也不互通。
              当前网关：<code className="font-mono">{cfg?.realms.find((r) => r.id === realm)?.endpoint ?? "-"}</code>
            </p>
          </div>

          <div>
            <Label className="mb-1.5 block text-xs">本机设备</Label>
            <Input value={status?.deviceId || cfg?.deviceId || "登录后生成"} readOnly disabled />
            <p className="mt-1 text-[11px] text-muted-foreground">
              首次登录时生成，之后每次请求都带同一套。清掉它等于换一台机器，Trae 会重新认。
            </p>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <CardTitle className="text-base">模型</CardTitle>
            <div className="flex flex-wrap items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() => setModels((prev) => Array.from(new Set([...prev, ...(cfg?.fallbackModels ?? []).map((m) => m.id)])))}
                disabled={readOnly || !cfg?.fallbackModels.length}
              >
                <Plus className="size-3.5" />
                填入兜底清单
              </Button>
              <Button variant="outline" size="sm" onClick={() => void pullModels()} disabled={fetching || !signedIn}>
                {fetching ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
                从 Trae 拉取
              </Button>
            </div>
          </div>
          <CardDescription>
            勾选的模型才会出现在网关 <code className="font-mono">/v1/models</code> 里。
            「从 Trae 拉取」读的是你账号自己的清单——免费版和付费版看到的不一样。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex gap-2">
            <Input
              value={manual}
              placeholder="手动添加模型 id，例如 deepseek-v4.1-flash"
              disabled={readOnly}
              onChange={(event) => setManual(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") addManual();
              }}
            />
            <Button variant="outline" size="sm" disabled={readOnly || !manual.trim()} onClick={addManual}>
              <Plus className="size-4" />
              添加
            </Button>
          </div>

          {candidates.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              {signedIn ? "还没有模型。点「填入兜底清单」或「从 Trae 拉取」。" : "先登录，才能读到你账号的模型清单。"}
            </p>
          ) : (
            <div className="max-h-72 space-y-0.5 overflow-auto rounded-md border p-2">
              {candidates.map((id) => {
                const info = meta.get(id);
                return (
                  <label
                    key={id}
                    className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 text-xs hover:bg-muted/50"
                  >
                    <Checkbox
                      checked={models.includes(id)}
                      onCheckedChange={(next) => toggle(id, Boolean(next))}
                      disabled={readOnly}
                    />
                    <span className="truncate font-mono">{id}</span>
                    {info && <Badge variant="secondary">{info.name}</Badge>}
                    {info?.sees && <Badge variant="outline">收图</Badge>}
                    {info && (
                      <span className="text-[11px] text-muted-foreground">
                        {Math.round(info.context / 1000)}k
                      </span>
                    )}
                  </label>
                );
              })}
            </div>
          )}

          <div className="flex items-center gap-2">
            <Button size="sm" onClick={() => void save()} disabled={saving || readOnly}>
              {saving ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
              保存模型
            </Button>
            <span className="text-xs text-muted-foreground">已选 {models.length} 个</span>
            {readOnly && <span className="text-xs text-muted-foreground">演示模式下不可保存。</span>}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">调用示例</CardTitle>
          <CardDescription>
            用网关自己的 Key 调 8788，模型名带 <code className="font-mono">trae/</code> 前缀。
          </CardDescription>
        </CardHeader>
        <CardContent>
          <pre className="overflow-x-auto whitespace-pre-wrap break-all rounded-md border bg-muted/30 p-3 font-mono text-[11px] leading-relaxed text-muted-foreground">
{`curl http://127.0.0.1:8788/v1/chat/completions \\
  -H "Authorization: Bearer sk-wb-..." \\
  -H "Content-Type: application/json" \\
  -d '{"model":"trae/${models[0] ?? "glm-5.2"}","messages":[{"role":"user","content":"hi"}]}'`}
          </pre>
          <p className="mt-2 flex items-center gap-1.5 text-[11px] text-muted-foreground">
            <Activity className="size-3" />
            调用会记入网关请求台账，账号标签为 trae；用量来自 Trae 自己的 token 计数。
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
