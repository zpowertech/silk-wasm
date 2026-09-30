# silk-wasm

在浏览器里**直接播放 SILK 音频文件**（微信/QQ 语音常用的 `.slk` / `.silk` 格式）。

解码器完全用 **Rust** 实现并编译为 **WebAssembly**：不需要服务端转码，不需要
Emscripten，不需要 `ffmpeg`，也不需要 `wasm-bindgen` 命令行工具。产物只有一个
约 110 KB 的 `.wasm` 文件加一个 ESM 封装，直接 `import` 即可使用。

```
┌─────────────┐   .slk 字节流   ┌──────────────────┐   PCM 16-bit   ┌───────────┐
│ fetch / 本地 │ ─────────────▶ │ silk_core.wasm   │ ─────────────▶ │ Web Audio │
│  文件读取    │                │ (Rust 解码器)     │   单声道        │  播放     │
└─────────────┘                └──────────────────┘                └───────────┘
```

## 快速开始

```bash
./build.sh                    # 编译 wasm 并输出到 web/silk_core.wasm
python3 -m http.server 8000 --directory web
# 浏览器打开 http://localhost:8000/index.html
```

演示页面支持：**选择本地 `.slk` 文件**、**从 URL 加载**、波形显示、播放/停止、
**边下边播（流式）**、导出 WAV、音量调节。

## 在代码中使用

```js
import { loadSilk, playSilk } from './web/silk.js';

const silk = await loadSilk();                       // 加载 ./silk_core.wasm
const bytes = new Uint8Array(
  await (await fetch('https://example.com/voice.slk')).arrayBuffer()
);

const audio = silk.decode(bytes);                    // 一次性解码
console.log(audio.sampleRate, audio.duration, audio.samples.length);

await playSilk(audio);                               // 浏览器中播放
```

### 流式播放（下载未完成即可开始播放）

```js
import { loadSilk, createSilkStreamPlayer } from './web/silk.js';

const silk = await loadSilk();
const bytes = new Uint8Array(await (await fetch(url)).arrayBuffer());

const player = await createSilkStreamPlayer(silk, { gain: 1 });
for (const packet of silk.packets(bytes)) {
  player.push(packet);      // 每个包解出 20 ms 音频并立即排入播放队列
}
player.end();
await player.finished;
```

更底层的增量接口：

```js
const stream = silk.stream();
const packets = silk.packets(bytes);
for (const p of packets) {
  const pcm = stream.decode(p);      // Int16Array，单声道
  feed(pcm, stream.sampleRate);
}

// 或者让解码器自己遍历整个文件
for (;;) {
  const pcm = stream.decodeNext(bytes);
  if (pcm.length === 0) break;
  feed(pcm, stream.sampleRate);
}
stream.dispose();
```

## API

### `loadSilk(options?) → Promise<Silk>`

加载 wasm 模块。

| 选项 | 说明 |
| --- | --- |
| `wasmUrl` | 覆盖 `.wasm` 位置（默认与本模块同目录） |
| `wasmBytes` | 直接传入 wasm 字节，完全不发起请求（适合打包器/离线场景） |

### `Silk`

| 成员 | 说明 |
| --- | --- |
| `isSilk(bytes)` | 是否是 SILK 文件 |
| `detectSampleRate(bytes)` | 流自身的采样率（Hz） |
| `packets(bytes)` | 拆分为数据包数组（`Uint8Array[]`） |
| `decode(bytes)` | 一次性解码 → `{ samples, sampleRate, duration, toFloat32() }` |
| `decodeToWav(bytes)` | 解码为 16-bit 单声道 WAV 字节 |
| `stream()` | 创建增量解码器 `SilkStream` |
| `dispose()` | 释放内部暂存内存 |

### `SilkStream`

| 成员 | 说明 |
| --- | --- |
| `decode(packet)` | 解码单个数据包 → `Int16Array` |
| `decodeNext(bytes)` | 解码下一个包（内部游标自动前进），结束返回空数组 |
| `sampleRate` / `packetsSeen` | 采样率 / 已解码包数 |
| `dispose()` | 释放解码器 |

### 其他导出

- `playSilk(audio, { context, gain })` — 用 Web Audio 播放并返回 `{ source, stop }`
- `createSilkStreamPlayer(silk, { context, gain, onFrame })` — 边解边播
- `decodeToAudioBuffer(silk, bytes, context?)` — 解码成 `AudioBuffer`
- `float32From(samples)` — `Int16Array` → `Float32Array`（`[-1, 1)`）
- `SilkError` — 解码错误类型

