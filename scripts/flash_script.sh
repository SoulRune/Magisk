#MAGISK
############################################
# Magisk Flash Script (updater-script)
############################################

##############
# Preparation
##############

# Default permissions
umask 022

OUTFD=$2
COMMONDIR=$INSTALLER/assets
CHROMEDIR=$INSTALLER/assets/chromeos

APK="$3"
MAGISKBINTMP=$INSTALLER/bin

if [ ! -f $COMMONDIR/util_functions.sh ]; then
  echo "! Unable to extract zip file!"
  exit 1
fi

# Load utility functions
. $COMMONDIR/util_functions.sh
mkdir -p $MAGISKBINTMP

getvar SYSTEMMODE
SYSTEMINSTALL="$SYSTEMMODE"
[ -z "$SYSTEMINSTALL" ] && SYSTEMINSTALL=false

if echo "$3" | grep -q "systemmagisk"; then
  SYSTEMINSTALL=true
fi

setup_flashable

############
# Detection
############

if echo $MAGISK_VER | grep -q '\.'; then
  PRETTY_VER=$MAGISK_VER
else
  PRETTY_VER="$MAGISK_VER($MAGISK_VER_CODE)"
fi
print_title "Magisk $PRETTY_VER Installer"

is_mounted /data || mount /data || is_mounted /cache || mount /cache
mount_partitions
check_data
get_flags
find_boot_image

[ -z $BOOTIMAGE ] && abort "! Unable to detect target image"
ui_print "- Target image: $BOOTIMAGE"

# Detect version and architecture
api_level_arch_detect

[ $API -lt 23 ] && abort "! Magisk only support Android 6.0 and above"

ui_print "- Device platform: $ABI"

BINDIR=$INSTALLER/lib/$ABI
cd $BINDIR
for file in lib*.so; do mv "$file" "${file:3:${#file}-6}"; done
cd /
cp -af $INSTALLER/lib/$ABI32/libmagisk.so $BINDIR/magisk32 2>/dev/null

# Check if system root is installed and remove
$BOOTMODE || remove_system_su

##############
# Environment
##############

ui_print "- Constructing environment"

# Copy required files
rm -rf $MAGISKBIN 2>/dev/null
mkdir -p $MAGISKBIN 2>/dev/null
cp -af $BINDIR/. $COMMONDIR/. $BBBIN $MAGISKBIN

# Remove files only used by the Magisk app
rm -f $MAGISKBIN/bootctl $MAGISKBIN/main.jar \
  $MAGISKBIN/module_installer.sh $MAGISKBIN/uninstaller.sh

chmod -R 755 $MAGISKBIN

# Copy to temp bin dir
cat "$APK" >"$MAGISKBIN/magisk.apk" 2>/dev/null
cp -af $MAGISKBIN/* $MAGISKBINTMP
chmod -R 755 $MAGISKBINTMP

# addon.d
ADDOND=/system/addon.d
ADDOND_MAGISK=$ADDOND/magisk

if [ "$SYSTEMINSTALL" == "true" ]; then
  unzip -oj "$APK" "assets/app_functions.sh"
  BOOTMODE_OLD="$BOOTMODE"
  . ./app_functions.sh
  BOOTMODE="$BOOTMODE_OLD"
  . $COMMONDIR/util_functions.sh
  ADDOND_MAGISK=/system/etc/init/magisk
  [ -f "$ADDOND/99-magisk.sh" ] && sed -i "s/^SYSTEMINSTALL=.*/SYSTEMINSTALL=true/g" $ADDOND/99-magisk.sh
  if $BOOTMODE; then
    direct_install_system "$MAGISKBINTMP" || { cleanup_system_installation; abort "! Installation failed"; }
  else
    direct_install_system "$MAGISKBINTMP" || { cleanup_system_installation; abort "! Installation failed"; }
  fi
else
  install_magisk
fi

if [ -d /system/addon.d ]; then
  ui_print "- Adding addon.d survival script"
  blockdev --setrw /dev/block/mapper/system$SLOT 2>/dev/null
  mount -o rw,remount /system || mount -o rw,remount /
  rm -rf /system/addon.d/99-magisk.sh 2>/dev/null
  rm -rf /system/addon.d/magisk 2>/dev/null
  if [ "$ADDOND_MAGISK" == "/system/addon.d/magisk" ]; then
    mkdir -p /system/addon.d/magisk
  fi
  cp -af $MAGISKBINTMP/* $ADDOND_MAGISK
  if [ "$ADDOND_MAGISK" == "/system/addon.d/magisk" ]; then
    mv /system/addon.d/magisk/boot_patch.sh /system/addon.d/magisk/boot_patch.sh.in
  fi
  mv $ADDOND_MAGISK/addon.d.sh /system/addon.d/99-magisk.sh
  if [ "$SYSTEMINSTALL" == "true" ]; then
    sed -i "s/^SYSTEMINSTALL=.*/SYSTEMINSTALL=true/g" /system/addon.d/99-magisk.sh
  fi
fi

# Cleanups
$BOOTMODE || recovery_cleanup
rm -rf $TMPDIR

ui_print "- Done"
exit 0
