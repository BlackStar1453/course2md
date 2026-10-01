// course2md 浏览器扩展：工具栏按钮 → 取当前页视频 → 经 Native Messaging 交给本机 course2md。
//
// 视频不走 Chrome 下载：在页面里 fetch（带页面自己的 Referer，CDN 才肯给），
// 分块经 runtime 消息转给桥接程序，由它直接写进 ~/Movies/course2md-videos/。
// 这样避开了：CDN 拒绝扩展直接下载、Chrome「多次自动下载」拦截、macOS「下载」文件夹权限弹窗。
//
// 打开的 Native 端口会让服务工作线程一直存活；所以线程一旦重启，说明之前的连接都已断开。

const HOST = "com.course2md.host";
// course2md 自己能处理这些站点的链接（登录 / 字幕），直接交页面网址。
const NATIVE_SITES = ["bilibili.com", "b23.tv", "youtube.com", "youtu.be"];
// 通知必须带图标；用内联的 1x1 PNG，免得扩展里放图片文件。
const ICON =
  "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
const ACTIVE = ["starting", "uploading", "converting"];
const STAGE_BADGE = {
  fetch: "获取",
  download: "下载",
  subtitle: "字幕",
  model: "模型",
  audio: "音频",
  transcribe: "识别",
  llm: "校对",
  scenes: "截图",
  summary: "摘要",
  render: "导出",
};

// 本线程内活着的任务：jobId → Native 端口。只有这里有的任务才算「正在处理」。
const ports = new Map();

// ---------- 任务状态（持久化，每个任务一个 key；只用于点通知时找笔记） ----------

const KEY = (id) => `job:${id}`;

async function getJob(id) {
  const data = await chrome.storage.local.get(KEY(id));
  return data[KEY(id)] || null;
}

function putJob(job) {
  return chrome.storage.local.set({ [KEY(job.id)]: job });
}

async function patchJob(id, fields) {
  const job = await getJob(id);
  if (!job) return null;
  Object.assign(job, fields);
  await putJob(job);
  return job;
}

// 线程刚启动时没有任何活着的端口；存储里还标着进行中的任务都已断开，标记为中断。
(async () => {
  const data = await chrome.storage.local.get(null);
  for (const [k, job] of Object.entries(data)) {
    if (k.startsWith("job:") && ACTIVE.includes(job.status) && !ports.has(job.id)) {
      job.status = "interrupted";
      await putJob(job);
    }
  }
})();

// ---------- 小工具 ----------

function isNativeSite(url) {
  try {
    const host = new URL(url).hostname;
    return NATIVE_SITES.some((h) => host === h || host.endsWith("." + h));
  } catch {
    return false;
  }
}

function isYouTube(url) {
  try {
    const host = new URL(url).hostname;
    return ["youtube.com", "youtu.be"].some((h) => host === h || host.endsWith("." + h));
  } catch {
    return false;
  }
}

// youtube.com/watch?v=<id> 返回视频 ID，其他页面返回 null。
function youTubeWatchId(url) {
  try {
    const u = new URL(url);
    if (!isYouTube(url) || u.pathname !== "/watch") return null;
    const id = u.searchParams.get("v");
    return id && /^[\w-]{11}$/.test(id) ? id : null;
  } catch {
    return null;
  }
}

