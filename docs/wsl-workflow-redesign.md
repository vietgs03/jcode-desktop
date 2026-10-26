# Thiết kế: WSL-first workflow, Session browser, Code review

Trạng thái: đề xuất, chưa code. Mục tiêu: dùng Desktop giống hệt flow CLI
`cd ~/go-chathub/go-chathub && jcode`, nhưng có UI tốt hơn.

Phạm vi: CHỈ sửa Jcode Desktop (repo `jcode-desktop`). Jcode CLI chỉ là ví dụ tham khảo
về trải nghiệm (flow `cd` + `jcode`, màn `/session`), không đổi gì ở CLI. Dữ liệu còn thiếu
(model, số tin nhắn, token...) Desktop tự đọc phần đầu file session qua `wsl.exe` trong
`jcode-desktop-harness`, không sửa API của jcode.

## 0. Hiện trạng (đã đọc code, không đoán)

| Chỗ | Đang làm gì | Vấn đề |
|---|---|---|
| Ctrl+O Open folder (`workspace.rs open_folder`) | Duyệt thư mục bằng `std::fs` trên Windows, gốc là home Windows | Không thấy được thư mục WSL. Chọn xong gọi `open_local_draft`, tạo session LOCAL Windows, bỏ qua `default_remote_host = wsl` |
| New session khi default là WSL (`open_remote_draft`) | `CreateRemoteSession { working_dir: None }` | Luôn mở ở `~` của WSL, không vào được project |
| Folder search / gợi ý thư mục | Lấy từ path local + session local | Không gợi ý repo WSL |
| `/session` picker (`workspace_resume.rs`) | Click 1 lần = resume luôn | Không xem trước được, dễ mở nhầm, loãng |
| Preview trong picker | Đọc file local `persisted_history::load` | Từ chối id có `/`, nên MỌI session `ssh://wsl/...` báo "Preview unavailable" |
| Info mỗi dòng | title, dir, status | Thiếu: số tin nhắn, model, token, ngày tạo/hoạt động, prompt đầu tiên |
| Changes review (`panel_changes.rs`) | Working tree: staged/unstaged/untracked + diff từng file | Không có lịch sử commit, không xem diff theo commit, không nhóm theo lượt agent |
| Search | Chỉ có tìm session trong picker | Không có tìm trong transcript (Ctrl+F) |

## 1. WSL là "máy" thật sự, không phải ngoại lệ

Ý tưởng: thêm khái niệm `Machine` (Local | Wsl(distro) | Ssh(host)) và MỌI thao tác
mở thư mục / tạo session / terminal / git đều đi qua máy đang chọn.

- Máy mặc định = `default_remote_host` (đang là `wsl`). Hiển thị rõ ở header sidebar:
  `🐧 WSL · Ubuntu` với dropdown đổi máy.
