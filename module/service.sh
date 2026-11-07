#!/system/bin/sh
umask 077
MODDIR=${0%/*}
while [ "$(getprop sys.boot_completed)" != "1" ]; do
  [ -f "$MODDIR/disable" ] && exit 0
  sleep 2
done
DATA=/data/adb/natpierce-rs
mkdir -p "$DATA"
chmod 700 "$DATA"
[ -f "$DATA/safe_mode" ] && exit 0
if [ -f "$DATA/settings.json" ]; then
  tr -d '[:space:]' <"$DATA/settings.json" | grep -q '"autostart":false' && exit 0
fi
rm -f "$DATA/stop"
nohup sh "$MODDIR/bin/watchdog.sh" >/dev/null 2>&1 </dev/null &
