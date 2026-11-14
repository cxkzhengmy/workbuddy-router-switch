import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Activity,
  CircleAlert,
  CircleCheck,
  ExternalLink,
  Info,
  Loader2,
  Plus,
  RefreshCw,
  Save,
  Trash2,
  WifiOff,
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
import type { ProxyExternalConfig, ProxyExternalHealth } from "@/lib/types";

/**
 * 外部模型页：把内置网关接到本机 doubao2api（豆包反代）。
 *
 * 1.0.14 起整条通道换了实现。旧的 OpenCode 公网免费层在 1.0.12 判定不可用
 * （Bun TLS 指纹 / UA 版本闸 / 共享每日额度），界面只读存档；现在上游换成
 * 本机 127.0.0.1:9090 的 OpenAI 兼容服务，没有指纹问题，配置页恢复可用。
 *
 * 只代理聊天：图片 / 视频 / 音乐生成端点不经过网关，生成类模型在这里被标出来
 * 且不可勾选（Rust 侧 save/forward 也会二次拦截）。千问系模型是上游另一条
 * 可选通道，默认部署没开 QIANWEN_ENABLED，因此只做提示、不硬禁。
 */

/** 与 Rust `external::is_generation_model` 保持同一判据。 */
function isGenerationModel(id: string): boolean {
  const lower = id.toLowerCase();
  return lower.endsWith("-image") || lower.endsWith("-video") || lower.endsWith("-music");
}

/** 上游目录里的千问系模型，走另一套客户端，需要单独开 QIANWEN_ENABLED。 */
function isQianwenModel(id: string): boolean {
  return /^(qianwen|qwen)/i.test(id);
}

type ModelKind = "chat" | "aux" | "generation";

function modelKind(id: string): ModelKind {
  if (isGenerationModel(id)) return "generation";
  if (isQianwenModel(id)) return "aux";
  return "chat";
}