- **Open folder (Ctrl+O)** khi máy là WSL:
  - Duyệt thư mục qua `wsl.exe` (một lệnh liệt kê, có cache, chạy nền), gốc là `~` của WSL.
  - Ô search gõ `go-chathub` gợi ý từ: repo WSL đã từng có session, thư mục con của
    `~` sâu 3 cấp chứa `.git` (quét nền, cache 5 phút).
  - Enter: `CreateRemoteSession { host: "wsl", working_dir: "/home/viethx/go-chathub/go-chathub" }`.
    Panel mở ra đúng như `cd ... && jcode`: tiêu đề = tên repo, pill nhánh git, terminal
    Ctrl+` vào đúng thư mục đó.
- **Recent projects**: màn hình trống / sidebar có "Projects" gồm các repo WSL gần đây
  (lấy từ working_dir của session WSL), click = session mới trong repo đó.
- **New session (Ctrl+N)** trong panel đang ở repo X thì session mới cũng ở repo X
  (giống mở thêm tab terminal cùng thư mục), không về `~`.
- **Drag & drop** thư mục `\\wsl.localhost\Ubuntu\home\...` vào app: tự đổi sang path Linux.

Rủi ro: liệt kê thư mục qua `wsl.exe` mỗi lần ~300ms. Giải pháp: một tiến trình `wsl.exe`
giữ sống (giống kết nối session list đã cache ở `acbf214`) và cache theo thư mục.

## 2. Session browser kiểu CLI `/session`

Bố cục 2 cột, giữ phím như CLI:

```
┌ Sessions ─────────────── [All|Active|Saved|Crashed] [Machine: WSL ▾] [Repo: any ▾] ┐
│ 🔎 tìm theo tên, nội dung, repo...                                                  │
├──────────────────────────────┬───────────────────────────────────────────────────────┤
│ ● 🦊 fox  Fix PvP ladder     │ 🦊 fox · Fix PvP ladder          [Resume ⏎] [Fork]     │
│   go-chathub · main · 2m ago │ ~/go-chathub/go-chathub · main · WSL                  │
│   142 msg · opus · 81k tok   │ Tạo 23/10 14:02 · hoạt động 2 phút trước              │
│ ○ 🐙 octopus Changes review  │ 142 tin nhắn (38 user) · claude-opus · ~81k token     │
│   ...                        │ Swarm: coordinator, 4 agents (2 running)              │
│                              │ ───────────────────────────────────────────────────── │
│                              │ (transcript preview, render thật, cuộn được)         │
└──────────────────────────────┴───────────────────────────────────────────────────────┘
 ↑↓ chọn · Space/→ xem preview · Enter mở · Ctrl+Enter mở panel mới · Esc đóng
```

- **Click 1 lần = chọn + preview**, KHÔNG mở. Mở bằng Enter, double-click, hoặc nút Resume.
- **Preview chạy cho WSL**: dùng `Command::PeekRemote` (đã có, đọc phần đuôi file nhanh
  `stored_tail_fast`) thay vì đọc file local. Hiển thị 60 tin nhắn cuối, nút "Load full".
- **Info đầy đủ** như CLI: số tin nhắn (user/assistant), model, provider, token ước tính,
  ngày tạo, lần hoạt động cuối, prompt đầu tiên, nhánh git, máy, trạng thái swarm.
  Desktop tự đọc phần metadata ở đầu file session (qua `wsl.exe`, chạy nền, có cache),
  không sửa jcode CLI hay API của nó.
- Lọc theo Repo và Machine, nhóm theo repo (giống sidebar Projects).

## 3. Code review kiểu Warp / Cursor

Một view "Review" riêng (Ctrl+Alt+G, nút trên header panel, `/review`), gắn với repo của
session đang active. 3 tab:

### 3.1 Changes (đã có, nâng cấp)
Working tree hiện tại. Thêm: stage/unstage từng file hoặc hunk, discard (có xác nhận),
commit với message (agent có thể gợi ý message), mở file trong editor.

### 3.2 Agent turns (kiểu Cursor "các bước sửa")
Mỗi lượt agent = một "change set": danh sách file agent sửa trong lượt đó (lấy từ tool
call `edit`/`apply_patch`/`write` trong transcript, đã có dữ liệu), +/- mỗi file, diff
của chính lượt đó. Dòng thời gian:

```
Lượt 12 · 14:32 · "fix ladder rank calc"     3 files +42 -10   [Xem] [Revert lượt này]
  ├ internal/server/pvp_ladder.go   +30 -8
  ├ internal/server/pvp_common.go   +10 -2
  └ internal/server/pvp_test.go     +2