function safeName(title) {
  const name = (title || "")
    .replace(/[\\/:*?"<>|\u0000-\u001f\u007f]/g, " ")
    .replace(/\s+/g, " ")
    .replace(/^[\s.]+|[\s.]+$/g, "")
    .slice(0, 60)
    .trim();
  return name || "video";
}

function notify(id, title, message) {
  chrome.notifications.create(id, { type: "basic", iconUrl: ICON, title, message });
}

function setBadge(text) {
  chrome.action.setBadgeText({ text: text || "" });
}

async function finish(id, fields) {
  const port = ports.get(id);
  ports.delete(id);
  if (port) {
    try {
      port.disconnect();
    } catch {
      // 已断开
    }
  }
  if (!ports.size) setBadge("");
  return patchJob(id, fields);
}

async function fail(id, message) {
  const job = await finish(id, { status: "failed", error: message });
  notify(id, "course2md 转换失败", `${(job && job.title) || ""}\n${message}`);
}

// ---------- 入口：点工具栏按钮 ----------

chrome.action.onClicked.addListener(async (tab) => {
  for (const id of ports.keys()) {
    const job = await getJob(id);
    if (job && job.pageUrl === tab.url) {
      notify(`dup-${Date.now()}`, "course2md", "这个页面已经在处理中了");
      return;
    }
  }
  const job = {
    id: `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`,
    tabId: tab.id,
    pageUrl: tab.url,
    title: safeName(tab.title),
    status: "starting",
  };
  await putJob(job);

  const ytId = youTubeWatchId(tab.url);
  if (ytId) {
    return convertYouTube(job, ytId);
  }
  if (isNativeSite(tab.url)) {
    // B站等：course2md 自己处理。YouTube 的 Shorts / 其他页面没法在页面里取字幕，
    // 改用语音识别，避开会 429 的在线字幕下载。
    return convertUrl(job, tab.url, isYouTube(tab.url) ? { asrFallback: true } : {});
  }

  let picked;
  try {
    const [res] = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func: pickVideo });
    picked = res && res.result;
  } catch (e) {
    return fail(job.id, `无法读取页面：${e.message}`);
  }
  if (!picked) return fail(job.id, "这个页面上没找到视频");

  if (picked.srcs.length) {
    return uploadFromPage(job, picked.srcs);
  }
  // blob: / MediaSource 流拿不到文件地址，退回交页面网址，由 course2md（yt-dlp）试着解析。
  return convertUrl(job, tab.url);
});

// 注入页面执行：优先正在播放的视频，否则取画面最大的。
function pickVideo() {
  const videos = [...document.querySelectorAll("video")].filter((v) => v.currentSrc || v.src);
  if (!videos.length) return null;
  const area = (v) => v.clientWidth * v.clientHeight;
  const playing = videos.filter((v) => !v.paused && v.currentTime > 0);
  const pool = playing.length ? playing : videos;
  const best = pool.sort((a, b) => area(b) - area(a))[0];
  const src = best.currentSrc || best.src;
  if (/^https?:/i.test(src)) return { srcs: [src] };
  // blob: 视频流（MediaSource）没有文件地址；但抖音这类网站流里分段请求的仍是完整 mp4，
  // 而且画面和声音常是两个独立的 mp4。从页面的网络请求记录里取最近请求的 2 个不同 mp4
  // （按路径去重，同一文件的分段请求只算一个），不带 Range 再取一次就是整个文件，
  // 由桥接程序判断哪个是画面、哪个是声音并合并。
  // 只认完整 mp4（.mp4 或 mime_type=video_mp4）；HLS / DASH 分片拿不到整片，交回页面网址处理。
  const seen = new Set();
  const srcs = [];
  const urls = performance
    .getEntriesByType("resource")
    .map((e) => e.name)
    .filter((u) => /^https?:/i.test(u) && /mime_type=video_mp4|\.mp4(\?|$)/i.test(u));
  for (const u of urls.reverse()) {
    const key = new URL(u).origin + new URL(u).pathname;
    if (seen.has(key)) continue;
    seen.add(key);
    srcs.push(u);
    if (srcs.length === 2) break;
  }
  return { srcs };
}

// ---------- 连接桥接程序 ----------

