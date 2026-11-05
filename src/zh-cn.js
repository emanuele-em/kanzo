// zh-CN dictionary for the board UI, keyed by the English string it translates.
// A missing key falls back to the English original, so a string renamed upstream
// degrades gracefully instead of breaking. Served at /i18n/zh-cn.js and loaded by
// app.js only when the board's language isn't English.
//
// Plural nouns carry their measure word, so `plural()` can render 5 个任务,
// 2 条评论, 3 台电脑. Status names translate the column titles only; the stored
// status values (todo, doing, …) and the MCP API stay English.
const ZH_CN = {
  // Status column titles, stored values stay English.
  "backlog": "暂存列",
  "todo": "待办",
  "doing": "进行中",
  "review": "评审",
  "done": "完成",

  // Sidebar and header.
  "Search (/ or ⌘K)": "搜索（/ 或 ⌘K）",
  "Search tasks": "搜索任务",
  "New project": "新建项目",
  "New task (n)": "新建任务（n）",
  "New task": "新建任务",
  "Settings": "设置",
  "All projects": "所有项目",
  "Projects": "项目",
  "Project": "项目",
  "Filter board": "筛选看板",
  "Filter the board": "筛选看板",
  "Show only the tasks that match, like tag:bug or -status:done (f)": "只显示匹配的任务，例如 tag:bug 或 -status:done（f）",
  "Show only human tasks (h)": "只显示人工任务（h）",
  "Show only human tasks": "只显示人工任务",
  "Board not reachable — is kanzo running?": "看板连不上——kanzo 还在运行吗？",

  // Columns and cards.
  "Welcome to Kanzo": "欢迎使用 Kanzo",
  "Create your first project in the sidebar. Link it to a GitHub repo and agents working in that repo will find it on their own.":
    "在侧边栏创建你的第一个项目。把它关联到 GitHub 仓库，在那个仓库里干活的智能体会自己找到它。",
  "Show older": "显示更早的",
  "Show {0} older": "显示更早的 {0} 个",
  "Human task: agents skip it": "人工任务：智能体会跳过",
  "Copy {0}": "复制 {0}",
  "Copied {0}": "已复制 {0}",
  "Show {0}": "展开{0}",
  "Collapse {0}": "折叠{0}",

  // Project menu.
  "More": "更多",
  "More for {0}": "{0} 的更多操作",
  "Color": "颜色",
  "Repos": "仓库",
  "Link a repo: owner/repo": "关联仓库：owner/repo",
  "Link a GitHub repo": "关联 GitHub 仓库",
  "Unlink {0}": "取消关联 {0}",
  "Rename": "重命名",
  "Rename {0}": "重命名 {0}",
  "Delete project": "删除项目",
  "gray": "灰色",
  "red": "红色",
  "orange": "橙色",
  "yellow": "黄色",
  "green": "绿色",
  "teal": "青色",
  "blue": "蓝色",
  "purple": "紫色",
  "pink": "粉色",

  // Filters.
  "Remove \"{0}\"": "移除「{0}」",
  "Remove the filter {0}": "移除筛选条件 {0}",

  // Task panel.
  "Delete task": "删除任务",
  "Close (Esc)": "关闭（Esc）",
  "Close": "关闭",
  "Task title": "任务标题",
  "Status": "状态",
  "Tags": "标签",
  "Add tags, like bug, ui": "添加标签，如 bug、ui",
  "URL": "URL",
  "Add a link, like a pull request": "添加链接，如拉取请求",
  "Human": "人工",
  "Human task": "人工任务",
  "Agents skip it": "智能体会跳过",
  "Created": "创建时间",
  "Updated": "更新时间",
  "Description": "描述",
  "Add a description (Markdown)": "添加描述（Markdown）",
  "Edit description": "编辑描述",
  "Comments": "评论",
  "Add a comment": "添加评论",
  "Send (⌘↵)": "发送（⌘↵）",
  "Send comment": "发送评论",
  "Delete comment": "删除评论",
  "Remove image": "移除图片",
  "Attached image": "图片附件",
  "That task was deleted": "该任务已被删除",

  // Confirms and toasts.
  "Delete project {0}{1}? This cannot be undone.": "删除项目 {0}{1}？此操作无法撤销。",
  " and its {0}, with their comments": "，及其 {0} 与其中的评论",
  "Delete {0}{1}? This cannot be undone.": "删除 {0}{1}？此操作无法撤销。",
  " and its {0}": "，及其 {0}",
  "Delete this comment? This cannot be undone.": "删除这条评论？此操作无法撤销。",
  "Create a project first": "请先创建项目",

  // Search.
  "Tasks": "任务",
  "Recent": "最近",
  "No tasks match.": "没有匹配的任务。",
  "No tasks yet.": "还没有任务。",

  // Header counts and relative times.
  "{0} in {1}": "{1} 中的 {0}",
  "Open {0} on GitHub": "在 GitHub 上打开 {0}",
  "just now": "刚刚",
  "{0}m ago": "{0} 分钟前",
  "{0}h ago": "{0} 小时前",
  "{0}d ago": "{0} 天前",

  // Plural nouns; each carries its measure word.
  "task": "个任务",
  "open task": "个未完成任务",
  "project": "个项目",
  "comment": "条评论",
  "computer": "台电脑",

  // Network sharing.
  "Share": "共享",
  "Share this board": "共享此看板",
  "Shared": "已共享",
  ": {0} connected": "：{0}已连接",
  "Shared by {0}{1}": "由 {0} 共享{1}",
  "Shared on your network{1}": "已在你的网络中共享{1}",
  "Share this board, or join another": "共享此看板，或加入其他看板",
  "Only the computer with the board can change this": "只有存放看板的电脑能更改此项",
  "Other computers join it at {0}": "其他电脑可通过{0}加入",
  "the address for their network": "其网络对应的地址",
  "this address": "此地址",
  "this computer's IP address": "这台电脑的 IP 地址",
  "Leave the shared board to share this one": "先离开共享的看板，才能共享此看板",
  "Other computers on your network can use it": "你网络中的其他电脑可以使用它",
  "Copy the address": "复制地址",
  "Connected now": "当前已连接",
  "No other computer is connected yet": "还没有其他电脑连接",
  "A browser": "某台浏览器",
  "Connected to {0}": "已连接到 {0}",
  "Join a shared board": "加入共享的看板",
  "This computer uses that board": "本电脑使用那个看板",
  "Stop sharing to join another board": "先停止共享，才能加入其他看板",
  "The address the sharing computer shows": "共享方电脑显示的地址",
  "Address of the sharing computer": "共享方电脑的地址",
  "Join": "加入",
  "Joining…": "加入中…",
  "Leave": "离开",
  "There are no logins: share only on a network you trust.": "没有登录机制：请只在可信的网络里共享。",

  // Sync.
  "Syncing…": "同步中…",
  "Checking…": "检查中…",
  "Save and sync": "保存并同步",
  "Repository URL": "仓库 URL",
  "Create a private repo on GitHub": "在 GitHub 上新建私有仓库",
  "Keep this board the same on all your machines through a private git repo you own. Use the same repo on each machine.":
    "通过你自己的私有 git 仓库，让所有电脑上的看板保持一致。每台电脑填同一个仓库。",
  "{0} already has a board. Use it on this machine?\n\nThis machine's current board is kept in kanzo.db.bak.":
    "{0} 上已有一个看板。要在这台电脑上使用它吗？\n\n本机当前的看板会保留在 kanzo.db.bak。",
  "Replace the board in {0} with this machine's board instead?": "改为用本机的看板替换 {0} 里的看板？",
  "This machine and another one both changed the board since the last sync, and boards can't be merged.\n\nKeep THIS machine's board, and overwrite the other one?":
    "上次同步后，本机与另一台电脑都改动了看板，两边无法合并。\n\n保留本机的看板并覆盖对方？",
  "Take the other machine's board instead? This machine's board is kept in kanzo.db.bak.":
    "改用对方电脑的看板？本机的看板会保留在 kanzo.db.bak。",

  // Settings.
  "Board": "看板",
  "About": "关于",
  "Language": "界面语言",
  "The language of the board's interface": "看板界面的显示语言",
  "Backlog column": "暂存列",
  "A column before todo, for tasks agents leave alone": "待办之前的列，智能体不会动里面的任务",
  "Task properties": "任务属性",
  "Turn one off to hide it on the board and leave it out of what agents get. Nothing is deleted.":
    "关掉一项即可在看板上隐藏，智能体也拿不到它。内容不会被删除。",
  "Images": "图片",

  // Updates.
  "Version": "版本",
  "Check for updates": "检查更新",
  "Update to {0}": "更新到 {0}",
  "You have {0}": "当前版本 {0}",
  "Update Kanzo from {0} to {1}? The board restarts, which takes a few seconds.":
    "把 Kanzo 从 {0} 更新到 {1}？看板会重启，需要几秒钟。",
  "Checking for updates…": "正在检查更新…",
  "Kanzo {0} is out (you have {1}). Download and install it now? The board restarts.":
    "Kanzo {0} 已发布（当前为 {1}）。现在下载并安装吗？看板会重启。",
  "Kanzo is up to date ({0})": "Kanzo 已是最新版本（{0}）",
  "Can't reach GitHub to check for updates": "连不上 GitHub，无法检查更新",
  "Updating…": "正在更新…",
  "Restart Kanzo to finish": "重启 Kanzo 完成更新",
  "Kanzo {0} is installed. Restart Kanzo to use it.": "Kanzo {0} 已安装，重启 Kanzo 后生效。",

  // New project dialog.
  "The slug starts each task's key, like api#12.": "slug 构成任务键的前缀，如 api#12。",
  "Slug, like api": "slug，如 api",
  "Slug": "slug",
  "Name (optional)": "名称（可选）",
  "Name": "名称",
  "GitHub owner/repo (optional)": "GitHub owner/repo（可选）",
  "GitHub repo": "GitHub 仓库",
  "Letters, digits and -, up to 32": "字母、数字和 -，最长 32 个",
  "Cancel": "取消",
  "Create": "创建",
};
