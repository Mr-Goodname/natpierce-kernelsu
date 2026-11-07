#!/system/bin/sh
SKIPUNZIP=0
[ "$ARCH" = "arm64" ] || abort "仅支持 ARM64 设备"
[ -e /dev/net/tun ] || abort "内核不支持 TUN"
ui_print "安装 Chimiko · 多 AI 接力制作的 Rust 管理服务"
ui_print "不捆绑官方核心，请首次打开 WebUI 下载"
ui_print "数据保存在 /data/adb/natpierce-rs，升级时保留"
set_perm_recursive "$MODPATH" 0 0 0755 0644
set_perm_recursive "$MODPATH/bin" 0 0 0755 0755
set_perm "$MODPATH/service.sh" 0 0 0755
set_perm "$MODPATH/uninstall.sh" 0 0 0755
touch "$MODPATH/skip_mount"
