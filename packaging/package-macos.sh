#!/usr/bin/env bash
#
# 把 MiniProxy 打包成 macOS .app
#
#   packaging/package-macos.sh [--no-ffmpeg] [--no-sign]
#
# 产物：dist/MiniProxy.app （可直接拖进 /Applications）
#
# 流程：构建前端 → 构建后端 release → 组装 Bundle → 取静态 ffmpeg → 生成图标 → 签名
#
# 可调环境变量：
#   MINIPROXY_SIGN_ID     签名身份，默认 "-"（ad-hoc，本地自用）。分发要填
#                         "Developer ID Application: xxx (TEAMID)"
#   MINIPROXY_PKG_CACHE   ffmpeg 下载缓存目录，默认 ~/.cache/miniproxy-packaging
#   MINIPROXY_CARGO_FLAGS cargo 附加参数，默认 "--offline"（依赖已缓存，避免联网）
#
set -euo pipefail
# 有些环境 LANG=C，bash 会把紧跟变量名的全角标点字节并进变量名
# （`$SIGN_ID）` 被当成变量 `SIGN_ID\xef…` → unbound variable），所以变量一律加花括号
export LC_ALL="${LC_ALL:-en_US.UTF-8}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="MiniProxy"
BUNDLE_ID="com.chenqingfeng.miniproxy"
VERSION="$(sed -nE 's/^version *= *"([^"]+)".*/\1/p' "$ROOT/backend/Cargo.toml" | head -1)"
OUT="$ROOT/dist"
APP="$OUT/$APP_NAME.app"
CONTENTS="$APP/Contents"
CACHE="${MINIPROXY_PKG_CACHE:-$HOME/.cache/miniproxy-packaging}"
SIGN_ID="${MINIPROXY_SIGN_ID:--}"
CARGO_FLAGS="${MINIPROXY_CARGO_FLAGS:---offline}"

BUNDLE_FFMPEG=1
DO_SIGN=1
for arg in "$@"; do
  case "$arg" in
    --no-ffmpeg) BUNDLE_FFMPEG=0 ;;
    --no-sign)   DO_SIGN=0 ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done

export PATH="$HOME/.cargo/bin:$PATH"
NODE_BIN="${NODE_BIN:-/Users/chenqingfeng/.workbuddy/binaries/node/versions/22.22.2-3/bin}"
[ -d "$NODE_BIN" ] && export PATH="$NODE_BIN:$PATH"

step() { printf '\n\033[1m▶ %s\033[0m\n' "$1"; }
ok()   { printf '  ✓ %s\n' "$1"; }

# ---------------------------------------------------------------- 1. 前端
step "构建前端 (vite)"
cd "$ROOT/frontend"
if [ ! -d node_modules ]; then
  npm install
fi
npm run build
[ -f dist/index.html ] || { echo "前端构建失败：dist/index.html 不存在" >&2; exit 1; }
ok "frontend/dist 就绪"

# ---------------------------------------------------------------- 2. 后端
step "构建后端 (cargo build --release)"
cd "$ROOT/backend"
# 不传 --offline 时 cargo 可能联网；这里默认离线，依赖缺失再让用户放开
cargo build --release $CARGO_FLAGS
BIN="$ROOT/backend/target/release/miniproxy"
[ -x "$BIN" ] || { echo "后端构建失败：$BIN 不存在" >&2; exit 1; }
ok "miniproxy $(du -h "$BIN" | cut -f1)"

# ---------------------------------------------------------------- 3. 静态 ffmpeg
FFMPEG_SRC=""
FFPROBE_SRC=""
if [ "$BUNDLE_FFMPEG" = 1 ]; then
  step "准备静态 ffmpeg / ffprobe（音视频合并用）"
  mkdir -p "$CACHE"
  # 优先用随仓库放置的本地副本（离线打包）
  for name in ffmpeg ffprobe; do
    if [ -x "$ROOT/packaging/vendor/$name" ]; then
      ok "复用 packaging/vendor/$name"
    fi
  done
  fetch_tool() { # $1=名字 $2=下载地址
    local name="$1" url="$2"
    if [ -x "$ROOT/packaging/vendor/$name" ]; then
      echo "$ROOT/packaging/vendor/$name"; return
    fi
    if [ ! -x "$CACHE/$name" ]; then
      echo "  下载 $name …" >&2
      local zip="$CACHE/$name.zip" tmp="$CACHE/$name.extract"
      curl -fL --retry 2 -o "$zip" "$url"
      rm -rf "$tmp"; mkdir -p "$tmp"
      unzip -oq "$zip" -d "$tmp"
      # 压缩包里可能还有一层目录，按文件名找最里层那个可执行文件
      local found
      found="$(find "$tmp" -type f -name "$name" -perm -u+x | head -1)"
      [ -n "$found" ] || found="$(find "$tmp" -type f -name "$name" | head -1)"
      [ -n "$found" ] || { echo "解包后没找到 $name" >&2; exit 1; }
      cp "$found" "$CACHE/$name"; chmod +x "$CACHE/$name"; rm -rf "$tmp"
    fi
    echo "$CACHE/$name"
  }
  FFMPEG_SRC="$(fetch_tool ffmpeg  https://www.osxexperts.net/ffmpeg9arm.zip)"
  FFPROBE_SRC="$(fetch_tool ffprobe https://www.osxexperts.net/ffprobe9arm.zip)"
  for t in "$FFMPEG_SRC" "$FFPROBE_SRC"; do
    local_arch="$(file -b "$t" | sed -nE 's/.*(arm64|x86_64).*/\1/p')"
    [ "$local_arch" = "arm64" ] || echo "  ⚠️  $(basename "$t") 是 ${local_arch}，在 Apple Silicon 上需要 Rosetta"
    ok "$(basename "$t") $(du -h "$t" | cut -f1) [$local_arch]"
  done
