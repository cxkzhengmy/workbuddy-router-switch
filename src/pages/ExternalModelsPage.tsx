import { useCallback, useEffect, useMemo, useState } from "react";
import { Cloud, Loader2, Plus, RefreshCw, Save, Trash2 } from "lucide-react";
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
 * 存在的原因是 OpenCode 的免费模型只能在它自己的客户端里用——直接打它的
 * Zen 端点会拿到 429「Free models work in OpenCode」。社区通行做法是在
 * 本地跑 opencode2api 这类代理（它借用 OpenCode CLI 的登录态），对外暴露
 * 标准 OpenAI 接口；这一页就是让网关去连那个代理。
 *
 * 所以 Base URL 是可配置的，不写死 opencode.ai：既能指向本地代理，也能
 * 接任何别的兼容服务。
 */
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
        </p>
      </div>

      <Alert>
        <Cloud className="size-4" />
        <AlertTitle>OpenCode 免费模型需要本地代理</AlertTitle>
        <AlertDescription className="space-y-1 text-xs leading-relaxed">
          <p>
            官方限制免费模型只能在 OpenCode 客户端内使用，直接请求 Zen 端点会返回{" "}
            <code className="font-mono">429 Free models work in OpenCode</code>。通行做法是在本机跑
            一个 <code className="font-mono">opencode2api</code> 这类代理，它借用 OpenCode CLI
            的登录态并对外暴露标准 OpenAI 接口。
          </p>
          <p>
            把下面的 Base URL 指向那个代理（例如 <code className="font-mono">http://127.0.0.1:10000/v1</code>
            ），再拉取并勾选模型即可。带余额的 console Key 也可以直接填{" "}
            <code className="font-mono">https://opencode.ai/inference/openai/v1</code>。
          </p>
        </AlertDescription>
      </Alert>

      <Card>
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <CardTitle className="text-base">接入配置</CardTitle>
            <div className="flex items-center gap-2">
              <span className="text-xs text-muted-foreground">
                {enabled ? "已启用" : "未启用"}
              </span>
              <Switch
                checked={enabled}
                onCheckedChange={(next) => {
                  setEnabled(next);
                  void save({ enabled: next });
                }}
              />
            </div>
          </div>
          <CardDescription>关掉后带前缀的模型会从列表中消失，请求返回 404。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div>
            <Label className="mb-1.5 block text-xs">Base URL</Label>
            <Input
              value={baseUrl}
              placeholder="https://opencode.ai/inference/openai/v1"
              onChange={(event) => setBaseUrl(event.target.value)}
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
              />
              {cfg?.hasKey && (
                <Button variant="outline" size="sm" onClick={() => void clearKey()}>
                  <Trash2 className="size-3.5" />
                  清除
                </Button>
              )}
            </div>
          </div>

          <div>
            <Label className="mb-1.5 block text-xs">User-Agent（可选）</Label>
            <Input value={userAgent} onChange={(event) => setUserAgent(event.target.value)} />
            <p className="mt-1 text-[11px] text-muted-foreground">
              部分上游会看 UA 判定客户端类型。
            </p>
          </div>

          <Button size="sm" onClick={() => void save()} disabled={saving}>
            {saving ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
            保存
          </Button>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <CardTitle className="text-base">模型</CardTitle>
            <Button variant="outline" size="sm" onClick={() => void pullModels()} disabled={fetching}>
              {fetching ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
              从上游拉取
            </Button>
          </div>
          <CardDescription>
            勾选的模型才会出现在 <code className="font-mono">/v1/models</code> 里。先保存再拉取。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-3">
            <label className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
              <Checkbox checked={freeOnly} onCheckedChange={(v) => setFreeOnly(Boolean(v))} />
              只看免费（{freeCount} 个）
            </label>
            {freeOnly && (
              <Button
                variant="ghost"
                size="sm"
                className="h-7 text-xs"
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
              disabled={!manual.trim()}
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
              还没有模型。点「从上游拉取」，或手动添加。
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
                  />
                  <span className="truncate font-mono">{id}</span>
                  {isFreeModel(id) && <Badge variant="secondary">免费</Badge>}
                </label>
              ))}
            </div>
          )}

          <div className="flex items-center gap-2">
            <Button size="sm" onClick={() => void save()} disabled={saving}>
              {saving ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
              保存模型
            </Button>
            <span className="text-xs text-muted-foreground">已选 {models.length} 个</span>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">调用示例</CardTitle>
          <CardDescription>任何 OpenAI 兼容客户端都能用，模型名要带前缀。</CardDescription>
        </CardHeader>
        <CardContent>
          <pre className="overflow-x-auto whitespace-pre-wrap break-all rounded-md border bg-muted/30 p-3 font-mono text-[11px] leading-relaxed text-muted-foreground">
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
