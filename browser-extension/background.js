// course2md 浏览器扩展：工具栏按钮 → 取当前页视频 → 经 Native Messaging 交给本机 course2md。
// 服务工作线程可能随时被回收，任务状态一律存 chrome.storage.local，监听器都在顶层注册。

const HOST = "com.course2md.host";
// course2md 自己能处理这些站点的链接（登录 / 字幕），直接交页面网址。
const NATIVE_SITES = ["bilibili.com", "b23.tv", "youtube.com", "youtu.be"];
// 通知必须带图标；用内联的 1x1 PNG，免得扩展里放图片文件。
const ICON =
  "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
const ACTIVE = ["starting", "downloading", "converting"];
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

// ---------- 任务状态（持久化，每个任务一个 key） ----------

const KEY = (id) => `job:${id}`;

// 所有「读-改-写」串行执行，避免多个监听器同时改状态互相覆盖。
let queue = Promise.resolve();
function locked(fn) {
  const run = queue.then(fn, fn);
  queue = run.catch(() => {});
  return run;
}

async function getJob(id) {
  const data = await chrome.storage.local.get(KEY(id));
  return data[KEY(id)] || null;
}

async function allJobs() {
  const data = await chrome.storage.local.get(null);
  return Object.keys(data)
    .filter((k) => k.startsWith("job:"))
    .map((k) => data[k]);
}

async function findJob(pred) {
  return (await allJobs()).find(pred) || null;
}

function putJob(job) {
  return chrome.storage.local.set({ [KEY(job.id)]: job });
}

// 在锁内读最新状态、改、写回；fn 返回 false 表示不改。
function updateJob(id, fn) {
  return locked(async () => {
    const job = await getJob(id);
    if (!job || fn(job) === false) return null;
    await putJob(job);
    return job;
  });
}

// 浏览器重启后，上次没结束的任务已经没有连接可接了，标记为中断，允许重新点击。
chrome.runtime.onStartup.addListener(() =>
  locked(async () => {
    for (const job of await allJobs()) {
      if (ACTIVE.includes(job.status)) {
        job.status = "interrupted";
        await putJob(job);
      }
    }
    setBadge("");
  })
);

// ---------- 小工具 ----------

function isNativeSite(url) {
  try {
    const host = new URL(url).hostname;
    return NATIVE_SITES.some((h) => host === h || host.endsWith("." + h));
  } catch {
    return false;
  }
}

// downloads API 只收相对路径；去掉分隔符、控制字符和文件系统非法字符，并限制长度。
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

async function fail(id, message) {
  const job = await updateJob(id, (j) => {
    j.status = "failed";
    j.error = message;
  });
  setBadge("");
  notify(id, "course2md 转换失败", `${(job && job.title) || ""}\n${message}`);
}

// ---------- 入口：点工具栏按钮 ----------

chrome.action.onClicked.addListener(async (tab) => {
  const running = await findJob((j) => j.pageUrl === tab.url && ACTIVE.includes(j.status));
  if (running) {
    notify(`dup-${Date.now()}`, "course2md", "这个页面已经在处理中了");
    return;
  }
  const job = {
    id: `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`,
    tabId: tab.id,
    pageUrl: tab.url,
    title: safeName(tab.title),
    status: "starting",
  };
  await locked(() => putJob(job));

  if (isNativeSite(tab.url)) {
    return startConvert(job.id, { source: tab.url, isFile: false });
  }

  let picked;
  try {
    const [res] = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func: pickVideo });
    picked = res && res.result;
  } catch (e) {
    return fail(job.id, `无法读取页面：${e.message}`);
  }
  if (!picked) return fail(job.id, "这个页面上没找到视频");

  if (/^https?:/i.test(picked.src)) {
    return downloadDirect(job.id, picked.src);
  }
  // blob: / MediaSource 流拿不到文件地址，退回交页面网址，由 course2md（yt-dlp）试着解析。
  return startConvert(job.id, { source: tab.url, isFile: false });
});

// 注入页面执行：优先正在播放的视频，否则取画面最大的。
function pickVideo() {
  const videos = [...document.querySelectorAll("video")].filter((v) => v.currentSrc || v.src);
  if (!videos.length) return null;
  const area = (v) => v.clientWidth * v.clientHeight;
  const playing = videos.filter((v) => !v.paused && v.currentTime > 0);
  const pool = playing.length ? playing : videos;
  const best = pool.sort((a, b) => area(b) - area(a))[0];
  return { src: best.currentSrc || best.src };
}

// ---------- 下载：先用 downloads API，失败再在页面里 fetch ----------

async function downloadDirect(id, src) {
  const job = await updateJob(id, (j) => {
    j.status = "downloading";
    j.videoSrc = src;
  });
  setBadge("下载");
  let downloadId;
  try {
    downloadId = await chrome.downloads.download({
      url: src,
      filename: `course2md/${job.title}-${job.id}.mp4`,
      conflictAction: "uniquify",
      saveAs: false,
    });
  } catch (e) {
    return downloadInPage(id);
  }
  await updateJob(id, (j) => {
    j.downloadId = downloadId;
  });
  // 小文件可能在 downloadId 写入前就下完了，那次 onChanged 会找不到任务；这里补查一次。
  const [item] = await chrome.downloads.search({ id: downloadId });
  if (item) handleDownloadState(downloadId, item.state);
}