fi

# ---------------------------------------------------------------- 4. 组装 Bundle
step "组装 $APP_NAME.app"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources/static"
if [ "$BUNDLE_FFMPEG" = 1 ]; then
  mkdir -p "$CONTENTS/Resources/bin"
  cp "$FFMPEG_SRC"  "$CONTENTS/Resources/bin/ffmpeg"
  cp "$FFPROBE_SRC" "$CONTENTS/Resources/bin/ffprobe"
  chmod +x "$CONTENTS/Resources/bin/ffmpeg" "$CONTENTS/Resources/bin/ffprobe"
fi

# 真正的后端二进制；CFBundleExecutable 是下面的原生外壳（窗口 + WKWebView）
cp "$BIN" "$CONTENTS/MacOS/miniproxy-bin"
chmod +x "$CONTENTS/MacOS/miniproxy-bin"

# 原生外壳：点开图标直接出窗口，不再跳浏览器。
# 用系统自带 clang 编译，零额外依赖；外壳负责拉起 miniproxy-bin 并优雅退出。
step "编译原生外壳 (AppShell.m)"
clang -fobjc-arc -O2 \
  -framework Cocoa -framework WebKit \
  "$ROOT/packaging/shell/AppShell.m" \
  -o "$CONTENTS/MacOS/$APP_NAME"
ok "MacOS/$APP_NAME"

cp -R "$ROOT/frontend/dist/." "$CONTENTS/Resources/static/"
ok "静态文件 $(du -sh "$CONTENTS/Resources/static" | cut -f1)"

cat > "$CONTENTS/PkgInfo" <<< 'APPL????'

cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>       <string>zh_CN</string>
  <key>CFBundleExecutable</key>              <string>$APP_NAME</string>
  <key>CFBundleIdentifier</key>              <string>$BUNDLE_ID</string>
  <key>CFBundleInfoDictionaryVersion</key>   <string>6.0</string>
  <key>CFBundleName</key>                    <string>$APP_NAME</string>
  <key>CFBundleDisplayName</key>             <string>MiniProxy 抓包代理</string>
  <key>CFBundlePackageType</key>             <string>APPL</string>
  <key>CFBundleShortVersionString</key>      <string>$VERSION</string>
  <key>CFBundleVersion</key>                 <string>$VERSION</string>
  <key>CFBundleIconFile</key>                <string>AppIcon</string>
  <key>LSMinimumSystemVersion</key>          <string>11.0</string>
  <key>LSApplicationCategoryType</key>       <string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key>         <true/>
  <key>NSHumanReadableCopyright</key>        <string>MiniProxy</string>
  <!-- macOS 15+ 监听/访问局域网会弹权限框，这里给出理由文案 -->
  <key>NSLocalNetworkUsageDescription</key>
  <string>MiniProxy 需要监听本机端口以接收浏览器和局域网设备发来的代理请求。</string>
</dict>
</plist>
PLIST
plutil -lint "$CONTENTS/Info.plist" >/dev/null
ok "Info.plist (版本 ${VERSION})"

# ---------------------------------------------------------------- 5. 图标
if [ -f "$ROOT/packaging/AppIcon.png" ]; then
  step "生成 AppIcon.icns"
  ICONSET="$(mktemp -d)/AppIcon.iconset"
  mkdir -p "$ICONSET"
  for size in 16 32 128 256 512; do
    sips -z $size $size          "$ROOT/packaging/AppIcon.png" --out "$ICONSET/icon_${size}x${size}.png"      >/dev/null
    sips -z $((size*2)) $((size*2)) "$ROOT/packaging/AppIcon.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/AppIcon.icns"
  rm -rf "$(dirname "$ICONSET")"
  ok "AppIcon.icns"
else
  echo "  · 跳过图标（未找到 packaging/AppIcon.png）"
fi

# ---------------------------------------------------------------- 6. 签名
xattr -cr "$APP" 2>/dev/null || true
if [ "$DO_SIGN" = 1 ]; then
  step "签名（identity: ${SIGN_ID}）"
  # 由内到外签名：先嵌套的可执行文件，再 Bundle 本身。
  # 不用 --deep（已废弃，且不会给嵌套二进制补上正确的签名结构）。
  if [ "$BUNDLE_FFMPEG" = 1 ]; then
    for t in ffmpeg ffprobe; do
      codesign --force --sign "$SIGN_ID" "$CONTENTS/Resources/bin/$t"
    done
  fi
  codesign --force --sign "$SIGN_ID" "$CONTENTS/MacOS/miniproxy-bin"
  codesign --force --sign "$SIGN_ID" "$CONTENTS/MacOS/$APP_NAME"
  codesign --force --sign "$SIGN_ID" "$APP"
  codesign --verify --strict --verbose=1 "$APP" 2>&1 | sed 's/^/  /'
  ok "签名完成"
else
  echo "  · 跳过签名（--no-sign）"
fi

# ---------------------------------------------------------------- 7. 汇总
step "完成"
SIZE="$(du -sh "$APP" | cut -f1)"
cat <<EOF
  $APP   ($SIZE)

  安装：  cp -R "$APP" /Applications/
  运行：  open "$APP"        # 会自动打开界面 http://127.0.0.1:9000
  日志：  tail -f ~/.miniproxy/miniproxy.log
  卸载：  rm -rf /Applications/$APP_NAME.app
  清配置：rm -rf ~/.miniproxy          # 含 CA、上游级联、分流规则

  ⚠️ 首次使用仍需手动信任 CA（否则 HTTPS 只能以隧道形式记录）：
     sudo security add-trusted-cert -d -r trustRoot \\
       -k /Library/Keychains/System.keychain ~/.miniproxy/ca.crt
EOF
