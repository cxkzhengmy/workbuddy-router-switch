#!/usr/bin/env node
// 发版后验签：确认安装包确实由 tauri.conf.json 中配置的公钥对应私钥签出。
//
// 用法：
//   node scripts/verify-release-signature.mjs                      # 自动找当前版本的 nsis 安装包
//   node scripts/verify-release-signature.mjs <安装包路径> [更多路径...]
//
// 为什么需要这一步：签名与公钥不配对时，客户端能下载、能校验版本号，
// 只有最后一步验签失败，用户只看到一句「安装失败」。这种情况必须在
// 发布当时就查出来，不能等用户反馈。
//
// Tauri 的签名文件有两层封装，解析时容易踩坑（都实测过）：
//   1. .sig 与 .pub 文件**整体是 base64**，里面包的才是 minisign 明文。
//      直接按明文解析会把 "untrusted " 当成算法名，keyId 解出乱码。
//   2. 全局签名的被签内容是 `64 字节纯签名 + trusted comment 内容`，
//      **不含** "trusted comment: " 前缀；用 74 字节（含 alg+keyId）也验不过。
//   3. 公钥 alg 恒为 "Ed"，签名 alg 为 "ED"（prehashed）。两者不相等是正常的，
//      不能拿它们做配对判据。真正的判据是 keyId 相等 + 主签名实测通过。

import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const CONF = path.join(ROOT, 'src-tauri/tauri.conf.json');
const LOCAL_PUB = path.join(
  process.env.HOME || process.env.USERPROFILE || '',
  '.wb-switch/wb-switch-updater.key.pub'
);

let failures = 0;
function check(label, ok, detail = '') {
  console.log(`  [${ok ? 'PASS' : 'FAIL'}] ${label}${detail ? '  ' + detail : ''}`);
  if (!ok) failures++;
}

// 外层 base64 解出 minisign 明文
function unwrap(text) {
  return Buffer.from(text.trim(), 'base64').toString('utf8');
}

function parsePublicKey(pubkeyText) {
  const lines = unwrap(pubkeyText).split(/\r?\n/).filter((l) => l.trim());
  const b64 = lines.find((l) => !l.startsWith('untrusted comment'));
  if (!b64) throw new Error('公钥内容里找不到 base64 行');
  const raw = Buffer.from(b64.trim(), 'base64');
  if (raw.length < 42) throw new Error(`公钥长度异常：${raw.length} 字节（期望 42）`);
  return {
    alg: raw.subarray(0, 2).toString('ascii'),
    keyId: raw.subarray(2, 10),
    pub: raw.subarray(10, 42),
  };
}

function parseSignature(sigText) {
  const lines = unwrap(sigText).split(/\r?\n/);
  const mainB64 = lines.find(
    (l) => l.trim() && !l.startsWith('untrusted comment') && !l.startsWith('trusted comment')
  );
  if (!mainB64) throw new Error('签名内容里找不到主签名行');
  const raw = Buffer.from(mainB64.trim(), 'base64');
  if (raw.length < 74) throw new Error(`主签名长度异常：${raw.length} 字节（期望 74）`);

  const sig64 = raw.subarray(10, 74);
  const tIdx = lines.findIndex((l) => l.startsWith('trusted comment'));
  const trustedLine = tIdx >= 0 ? lines[tIdx] : '';
  const trusted = trustedLine.replace('trusted comment: ', '');
  const globalB64 = tIdx >= 0 ? lines[tIdx + 1] || '' : '';
  const globalSig = globalB64 ? Buffer.from(globalB64.trim(), 'base64') : null;

  return {
    alg: raw.subarray(0, 2).toString('ascii'),
    keyId: raw.subarray(2, 10),
    sig: sig64,
    trusted,
    globalSig,
    globalMsg: Buffer.concat([sig64, Buffer.from(trusted, 'utf8')]),
  };
}

// 32 字节 raw Ed25519 公钥 -> SPKI DER
function ed25519Key(pub32) {
  return crypto.createPublicKey({
    key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), pub32]),
    format: 'der',
    type: 'spki',
  });
}

function findBundles() {
  const conf = JSON.parse(fs.readFileSync(CONF, 'utf8'));
  const version = conf.version;
  const dir = path.join(ROOT, 'target/release/bundle/nsis');
  if (!fs.existsSync(dir)) return { version, files: [] };
  const files = fs
    .readdirSync(dir)
    .filter((f) => f.includes(version) && f.endsWith('.exe'))
    .map((f) => path.join(dir, f));
  return { version, files };
}