// 在页面上下文里 fetch（带页面自己的 Referer），再用 <a download> 存下来；
// 由 onDeterminingFilename 按「页面来源的 blob: 地址 + 登记的文件名」认领并改名。
async function downloadInPage(id) {
  let alreadyTried = false;
  const job = await updateJob(id, (j) => {
    if (j.fallbackTried) {
      alreadyTried = true;
      return false;
    }
    j.fallbackTried = true;
    j.downloadId = null;
    j.fallbackName = `course2md-${j.id}.mp4`;
  });
  if (alreadyTried) return fail(id, "视频下载失败（直接下载和页面内下载都不行）");
  if (!job) return;
  try {
    const [res] = await chrome.scripting.executeScript({
      target: { tabId: job.tabId },
      func: fetchInPage,
      args: [job.videoSrc, job.fallbackName],
    });
    const r = res && res.result;
    if (!r || !r.ok) return fail(id, `页面内下载失败：${(r && r.error) || "未知原因"}`);
  } catch (e) {
    return fail(id, `页面内下载失败：${e.message}`);
  }
}

async function fetchInPage(src, name) {
  try {
    // 不带 credentials: "include"：视频 CDN 多为跨域，带凭据会要求 CDN 额外放行，反而失败。
    const resp = await fetch(src);
    if (!resp.ok) return { ok: false, error: `HTTP ${resp.status}` };
    const blob = await resp.blob();
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = name;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 120000);
    return { ok: true, size: blob.size };
  } catch (e) {
    return { ok: false, error: String(e && e.message) };
  }
}

function originOf(url) {
  try {
    return new URL(url).origin;
  } catch {
    return null;
  }
}

// 注册后会拦到所有下载，所以每个下载都必须调用 suggest()（包括出错时），不相关的原样放行。
chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  const base = (item.filename || "").split(/[\\/]/).pop();
  if (!/^course2md-[a-z0-9]+\.mp4$/.test(base) || !item.url.startsWith("blob:")) {
    suggest();
    return;
  }
  locked(async () => {
    const job = await findJob((j) => j.fallbackName === base);
    // 只认领：仍在下载中、还没关联下载、且 blob 来自该任务页面的同源。
    if (!job || job.status !== "downloading" || job.downloadId != null ||
        !item.url.startsWith(`blob:${originOf(job.pageUrl)}/`)) {
      return null;
    }
    job.downloadId = item.id;
    job.fallbackName = null; // 一次性消费
    await putJob(job);
    return job;
  })
    .then((job) => {
      if (job) suggest({ filename: `course2md/${job.title}-${job.id}.mp4`, conflictAction: "uniquify" });
      else suggest();
    })
    .catch(() => suggest());
  return true;
});

chrome.downloads.onChanged.addListener((delta) => {
  if (delta.state) handleDownloadState(delta.id, delta.state.current);
});

// onChanged 和 downloadDirect 的补查都会走到这里；靠状态迁移保证只处理一次。
async function handleDownloadState(downloadId, state) {
  if (state !== "complete" && state !== "interrupted") return;
  const job = await findJob((j) => j.downloadId === downloadId && j.status === "downloading");
  if (!job) return;
  if (state === "interrupted") {
    // 同样先认领，避免 onChanged 和补查各触发一次回退、第二次被误判为「两种都失败」。
    const claimedFail = await updateJob(job.id, (j) => {
      if (j.status !== "downloading" || j.downloadId !== downloadId) return false;
      j.downloadId = null;
    });
    if (claimedFail) return downloadInPage(job.id);
    return;
  }
  const claimed = await updateJob(job.id, (j) => {
    if (j.status !== "downloading" || j.downloadId !== downloadId) return false;
    j.status = "converting";
  });
  if (!claimed) return;
  // 以 Chrome 实际保存的绝对路径为准（可能因重名被改过）。
  const [item] = await chrome.downloads.search({ id: downloadId });
  if (!item || !item.filename) return fail(job.id, "找不到下载好的视频文件");
  return startConvert(job.id, { source: item.filename, isFile: true });
}

// ---------- 转换：连本机桥接程序 ----------

async function startConvert(id, input) {
  const job = await updateJob(id, (j) => {
    j.status = "converting";
  });
  if (!job) return;
  setBadge("开始");
  notify(id, "course2md 开始转换", job.title);

  let port;
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch (e) {
    return fail(id, `连不上本机桥接程序：${e.message}（运行过 host/install.sh 吗？）`);
  }
  let finished = false;

  port.onMessage.addListener(async (msg) => {
    if (msg.type === "stage") {
      // 阶段名可能带子阶段，如 model/prepare、scenes/scan，按前缀取。
      setBadge(STAGE_BADGE[String(msg.stage).split("/")[0]] || "处理");
    } else if (msg.type === "done") {
      finished = true;
      const done = await updateJob(id, (j) => {
        j.status = msg.partial ? "partial" : "done";
        j.html = msg.html || null;
        j.noteTitle = msg.title || j.title;
      });
      setBadge("");
      const extra = msg.problems && msg.problems.length ? `\n部分步骤失败：${msg.problems.join("、")}` : "";
      const tail = msg.html ? "\n点击打开笔记" : "\n没有生成网页版，请在 course2md 笔记库里查看";
      notify(id, msg.partial ? "course2md 已完成（有步骤失败）" : "course2md 已完成",
        `${(done && done.noteTitle) || job.title}${extra}${tail}`);
    } else if (msg.type === "error") {
      finished = true;
      await fail(id, msg.message || "未知错误");
    }
  });

  port.onDisconnect.addListener(async () => {
    if (finished) return;
    const why = chrome.runtime.lastError ? chrome.runtime.lastError.message : "连接中断";
    // 桥接程序让 course2md 在独立进程组里跑，连接断了转换也可能还在继续。
    await fail(id, `${why}。若转换已开始，完成后仍可在 course2md 笔记库「未分类」里找到`);
  });

  port.postMessage({ type: "convert", jobId: id, source: input.source, isFile: input.isFile, title: job.title });
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