// 打开端口并挂好转换阶段的消息处理；失败返回 null（已通知）。
async function openHost(job) {
  let port;
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch (e) {
    await fail(job.id, `连不上本机桥接程序：${e.message}（运行过 host/install.sh 吗？）`);
    return null;
  }
  ports.set(job.id, port);

  port.onMessage.addListener(async (msg) => {
    if (msg.type === "stage") {
      if (msg.stage === "fetch") await patchJob(job.id, { status: "converting" });
      setBadge(STAGE_BADGE[String(msg.stage).split("/")[0]] || "处理");
    } else if (msg.type === "done") {
      const done = await finish(job.id, {
        status: msg.partial ? "partial" : "done",
        html: msg.html || null,
        noteTitle: msg.title || job.title,
      });
      const extra =
        (msg.problems && msg.problems.length ? `\n部分步骤失败：${msg.problems.join("、")}` : "") +
        (msg.notes && msg.notes.length ? `\n${msg.notes.join("；")}` : "");
      const tail = msg.html ? "\n点击打开笔记" : "\n没有生成网页版，请在 course2md 笔记库里查看";
      notify(job.id, msg.partial ? "course2md 已完成（有步骤失败）" : "course2md 已完成",
        `${(done && done.noteTitle) || job.title}${extra}${tail}`);
    } else if (msg.type === "error") {
      await fail(job.id, msg.message || "未知错误");
    }
  });

  port.onDisconnect.addListener(async () => {
    if (!ports.has(job.id)) return; // 已正常结束
    const why = chrome.runtime.lastError ? chrome.runtime.lastError.message : "连接中断";
    // 桥接程序让 course2md 在独立进程组里跑，连接断了转换也可能还在继续。
    await fail(job.id, `${why}。若转换已开始，完成后仍可在 course2md 笔记库「未分类」里找到`);
  });

  setBadge("开始");
  notify(job.id, "course2md 开始处理", job.title);
  return port;
}

// extra：{ subtitle: <VTT 文本> } 用浏览器里取到的字幕；{ asrFallback: true, note } 改用语音识别。
async function convertUrl(job, url, extra = {}) {
  const port = await openHost(job);
  if (!port) return;
  await patchJob(job.id, { status: "converting" });
  port.postMessage({ type: "convert", jobId: job.id, source: url, title: job.title, ...extra });
}

// ---------- YouTube：在页面里取字幕（带播放器自己的 PO Token），再交给 course2md ----------

async function convertYouTube(job, videoId) {
  setBadge("字幕");
  let r;
  try {
    const [res] = await chrome.scripting.executeScript({
      target: { tabId: job.tabId },
      world: "MAIN", // 要调用页面播放器的方法（#movie_player 上的 API 只在页面环境可见）
      func: grabYouTubeCaptions,
      args: [videoId],
    });
    r = res && res.result;
  } catch (e) {
    r = { ok: false, reason: e.message };
  }
  const url = `https://www.youtube.com/watch?v=${videoId}`;
  if (r && r.ok) {
    return convertUrl(job, url, { subtitle: r.vtt });
  }
  return convertUrl(job, url, {
    asrFallback: true,
    note: `没取到网页字幕（${(r && r.reason) || "未知原因"}），改用语音识别`,
  });
}

