import { useCallback, useEffect, useMemo, useState } from "react";
import { CloudOff, Loader2, Plus, RefreshCw, Save, Trash2 } from "lucide-react";
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
import type { ProxyExternalConfig } from "@/lib/types";

/**
 * 外部模型页：把网关接到另一个 OpenAI 兼容上游。
 *
 * 历史上这是给 OpenCode 免费模型用的：官方限制免费模型只能在自家客户端里跑，
 * 所以设想是让网关去连本机一个借用 CLI 登录态的代理（opencode2api 之类），
 * 对外暴露标准 OpenAI 接口。
 *
 * **当前状态：不可用，界面只读。** 2026-11-05 的逐变量实测证明这条路走不通：
 * 上游要 Bun 的 TLS/HTTP2 指纹、system 里的 `<env>` 块、UA 版本 ≥1.18，
 * 且额度与官方 CLI 共享每日窗口；普通 OpenAI 客户端（curl / Node undici /
 * 本网关的 reqwest）怎么配都拿不到 200。详见仓库外维护手册与当日工作日志。
 *
 * 这里**只改界面**：不删代码、不清配置、不动网关后端。控件全部禁用，避免用户
 * 以为填对 URL/Key 就能用；但「已启用」的开关仍允许往关的方向拨一次，否则
 * 老配置会被锁死在开启状态、界面上再没有关闭入口。
 */
/** 不可用期间所有编辑控件统一走这个开关，将来通道恢复只改这一处。 */
const UNAVAILABLE = true;
/** OpenCode 的免费模型：-free 后缀，外加 big-pickle 这个 stealth 免费模型。 */
function isFreeModel(id: string): boolean {
  return id.endsWith("-free") || id === "big-pickle";
}