export default function ExternalModelsPage() {
  const [cfg, setCfg] = useState<ProxyExternalConfig | null>(null);
  const [health, setHealth] = useState<ProxyExternalHealth | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [fetching, setFetching] = useState(false);
  const [checking, setChecking] = useState(false);
  const [remote, setRemote] = useState<string[] | null>(null);
  const [manual, setManual] = useState("");
  const [chatOnly, setChatOnly] = useState(true);

  const [enabled, setEnabled] = useState(false);
  const [baseUrl, setBaseUrl] = useState("");
  const [userAgent, setUserAgent] = useState("");
  const [models, setModels] = useState<string[]>([]);
  const [apiKey, setApiKey] = useState("");

  const readOnly = api.isDemoMode();

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

  const checkHealth = useCallback(async () => {
    setChecking(true);
    try {
      setHealth(await api.proxyExternalHealth());
    } catch (error) {
      setHealth(null);
      toast.error(`探测服务状态失败：${(error as Error).message}`);
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    void load();
    void checkHealth();
  }, [load, checkHealth]);

  const save = async (patch?: { enabled?: boolean }) => {
    setSaving(true);
    try {
      // 生成类模型网关不代理，提交前先滤掉，免得后端报错让用户来回改。
      const clean = models.filter((id) => !isGenerationModel(id));
      if (clean.length !== models.length) {
        toast.message("已自动去掉生成类模型：网关只代理对话请求");
        setModels(clean);
      }
      const view = await api.proxySaveExternalConfig({
        enabled: patch?.enabled ?? enabled,
        baseUrl,
        models: clean,
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
      void checkHealth();
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
      void checkHealth();
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

  /** 候选列表 = 远端 + 已启用（服务挂了也要能看到自己勾过的）。 */
  const allCandidates = useMemo(
    () => Array.from(new Set([...(remote ?? []), ...models])).sort(),
    [remote, models],
  );
  const candidates = useMemo(
    () => (chatOnly ? allCandidates.filter((id) => modelKind(id) === "chat") : allCandidates),
    [allCandidates, chatOnly],
  );
  const chatCount = useMemo(
    () => allCandidates.filter((id) => modelKind(id) === "chat").length,
    [allCandidates],
  );

  const toggle = (id: string, on: boolean) => {
    setModels((prev) => (on ? Array.from(new Set([...prev, id])) : prev.filter((x) => x !== id)));
  };

  const addManual = () => {
    const id = manual.trim();
    if (!id) return;
    if (isGenerationModel(id)) {
      toast.error(`${id} 是生成类模型，网关只代理对话请求`);
      return;
    }
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

  const healthBadge = !health
    ? { variant: "secondary" as const, text: "未探测" }
    : health.ok
      ? { variant: "default" as const, text: "可用" }
      : health.reachable
        ? { variant: "destructive" as const, text: "需处理" }
        : { variant: "destructive" as const, text: "未启动" };

  return (
    <div className="space-y-4">
      <div>
        <h1 className="text-xl font-semibold tracking-tight">外部模型</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          把内置网关接到本机 doubao2api（豆包反代）。模型会以{" "}
          <code className="font-mono">{cfg?.prefix ?? "doubao/"}&lt;id&gt;</code> 的形式出现在{" "}
          <code className="font-mono">/v1/models</code> 里，与 WorkBuddy 自己的模型并存。
        </p>
      </div>

      <Alert className="border-sky-500/30 bg-sky-500/5">
        <Info className="size-4" />
        <AlertTitle>三步走：启动服务 → 扫码登录 → 在这里启用</AlertTitle>
        <AlertDescription className="text-xs leading-relaxed">
          <span>
            上游是本机 <code className="font-mono">http://127.0.0.1:9090/v1</code> 的 Python 服务，
            靠浏览器登录态调用豆包网页版。服务在跑不等于能用：首次使用、Cookie 过期或命中风控时，
            都要到 Admin 面板用豆包 App 扫码 / 处理验证码。网关只转发{" "}
            <code className="font-mono">/v1/chat/completions</code>，图片、视频、音乐生成端点请直连 9090。
          </span>
        </AlertDescription>
      </Alert>

      <Card>
        <CardHeader>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-2">
              <CardTitle className="text-base">服务状态</CardTitle>
              <Badge variant={healthBadge.variant}>{healthBadge.text}</Badge>
            </div>
            <div className="flex items-center gap-2">
              <Button variant="outline" size="sm" onClick={() => void checkHealth()} disabled={checking}>
                {checking ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
                重新探测
              </Button>
              {cfg?.adminLink && (
                <Button variant="outline" size="sm" asChild>
                  <a href={cfg.adminLink} target="_blank" rel="noreferrer">
                    <ExternalLink className="size-3.5" />
                    Admin 面板
                  </a>
                </Button>
              )}
            </div>
          </div>
          <CardDescription>探测 {health?.url ?? `${baseUrl.replace(/\/v1$/, "")}/health`} 的在线与登录状态。</CardDescription>
        </CardHeader>
        <CardContent>
          <div className="flex items-start gap-2 text-sm">
            {health?.ok ? (
              <CircleCheck className="mt-0.5 size-4 shrink-0 text-emerald-600" />
            ) : health?.reachable ? (
              <CircleAlert className="mt-0.5 size-4 shrink-0 text-amber-600" />
            ) : (
              <WifiOff className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
            )}
            <div className="min-w-0">
              <p>{health?.message ?? "尚未取得服务状态，点「重新探测」重试。"}</p>
              {health && (
                <div className="mt-1.5 flex flex-wrap gap-1.5 text-[11px] text-muted-foreground">
                  <Badge variant="outline">在线 {health.reachable ? "是" : "否"}</Badge>
                  <Badge variant="outline">
                    已登录 {health.loggedIn === null ? "未知" : health.loggedIn ? "是" : "否"}
                  </Badge>
                  <Badge variant="outline">
                    风控验证码 {health.needsCaptcha === null ? "未知" : health.needsCaptcha ? "需要" : "无"}
                  </Badge>
                  {health.statusText && <Badge variant="outline">{health.statusText}</Badge>}
                </div>
              )}
            </div>
          </div>
          {cfg?.adminUrl && (
            <p className="mt-3 text-[11px] text-muted-foreground">
              Admin 面板：<code className="font-mono">{cfg.adminUrl}</code>
              {cfg.hasKey && "（上面的按钮已自动带上 API Key；手动打开时需在地址后加 ?key=你的 Key）"}
            </p>
          )}
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
                disabled={readOnly}
                onCheckedChange={(next) => {
                  setEnabled(next);
                  void save({ enabled: next });
                }}
              />
            </div>
          </div>
          <CardDescription>
            关掉后带前缀的模型会从网关列表里消失，相关请求返回 404。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div>
            <Label className="mb-1.5 block text-xs">Base URL</Label>
            <Input
              value={baseUrl}
              placeholder="http://127.0.0.1:9090/v1"
              onChange={(event) => setBaseUrl(event.target.value)}
              disabled={readOnly}
            />
            <p className="mt-1 text-[11px] text-muted-foreground">
              指向 doubao2api 的 /v1 根路径，不要带 /chat/completions。默认只监听 127.0.0.1。
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
                placeholder={cfg?.hasKey ? "留空表示不修改" : "填 doubao2api 的 DOUBAO_API_KEY"}
                onChange={(event) => setApiKey(event.target.value)}
                disabled={readOnly}
              />
              {cfg?.hasKey && (
                <Button variant="outline" size="sm" onClick={() => void clearKey()} disabled={readOnly}>
                  <Trash2 className="size-3.5" />
                  清除
                </Button>
              )}
            </div>
            <p className="mt-1 text-[11px] text-muted-foreground">
              上游用 <code className="font-mono">DOUBAO_API_KEY</code> 保护 /v1/*；留空部署时这里也可以留空。
            </p>
          </div>

          <div>
            <Label className="mb-1.5 block text-xs">User-Agent（可选）</Label>
            <Input value={userAgent} onChange={(event) => setUserAgent(event.target.value)} disabled={readOnly} />
            <p className="mt-1 text-[11px] text-muted-foreground">
              doubao2api 不校验 UA，这里只是沿用 OpenAI 客户端的通用配置项。
            </p>
          </div>

          <div className="flex items-center gap-2">
            <Button size="sm" onClick={() => void save()} disabled={saving || readOnly}>
              {saving ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
              保存
            </Button>
            {readOnly && <span className="text-xs text-muted-foreground">演示模式下不可保存。</span>}
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
                onClick={() => setModels((prev) => Array.from(new Set([...prev, ...(cfg?.chatModelPreset ?? [])])))}
                disabled={readOnly || !cfg?.chatModelPreset.length}
              >
                <Plus className="size-3.5" />
                一键填入对话模型
              </Button>
              <Button variant="outline" size="sm" onClick={() => void pullModels()} disabled={fetching}>
                {fetching ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
                从上游拉取
              </Button>
            </div>
          </div>
          <CardDescription>
            勾选的模型才会出现在网关 <code className="font-mono">/v1/models</code> 里。生成类模型不代理。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-3">
            <label className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
              <Checkbox checked={chatOnly} onCheckedChange={(v) => setChatOnly(Boolean(v))} />
              只看可代理的对话模型（{chatCount} 个）
            </label>
            {chatOnly && allCandidates.length > chatCount && (
              <span className="text-[11px] text-muted-foreground">
                另有 {allCandidates.length - chatCount} 个生成 / 千问模型已隐藏
              </span>
            )}
          </div>

          <div className="flex gap-2">
            <Input
              value={manual}
              placeholder="手动添加模型 id，例如 doubao-pro"
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
              还没有模型。点「一键填入对话模型」或「从上游拉取」。
            </p>
          ) : (
            <div className="max-h-64 space-y-0.5 overflow-auto rounded-md border p-2">
              {candidates.map((id) => {
                const kind = modelKind(id);
                const blocked = kind === "generation";
                return (
                  <label
                    key={id}
                    className={`flex items-center gap-2 rounded px-1.5 py-1 text-xs ${
                      blocked ? "cursor-not-allowed opacity-55" : "cursor-pointer hover:bg-muted/50"
                    }`}
                  >
                    <Checkbox
                      checked={models.includes(id)}
                      onCheckedChange={(next) => toggle(id, Boolean(next))}
                      disabled={readOnly || blocked}
                    />
                    <span className="truncate font-mono">{id}</span>
                    {kind === "generation" && <Badge variant="destructive">生成类 · 不代理</Badge>}
                    {kind === "aux" && <Badge variant="outline">千问通道 · 需上游开启</Badge>}
                    {kind === "chat" && <Badge variant="secondary">对话</Badge>}
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
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">调用示例</CardTitle>
          <CardDescription>
            用网关自己的 Key 调 8788，模型名带 <code className="font-mono">doubao/</code> 前缀。
          </CardDescription>
        </CardHeader>
        <CardContent>
          <pre className="overflow-x-auto whitespace-pre-wrap break-all rounded-md border bg-muted/30 p-3 font-mono text-[11px] leading-relaxed text-muted-foreground">
{`curl http://127.0.0.1:8788/v1/chat/completions \\
  -H "Authorization: Bearer sk-wb-..." \\
  -H "Content-Type: application/json" \\
  -d '{"model":"doubao/${models[0] ?? "doubao"}","messages":[{"role":"user","content":"hi"}]}'`}
          </pre>
          <p className="mt-2 flex items-center gap-1.5 text-[11px] text-muted-foreground">
            <Activity className="size-3" />
            调用会记入网关请求台账，账号标签为 doubao；用量由上游返回的 usage 决定。
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