function main() {
  console.log('=== 1. 公钥来源 ===');
  const conf = JSON.parse(fs.readFileSync(CONF, 'utf8'));
  const confPubkey = conf.plugins?.updater?.pubkey;
  if (!confPubkey) {
    console.log('  [FAIL] tauri.conf.json 里没有 plugins.updater.pubkey，客户端无法验签');
    process.exit(1);
  }
  check('tauri.conf.json 里有 pubkey', true, `版本 ${conf.version}`);

  const confClean = confPubkey.replace(/\\n/g, '\n').trim();
  check(
    '配置里的 pubkey 是 base64 文本（不是文件路径）',
    !confClean.includes('/') && !confClean.includes('\\'),
    `${confClean.length} 字符`
  );

  if (fs.existsSync(LOCAL_PUB)) {
    const localText = fs.readFileSync(LOCAL_PUB, 'utf8').trim();
    check('与本机 ~/.wb-switch/*.key.pub 一致', confClean === localText,
      confClean === localText ? '' : '不一致：换过 key 却没同步进配置？');
  } else {
    console.log('  [SKIP] 本机没有 ~/.wb-switch/*.key.pub，跳过交叉核对');
  }

  const pub = parsePublicKey(confClean);
  console.log('  公钥 alg  :', pub.alg);
  console.log('  公钥 keyId:', pub.keyId.toString('hex').toUpperCase());
  check('公钥 alg 为 Ed', pub.alg === 'Ed', pub.alg);

  const key = ed25519Key(pub.pub);

  // 选出待验的安装包
  let targets = process.argv.slice(2).filter((a) => !a.startsWith('--'));
  if (targets.length === 0) {
    const { version, files } = findBundles();
    if (files.length === 0) {
      console.log(`\n找不到 ${version} 的 nsis 安装包，请先构建，或显式传入安装包路径。`);
      process.exit(1);
    }
    targets = files;
  }

  for (const exe of targets) {
    console.log(`\n=== 2. 验签 ${path.basename(exe)} ===`);
    if (!fs.existsSync(exe)) {
      check('安装包存在', false, exe);
      continue;
    }
    const sigPath = `${exe}.sig`;
    if (!fs.existsSync(sigPath)) {
      check(`签名文件存在（${path.basename(sigPath)}）`, false, '没有 .sig 就无法签发更新清单');
      continue;
    }

    const data = fs.readFileSync(exe);
    const sig = parseSignature(fs.readFileSync(sigPath, 'utf8'));
    console.log('  签名 alg  :', sig.alg);
    console.log('  签名 keyId:', sig.keyId.toString('hex').toUpperCase());
    console.log('  trusted   :', sig.trusted);
    console.log('  安装包    :', data.length, '字节');

    check('签名 alg 为 ED/Ed', ['Ed', 'ED'].includes(sig.alg), sig.alg);
    check('keyId 一致（公钥私钥成对）', pub.keyId.equals(sig.keyId));

    const digest = crypto.createHash('blake2b512').update(data).digest();
    let mainOk = false;
    try { mainOk = crypto.verify(null, digest, key, sig.sig); } catch { /* 落到下面的 FAIL */ }
    check('主签名验证通过（Ed25519 over blake2b512）', mainOk);

    if (sig.globalSig && sig.globalSig.length === 64) {
      let globalOk = false;
      try { globalOk = crypto.verify(null, sig.globalMsg, key, sig.globalSig); } catch { /* ignore */ }
      check('全局签名验证通过（trusted comment 未被篡改）', globalOk);
    } else {
      console.log('  [SKIP] 未取到全局签名');
    }

    const fileInSig = (sig.trusted.match(/file:(.+)$/) || [])[1] || '';
    if (fileInSig) {
      check('签名内文件名与安装包一致', fileInSig.trim() === path.basename(exe), fileInSig.trim());
    }
  }

  console.log('\n=== 结论 ===');
  if (failures === 0) {
    console.log('  签名与配置公钥真实配对，客户端下载后能验签通过。');
    process.exit(0);
  }
  console.log(`  有 ${failures} 项未通过 —— 客户端会报「安装失败」，不要发布。`);
  process.exit(1);
}

main();