## 为什么解码是精确的

解码结果与 **SILK 参考实现（Skype SILK SDK v1.0.9，定点 FIX 版）逐样本一致**，
不是"听起来差不多"，而是二进制完全相同：

```
$ cargo build -p silk-cli
$ ./target/debug/silk-cli diff test/fixtures/test.slk test/fixtures/test_16000.pcm 16000
IDENTICAL: 80000 samples match the reference exactly
```

对全部受支持的采样率都做了逐样本比对，**全部 `IDENTICAL`**：

| 输出采样率 | 与参考实现比对 |
| --- | --- |
| 8000 Hz | 逐样本一致（40000/40000） |
| 12000 Hz | 逐样本一致（60000/60000） |
| 16000 Hz | 逐样本一致（80000/80000） |
| 24000 Hz | 逐样本一致（120000/120000） |
| 32000 Hz | 逐样本一致（160000/160000） |
| 48000 Hz | 逐样本一致（240000/240000） |

### 关于 44.1 kHz

SILK 文件内部只有 **8 / 12 / 16 / 24 kHz** 四种速率，SILK SDK 本身也只实现
这些速率（以及 32 / 48 kHz）。**44.1 kHz 与 22.05 kHz 会明确报错**，而不是返回
有细微偏差的音频：

```
cannot resample 16000 Hz (the stream's rate) to 44100 Hz:
supported output rates are 8000, 12000, 16000, 24000, 32000 and 48000 Hz.
Decode at the native rate and resample afterwards instead
```

这是刻意设计：默认按文件自身速率解码，输出永远是精确的；浏览器播放时会由
Web Audio 自动重采样到声卡速率（例如 44.1 kHz），音质和兼容性都比内置一个
近似重采样器更好。如果确实需要自己转成 44.1 kHz，请在拿到原生 PCM 后使用
成熟的重采样库。

## 项目结构

```
crates/
  silk-decoder/     纯 Rust 的 SILK v3 解码器核心（无 unsafe、无 C 依赖）
  silk-core-wasm/   免工具链的 wasm 导出层（C 风格 ABI + 线性内存）
  silk-wasm/        可选的 wasm-bindgen 绑定层（需要 wasm-bindgen-cli）
  silk-cli/         开发用命令行工具，用于与 C 参考实现做逐样本比对
web/
  silk.js           ESM 封装（浏览器 + Node 通用）
  silk_core.wasm    构建产物
  index.html        演示页面
test/
  silk.test.mjs     Node 测试套件
  fixtures/         测试文件与 C 参考 PCM
```

## 构建

```bash
./build.sh              # release
./build.sh --debug      # 更快的构建，体积更大
```

只需要 Rust 的 `wasm32-unknown-unknown` 目标：

```bash
rustup target add wasm32-unknown-unknown
```

Cargo registry 被固定在工作目录下的 `.cargo-home/`，构建过程不依赖也不写入全局
`~/.cargo`。

## 测试

```bash
npm test           # Node 端 14 项测试（与 C 参考 PCM 逐样本比对）
npm run test:rust  # Rust 单元测试（24 项）
```

`test/fixtures/test_16000.pcm` 由 SILK 参考解码器生成，是测试的"标准答案"。

### 与 C 参考实现比对（可选）

`silk-cli` 用于开发期验证：

```bash
cargo build -p silk-cli
./target/debug/silk-cli decode input.slk out.wav 24000   # 解码为 WAV
./target/debug/silk-cli pcm    input.slk out.pcm 16000   # 解码为裸 PCM
./target/debug/silk-cli diff   input.slk ref.pcm 16000   # 与参考 PCM 比对
```

## 浏览器兼容性

- `WebAssembly.instantiate`（所有现代浏览器）
- 播放使用 Web Audio API；16 kHz 会在浏览器内被重采样到声卡速率
- 自动播放策略：首次播放需由用户手势触发（演示页通过按钮满足）
- 若从 `https://` 页面加载 `http://` 的 `.slk` 链接会被浏览器拦截（混合内容），
  此时请下载到本地后用文件选择器打开

## 已知限制

- 只做**解码**，不含编码器
- 输出为单声道 16-bit PCM（SILK 本身即单声道）
- 内置重采样只支持 8000 / 12000 / 16000 / 24000 / 32000 / 48000 Hz；
  44.1 kHz 系列会明确报错（见上文）

## 许可

MIT。SILK 编解码算法源自 Skype 的 SILK SDK；本仓库为独立的 Rust 实现。