// 注入页面（MAIN world）执行。实测（2026-10）：字幕地址不带 pot 时返回空内容；
// 播放器开字幕时自己发出的 timedtext 请求带 pot（与视频 ID 绑定），换 lang / fmt 后可取任意轨道的 VTT。
// 选轨：人工中文字幕 > 原语言（人工优先于自动生成）；不用机器翻译。
// 若播放器还没请求过字幕：临时静音播放并打开字幕，拿到请求后只恢复自己改过的状态。
async function grabYouTubeCaptions(expectedId) {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const player = document.querySelector("#movie_player");
  if (!player || !player.getPlayerResponse) return { ok: false, reason: "找不到播放器" };
  const currentId = () => (player.getVideoData && player.getVideoData().video_id) || "";
  if (currentId() !== expectedId) return { ok: false, reason: "页面上的视频已经变了" };

  const response = player.getPlayerResponse();
  const tracks = (response && response.captions &&
    response.captions.playerCaptionsTracklistRenderer &&
    response.captions.playerCaptionsTracklistRenderer.captionTracks) || [];
  if (!tracks.length) return { ok: false, reason: "这个视频没有字幕" };
  const manual = tracks.filter((t) => t.kind !== "asr");
  const auto = tracks.find((t) => t.kind === "asr");
  const pick =
    manual.find((t) => /^zh/i.test(t.languageCode)) ||
    (auto && (manual.find((t) => t.languageCode === auto.languageCode) || auto)) ||
    manual[0];

  const findCaptured = () =>
    performance
      .getEntriesByType("resource")
      .map((e) => e.name)
      .filter((u) => u.includes("/api/timedtext"))
      .map((u) => new URL(u))
      .filter((u) => u.searchParams.get("v") === expectedId && u.searchParams.get("pot"))
      .pop();

  let captured = findCaptured();
  if (!captured) {
    const restore = [];
    try {
      // 片头广告期间不会请求正片字幕，先等广告放完（最多 60 秒）
      for (let i = 0; i < 120 && player.classList.contains("ad-showing"); i++) await sleep(500);
      if (currentId() !== expectedId) return { ok: false, reason: "取字幕时页面切换了视频" };
      // 只有「暂停 / 未开始 / 已就绪」时才由我们来播放；缓冲中（3）本来就在播，不去碰
      const state = player.getPlayerState();
      if (state === 2 || state === -1 || state === 5) {
        const at = player.getCurrentTime();
        if (!player.isMuted()) {
          player.mute();
          restore.push(() => player.unMute());
        }
        player.playVideo();
        restore.push(() => {
          player.pauseVideo();
          player.seekTo(at, true);
        });
        for (let i = 0; i < 20 && player.getPlayerState() !== 1; i++) await sleep(250);
      }
      const ccTrack = player.getOption && player.getOption("captions", "track");
      if (!(ccTrack && ccTrack.languageCode)) {
        if (player.toggleSubtitlesOn) player.toggleSubtitlesOn();
        else player.toggleSubtitles();
        restore.push(() => player.toggleSubtitles());
      }
      for (let i = 0; i < 20 && !captured; i++) {
        await sleep(500);
        captured = findCaptured();
      }
    } finally {
      // 页面已换成别的视频时不再恢复，免得暂停 / 跳转了新视频
      for (const undo of restore.reverse()) {
        if (currentId() !== expectedId) break;
        try {
          undo();
        } catch {
          // 尽力恢复
        }
      }
    }
  }
  if (currentId() !== expectedId) return { ok: false, reason: "取字幕时页面切换了视频" };
  if (!captured) return { ok: false, reason: "播放器没有发出字幕请求" };

  const fetchVtt = async (lang, kind) => {
    const u = new URL(captured.href);
    u.searchParams.delete("tlang");
    u.searchParams.delete("name");
    if (lang) u.searchParams.set("lang", lang);
    if (kind) u.searchParams.set("kind", kind);
    else if (lang) u.searchParams.delete("kind");
    u.searchParams.set("fmt", "vtt");
    try {
      const text = await (await fetch(u.href)).text();
      return text.startsWith("WEBVTT") && text.includes("-->") ? text : null;
    } catch {
      return null;
    }
  };
  const vtt =
    (await fetchVtt(pick.languageCode, pick.kind === "asr" ? "asr" : null)) ||
    (await fetchVtt(null, null)); // 退回播放器实际请求的那条轨道
  if (!vtt) return { ok: false, reason: "字幕内容为空或请求失败" };
  return { ok: true, vtt, lang: pick.languageCode, kind: pick.kind || "manual" };
}

// ---------- 视频：页面里 fetch，分块传给桥接程序 ----------

