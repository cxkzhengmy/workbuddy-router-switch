// OpenCode 免费通道探针
//
// 用途：验证 opencode.ai 的匿名免费通道当前是否可用。
// 机制（依据 opencode2dsh PR #110 的实测）：
//   POST https://opencode.ai/zen/v1/chat/completions
//   Authorization: Bearer public          ← 匿名通道的 key 就是字面量 public
//   x-opencode-* 一组 CLI 头 + 请求体里带 <env> 区块
//   必须用 Bun 的 fetch（服务端会看 TLS/HTTP2 指纹，Node/reqwest 会被判为非法客户端）
//
// 跑：bun run scripts/opencode-free-probe.ts
// 预期：
//   200 → 通道可用，可以接入网关
//   429 FreeUsageLimitError → 该出口 IP 的匿名额度已耗尽，等重置
//   400 Model is unavailable → 模型名不对（Zen 端点要裸 id，不带 opencode/ 前缀）

const ENDPOINT = "https://opencode.ai/zen/v1/chat/completions";
const UA = "opencode/2.2.0";

function headers(message: string): Record<string, string> {
  // CLI 的 session/project id 由首条用户消息的 SHA-256 派生（同一对话稳定、不可逆）
  const sid = new Bun.CryptoHasher("sha256").update(message).digest("hex").slice(0, 32);
  const req = crypto.randomUUID().replace(/-/g, "");
  return {
    Authorization: "Bearer public",
    "Content-Type": "application/json",
    "User-Agent": UA,
    "x-opencode-client": "cli",
    "x-opencode-session": sid,
    "x-session-affinity": sid,
    "X-Session-Id": sid,
    "x-opencode-request": req,
    "x-opencode-project": sid,
  };
}

const ENV_BLOCK = `<env>
  Working directory: ${process.cwd()}
  Is directory a git repo: no
  Platform: ${process.platform}
  Today's date: ${new Date().toDateString()}
</env>`;

async function probe(model: string): Promise<void> {
  const userTurn = "只回复两个字：收到";
  const body = {
    model, // 裸 id，不要带 opencode/ 前缀
    messages: [
      { role: "system", content: `You are opencode, an interactive CLI tool.\n\n${ENV_BLOCK}` },
      { role: "user", content: userTurn },
    ],
    max_tokens: 32,
    stream: false,
  };
  try {
    const resp = await fetch(ENDPOINT, {
      method: "POST",
      headers: headers(userTurn),
      body: JSON.stringify(body),
    });
    const text = await resp.text();
    const brief = text.replace(/\s+/g, " ").slice(0, 160);
    const mark = resp.status === 200 ? "✓" : "✗";
    console.log(`${mark} ${model.padEnd(30)} HTTP ${resp.status}  ${brief}`);
    if (resp.status === 200) {
      try {
        const json = JSON.parse(text);
        const content = json?.choices?.[0]?.message?.content;
        if (content) console.log(`    回复: ${content.slice(0, 80)}`);
      } catch {
        /* 非 JSON 就跳过 */
      }
    }
  } catch (error) {
    console.log(`✗ ${model.padEnd(30)} 连接失败: ${(error as Error).message}`);
  }
}

// 免费模型（-free 后缀 + big-pickle 这个 stealth 免费模型）
const FREE_MODELS = [
  "big-pickle",
  "glm-5.3-flash-free",
  "deepseek-v4.1-flash-free",
  "mimo-v2.6-flash-free",
  "nemotron-3.5-lightning-free",
];

console.log("OpenCode 匿名免费通道探针\n");
for (const model of FREE_MODELS) {
  await probe(model);
}
console.log("\n提示：429 表示该出口 IP 的额度耗尽（按 IP 计），换网络或等重置再试。");