export default function ExternalModelsPage() {
  const [cfg, setCfg] = useState<ProxyExternalConfig | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [fetching, setFetching] = useState(false);
  const [remote, setRemote] = useState<string[] | null>(null);
  const [manual, setManual] = useState("");
  const [freeOnly, setFreeOnly] = useState(true);

  const [enabled, setEnabled] = useState(false);
  const [baseUrl, setBaseUrl] = useState("");
  const [userAgent, setUserAgent] = useState("");
  const [models, setModels] = useState<string[]>([]);
  const [apiKey, setApiKey] = useState("");

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const view = await api.proxyExternalConfig();
      setCfg(view);
      setEnabled(view.enabled);
      setBaseUrl(view.baseUrl);
      setUserAgent(view.userAgent);
      setModels(view.models);
      setApiKey("");
    } catch (error) {
      toast.error(`读取配置失败：${(error as Error).message}`);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async (patch?: { enabled?: boolean }) => {
    setSaving(true);
    try {
      const view = await api.proxySaveExternalConfig({
        enabled: patch?.enabled ?? enabled,
        baseUrl,
        models,
        userAgent,
        // 空字符串表示「不改」，后端会保留原 Key；要清空得发 "-"。
        ...(apiKey.trim() ? { apiKey: apiKey.trim() } : {}),
      });
      setCfg(view);
      setEnabled(view.enabled);
      setBaseUrl(view.baseUrl);
      setUserAgent(view.userAgent);
      setModels(view.models);
      setApiKey("");
      toast.success("已保存");
    } catch (error) {
      toast.error(`保存失败：${(error as Error).message}`);
    } finally {
      setSaving(false);
    }
  };

  const clearKey = async () => {
    try {
      const view = await api.proxySaveExternalConfig({ apiKey: "-" });
      setCfg(view);
      toast.success("已清除 Key");
    } catch (error) {
      toast.error(`清除失败：${(error as Error).message}`);
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

  /** 候选列表 = 远端 + 已启用（远端挂了也要能看到自己勾过的）。 */
  const candidates = useMemo(() => {
    const set = new Set<string>([...(remote ?? []), ...models]);
    const all = Array.from(set).sort();
    // 默认只留免费：OpenCode 的目录里 82 个模型只有 9 个免费，混在一起
    // 找起来很费劲。判据是 -free 后缀，加上 big-pickle 这个 stealth 免费模型。
    return freeOnly ? all.filter(isFreeModel) : all;
  }, [remote, models, freeOnly]);

  const freeCount = useMemo(
    () => Array.from(new Set([...(remote ?? []), ...models])).filter(isFreeModel).length,
    [remote, models],
  );

  const toggle = (id: string, on: boolean) => {
    setModels((prev) => (on ? Array.from(new Set([...prev, id])) : prev.filter((x) => x !== id)));
  };

  if (loading) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-40 w-full" />
        <Skeleton className="h-56 w-full" />
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div>
        <h1 className="text-xl font-semibold tracking-tight">外部模型</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          把网关接到另一个 OpenAI 兼容上游。模型会以 <code className="font-mono">opencode/&lt;id&gt;</code>{" "}
          的形式出现在 <code className="font-mono">/v1/models</code> 里，与 WorkBuddy 自己的模型并存。
          <span className="ml-1 font-medium text-destructive">该通道当前不可用，下方配置仅为存档。</span>
        </p>
      </div>

      <Alert variant="destructive" className="border-destructive/40 bg-destructive/5">
        <CloudOff className="size-4" />
        <AlertTitle>外部模型当前不可用</AlertTitle>
        <AlertDescription className="space-y-2 text-xs leading-relaxed">
          <p>
            这一页现在只展示已保存的配置，编辑、保存、拉取和开启入口都已停用。原因是上游的免费层
            并不是一个「填对 URL 和 Key 就能调」的普通 OpenAI 端点，实测有四道绕不过的限制：
          </p>
          <ul className="list-disc space-y-1 pl-4">
            <li>
              <strong>免费层限制</strong>：官方只允许 OpenCode 自家客户端调用，直接请求
              <code className="font-mono"> opencode.ai/inference/openai/v1 </code>
              会拿到 <code className="font-mono">429 Free models work in OpenCode</code>。
            </li>
            <li>
              <strong>TLS 指纹</strong>：上游只放行 Bun 的 TLS/HTTP2 栈。同机同 IP、同请求体，
              Bun 能 200，curl / Node undici / 普通 SDK 一律 429；换 HTTP 代理也没用，
              隧道里握手的仍是客户端自己的指纹。
            </li>
            <li>
              <strong>UA 版本闸</strong>：低于 1.18 的写法（例如当前配置里的
              <code className="font-mono"> opencode/0.15.0</code>）会先命中
              <code className="font-mono"> 426 UpgradeRequired</code>，这道闸排在额度闸之前。
            </li>
            <li>
              <strong>共享每日额度</strong>：与官方 CLI 共用窗口，00:00 UTC（北京时间 08:00）重置。
              全模型同时 429 通常是额度或指纹问题，不是配置写错。
            </li>
          </ul>
          <p>
            唯一可行的路线是在本机跑一个能复用 OpenCode CLI 登录态、并满足上述指纹的代理，
            目前没有可用实现。网关后端逻辑未改动、已保存的配置也不会被清除；若此前已启用，
            仍可用右侧开关把它关掉（关掉后无法再从界面开启）。
          </p>
        </AlertDescription>
      </Alert>

      <Card>
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <CardTitle className="text-base">接入配置</CardTitle>
            <div className="flex items-center gap-2">
              <span className="text-xs text-muted-foreground">
                {UNAVAILABLE ? (enabled ? "已启用（仅可关闭）" : "当前不可用") : enabled ? "已启用" : "未启用"}
              </span>
              <Switch
                checked={enabled}
                // 未启用的不许开启；已启用的留一次关闭机会，别把老配置锁死。
                disabled={UNAVAILABLE && !enabled}
                onCheckedChange={(next) => {
                  setEnabled(next);
                  void save({ enabled: next });
                }}
              />
            </div>
          </div>
          <CardDescription>
            该通道当前不可用，配置仅供查看。关掉后带前缀的模型会从列表中消失，请求返回 404。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div>
            <Label className="mb-1.5 block text-xs">Base URL</Label>
            <Input
              value={baseUrl}
              placeholder="https://opencode.ai/inference/openai/v1"
              onChange={(event) => setBaseUrl(event.target.value)}
              disabled={UNAVAILABLE}
            />
            <p className="mt-1 text-[11px] text-muted-foreground">
              指向兼容 OpenAI 的 /v1 根路径，不要带 /chat/completions。
            </p>
          </div>

          <div>
            <Label className="mb-1.5 block text-xs">
              API Key
              {cfg?.hasKey && (
                <span className="ml-2 text-muted-foreground">
                  已保存（尾号 {cfg.keyTail || "****"}）
                </span>
              )}
            </Label>
            <div className="flex gap-2">
              <Input
                type="password"
                value={apiKey}
                placeholder={cfg?.hasKey ? "留空表示不修改" : "本地代理一般留空"}
                onChange={(event) => setApiKey(event.target.value)}
                disabled={UNAVAILABLE}
              />
              {cfg?.hasKey && (
                <Button variant="outline" size="sm" onClick={() => void clearKey()} disabled={UNAVAILABLE}>
                  <Trash2 className="size-3.5" />
                  清除
                </Button>
              )}
            </div>
          </div>

          <div>
            <Label className="mb-1.5 block text-xs">User-Agent（可选）</Label>
            <Input value={userAgent} onChange={(event) => setUserAgent(event.target.value)} disabled={UNAVAILABLE} />
            <p className="mt-1 text-[11px] text-muted-foreground">
              部分上游会看 UA 判定客户端类型；低于 1.18 的写法会先被 426 拦下。
            </p>
          </div>

          <Button size="sm" onClick={() => void save()} disabled={saving || UNAVAILABLE}>
            {saving ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
            保存
          </Button>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <CardTitle className="text-base">模型</CardTitle>
            <Button variant="outline" size="sm" onClick={() => void pullModels()} disabled={fetching || UNAVAILABLE}>
              {fetching ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
              从上游拉取
            </Button>
          </div>
          <CardDescription>
            勾选的模型才会出现在 <code className="font-mono">/v1/models</code> 里。通道不可用，此处仅供查看。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-3">
            <label className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
              <Checkbox
                checked={freeOnly}
                onCheckedChange={(v) => setFreeOnly(Boolean(v))}
                disabled={UNAVAILABLE}
              />
              只看免费（{freeCount} 个）
            </label>
            {freeOnly && (
              <Button
                variant="ghost"
                size="sm"
                className="h-7 text-xs"
                disabled={UNAVAILABLE}
                onClick={() =>
                  setModels((prev) =>
                    Array.from(new Set([...prev, ...candidates.filter(isFreeModel)])),
                  )
                }
              >
                全选免费
              </Button>
            )}
          </div>

          <div className="flex gap-2">
            <Input
              value={manual}
              placeholder="手动添加模型 id，例如 big-pickle"
              disabled={UNAVAILABLE}
              onChange={(event) => setManual(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && manual.trim()) {
                  toggle(manual.trim(), true);
                  setManual("");
                }
              }}
            />
            <Button
              variant="outline"
              size="sm"
              disabled={UNAVAILABLE || !manual.trim()}
              onClick={() => {
                toggle(manual.trim(), true);
                setManual("");
              }}
            >
              <Plus className="size-4" />
              添加
            </Button>
          </div>

          {candidates.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              没有已保存的模型。通道不可用期间不再提供拉取与添加入口。
            </p>
          ) : (
            <div className="max-h-64 space-y-0.5 overflow-auto rounded-md border p-2">
              {candidates.map((id) => (
                <label
                  key={id}
                  className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 text-xs hover:bg-muted/50"
                >
                  <Checkbox
                    checked={models.includes(id)}
                    onCheckedChange={(next) => toggle(id, Boolean(next))}
                    disabled={UNAVAILABLE}
                  />
                  <span className="truncate font-mono">{id}</span>
                  {isFreeModel(id) && <Badge variant="secondary">免费</Badge>}
                </label>
              ))}
            </div>
          )}

          <div className="flex items-center gap-2">
            <Button size="sm" onClick={() => void save()} disabled={saving || UNAVAILABLE}>
              {saving ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
              保存模型
            </Button>
            <span className="text-xs text-muted-foreground">已存 {models.length} 个（只读）</span>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">调用示例</CardTitle>
          <CardDescription>
            仅供留档，当前上游不可用、示例请求不会成功。模型名原本要带前缀。
          </CardDescription>
        </CardHeader>
        <CardContent>
          <pre className="overflow-x-auto whitespace-pre-wrap break-all rounded-md border bg-muted/30 p-3 font-mono text-[11px] leading-relaxed text-muted-foreground opacity-60">
{`curl http://127.0.0.1:8788/v1/chat/completions \\
  -H "Authorization: Bearer sk-wb-..." \\
  -H "Content-Type: application/json" \\
  -d '{"model":"opencode/${models[0] ?? "big-pickle"}","messages":[{"role":"user","content":"hi"}]}'`}
          </pre>
        </CardContent>
      </Card>
    </div>
  );
}