// 逐个传输候选文件（通常 1 个；视频流网站可能是画面 + 声音 2 个）。
// 某个失败就跳过它，至少传成功一个才继续；end 里告诉桥接程序哪些文件是完整的。
async function uploadFromPage(job, srcs) {
  const port = await openHost(job);
  if (!port) return;
  await patchJob(job.id, { status: "uploading" });
  setBadge("传输");
  port.postMessage({ type: "begin", title: job.title });

  const ok = [];
  let lastError = "";
  for (let index = 0; index < srcs.length; index++) {
    let r;
    try {
      const [res] = await chrome.scripting.executeScript({
        target: { tabId: job.tabId },
        func: sendVideoChunks,
        args: [srcs[index], job.id, index],
      });
      r = res && res.result;
    } catch (e) {
      r = { ok: false, error: e.message };
    }
    if (!ports.has(job.id)) return; // 传输途中桥接程序已报错 / 断开
    if (r && r.ok) ok.push(index);
    else lastError = (r && r.error) || "未知原因";
  }
  if (!ok.length) {
    port.postMessage({ type: "abort" });
    return fail(job.id, `视频传输失败：${lastError}`);
  }
  await patchJob(job.id, { status: "converting" });
  port.postMessage({ type: "end", ok });
}

// 页面里转来的视频分块，原样转给该任务的桥接程序；回复 false 让页面停止。
chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (!msg || msg.type !== "course2md-chunk") return;
  const port = ports.get(msg.jobId);
  if (!port) {
    sendResponse(false);
    return;
  }
  port.postMessage({ type: "chunk", index: msg.index, data: msg.data });
  sendResponse(true);
});

// 注入页面执行（隔离环境，可用 chrome.runtime）。
// 在页面上下文里 fetch，才带得上页面的 Referer；不带 credentials，免得跨域 CDN 要求额外放行。
// 每块约 4MB，转成 base64 发给扩展；2 分钟没有新数据就放弃，免得任务卡死。
async function sendVideoChunks(src, jobId, index) {
  const CHUNK = 4 * 1024 * 1024;
  const STALL_MS = 120000;
  const ctrl = new AbortController();
  let timer = setTimeout(() => ctrl.abort(), STALL_MS);
  const kick = () => {
    clearTimeout(timer);
    timer = setTimeout(() => ctrl.abort(), STALL_MS);
  };
  const toBase64 = (bytes) =>
    new Promise((resolve, reject) => {
      const fr = new FileReader();
      fr.onload = () => resolve(String(fr.result).split(",", 2)[1] || "");
      fr.onerror = () => reject(fr.error);
      fr.readAsDataURL(new Blob([bytes]));
    });
  const flush = async (parts) => {
    const data = await toBase64(new Blob(parts));
    const ok = await chrome.runtime.sendMessage({ type: "course2md-chunk", jobId, index, data });
    if (!ok) throw new Error("扩展端已停止接收");
  };
  try {
    const resp = await fetch(src, { signal: ctrl.signal });
    if (!resp.ok) return { ok: false, error: `HTTP ${resp.status}` };
    const reader = resp.body.getReader();
    let parts = [];
    let pending = 0;
    let size = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      kick();
      parts.push(value);
      pending += value.byteLength;
      size += value.byteLength;
      if (pending >= CHUNK) {
        await flush(parts);
        parts = [];
        pending = 0;
      }
    }
    if (pending) await flush(parts);
    if (!size) return { ok: false, error: "下载到的视频是空的" };
    return { ok: true, size };
  } catch (e) {
    return { ok: false, error: ctrl.signal.aborted ? "2 分钟没有收到新数据，已放弃" : String(e && e.message) };
  } finally {
    clearTimeout(timer);
  }
}

// ---------- 点通知：让桥接程序打开笔记网页版 ----------

chrome.notifications.onClicked.addListener(async (id) => {
  chrome.notifications.clear(id);
  const job = await getJob(id);
  if (!job || !job.html) return;
  try {
    chrome.runtime.sendNativeMessage(HOST, { type: "open", path: job.html }, () => void chrome.runtime.lastError);
  } catch {
    // 桥接程序没装好时点通知不做任何事
  }
});