Lượt 11 · 14:20 · ...
```
Click file mở diff của lượt đó (dùng DiffView có sẵn, split/unified). Đây là phần
"next step sửa các file nào với nhau" như Cursor. Revert lượt cần cẩn thận: chỉ cho khi
file chưa bị sửa sau đó, có xác nhận.

### 3.3 History (commit graph)
`git log --graph` dạng cây (nhánh, merge), mỗi commit: hash, message, tác giả, thời gian,
số file. Click commit = danh sách file + diff của commit (`git show`). So sánh 2 commit
hoặc nhánh với main. Tất cả git chạy nền qua `session_git::Location` (WSL được).

Giới hạn: log tải 200 commit đầu, cuộn tải thêm. Diff cắt 2MB như hiện tại.

## 4. Search

- **Ctrl+F trong panel**: tìm trong transcript đang mở, highlight, Enter/Shift+Enter
  nhảy kết quả, đếm "3/17".
- **Ctrl+Shift+F toàn cục**: tìm trong mọi session (local + WSL), kết quả mở trong
  Session browser ở chế độ preview đúng đoạn có kết quả. WSL cần tìm phía WSL
  (một lệnh `grep -l` qua `wsl.exe` hoặc API bridge), không quét qua `\\wsl$`.

## 5. Thứ tự làm và chia việc (đề xuất)

| Đợt | Việc | Ước lượng |
|---|---|---|
| 1 | Machine model + Open folder WSL + New session theo repo + Recent projects | lớn |
| 1 | Session browser: click = preview, preview WSL qua PeekRemote, info đầy đủ | vừa |
| 2 | Review view: tab Agent turns + tab History (commit graph) | lớn |
| 2 | Search Ctrl+F trong panel | vừa |
| 3 | Stage/commit trong Changes, Revert lượt, global search WSL | vừa |

Mỗi đợt có GPUI test trên Windows với WSL thật, hot reload vào app để bạn thử.

## 6. Quyết định (user chốt 26/10)

1. Session browser: click 1 lần = chọn + preview, Enter / double-click / nút Resume = mở.
2. Review: chọn theo UX chuyên nghiệp nhất hiện nay (xem 6.1).
3. Revert lượt agent / discard / stage-commit: để đợt 3.
4. Giữ cả Windows và WSL, đổi qua lại như VS Code Remote (xem 6.2).

### 6.1 Review: bố cục chọn

Tham khảo: Warp Code Review panel (docs.warp.dev/code/code-review), Zed Project Diff +
Git Graph, Cursor agent review. Điểm chung của các tool tốt nhất:
- Review là **pane dock bên phải cuộc chat**, không che chat, vì giá trị chính là vừa xem diff
  vừa nói tiếp với agent. Có nút phóng to toàn màn hình (Ctrl+Shift+Enter / nút ⤢) và
  kéo đổi độ rộng.
- **Một luồng diff cuộn liên tục** (mọi file xếp chồng như multibuffer của Zed / Warp),
  bên trái là **cây file** có +/-; click file = cuộn tới file đó, không mở view riêng.
- **Bộ chọn phạm vi** ở header: `Uncommitted` | `vs main` | `vs nhánh…` | `Lượt agent #N` |
  `Commit abc123`. Cùng một vùng diff hiển thị mọi phạm vi.
- **Tab History**: commit graph bên trái pane, chọn commit thì diff hiện ngay bên cạnh
  (không đè lên graph, điều người dùng Zed đang xin).
- **Lối vào**: chip `+A -D` ở chân panel (đã có), nút "Review changes" cuối mỗi lượt agent
  có sửa file (như Warp), Ctrl+Alt+G, `/review`.
- **Gửi diff cho agent**: nút "Attach to chat" trên file / hunk, chèn đoạn diff vào ô nhập.
- Cập nhật realtime sau mỗi lượt agent và khi file đổi (đã có cơ chế refresh TurnDone).

### 6.2 Đổi Windows / WSL như VS Code Remote

- Góc trái dưới sidebar có **Remote indicator**: `>< WSL: Ubuntu` hoặc `>< Windows`.
  Click mở menu: `Windows (local)`, từng distro WSL, các SSH host đã lưu, `Open folder…`.
- Đổi máy = đổi ngữ cảnh cho mọi thao tác mới (Open folder, New session, terminal, gợi ý
  project). Session đang mở giữ nguyên máy của nó, mỗi panel có badge máy.
- Lưu lựa chọn bằng `persist_default_remote_host` (đã có). Mỗi cửa sổ có thể khác nhau.
- Command palette: `Open Folder in WSL…`, `Open Folder in Windows…`, `Switch to WSL/Windows`.
