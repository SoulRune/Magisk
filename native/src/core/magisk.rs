use crate::consts::{
    APPLET_NAMES, DEVICEDIR, INTERNAL_DIR, MAGISK_VER_CODE, MAGISK_VERSION, POST_FS_DATA_WAIT_TIME,
    SEPOL_PROC_DOMAIN,
};
use crate::daemon::connect_daemon;
use crate::ffi::{RequestCode, denylist_cli, get_magisk_tmp, install_module, unlock_blocks};
use crate::mount::find_preinit_device;
use crate::selinux::restorecon;
use crate::socket::{Decodable, Encodable};
use argh::FromArgs;
use base::{CmdArgs, EarlyExitExt, LoggedResult, Utf8CString, argh, clone_attr, cstr};
use nix::mount::MsFlags;
use nix::poll::{PollFd, PollFlags, PollTimeout};
use nix::sys::statfs::{FsType, TMPFS_MAGIC, statfs};
use std::ffi::c_char;
use std::fs;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::exit;

fn print_usage() {
    eprintln!(
        r#"Magisk - Multi-purpose Utility

Usage: magisk [applet [arguments]...]
   or: magisk [options]...

Options:
   -c                        print current binary version
   -v                        print running daemon version
   -V                        print running daemon version code
   --list                    list all available applets
   --remove-modules [-n]     remove all modules, reboot if -n is not provided
   --install-module ZIP      install a module zip file

Advanced Options (Internal APIs):
   --daemon                  manually start magisk daemon
   --stop                    remove all magisk changes and stop daemon
   --[init trigger]          callback on init triggers. Valid triggers:
                             post-fs-data, service, boot-complete, zygote-restart
   --unlock-blocks           set BLKROSET flag to OFF for all block devices
   --restorecon              restore selinux context on Magisk files
   --clone-attr SRC DEST     clone permission, owner, and selinux context
   --clone SRC DEST          clone SRC to DEST
   --sqlite SQL              exec SQL commands to Magisk database
   --path                    print Magisk tmpfs mount path
   --denylist ARGS           denylist config CLI
   --preinit-device          resolve a device to store preinit files

Available applets:
     {}
"#,
        APPLET_NAMES.join(", ")
    );
}

const RAMFS_MAGIC: u32 = 0x858458f6;

fn is_rootfs() -> bool {
    use num_traits::AsPrimitive;
    if let Ok(s) = statfs(cstr!("/")) {
        s.filesystem_type() == FsType(RAMFS_MAGIC.as_()) || s.filesystem_type() == TMPFS_MAGIC
    } else {
        false
    }
}

fn tmpfs_mount(dest: &str) -> i32 {
    use nix::mount::mount;
    match mount(
        Some("magisk"),
        dest,
        Some("tmpfs"),
        MsFlags::empty(),
        Some("mode=755"),
    ) {
        Ok(()) => {
            eprintln!("tmpfs mount: {}", dest);
            0
        }
        Err(e) => {
            eprintln!("tmpfs mount failed {}: {}", dest, e);
            -1
        }
    }
}

fn clone_dir(src: &str, dest: &str) {
    let entries = match fs::read_dir(src) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let src_path = entry.path();
        let dest_path = Path::new(dest).join(entry.file_name());
        if let Ok(meta) = fs::symlink_metadata(&src_path) {
            if meta.is_symlink() {
                if let Ok(target) = fs::read_link(&src_path) {
                    let _ = symlink(&target, &dest_path);
                }
            } else if meta.is_dir() {
                let _ = fs::create_dir_all(&dest_path);
                let _ = fs::set_permissions(&dest_path, meta.permissions());
                clone_dir(
                    src_path.to_str().unwrap_or(""),
                    dest_path.to_str().unwrap_or(""),
                );
            } else {
                let _ = fs::hard_link(&src_path, &dest_path);
            }
        }
    }
}

fn mount_sbin() -> i32 {
    use nix::mount::mount;
    if is_rootfs() {
        if let Err(e) = mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REMOUNT,
            None::<&str>,
        ) {
            eprintln!("remount / rw failed: {}", e);
            return -1;
        }
        let _ = fs::create_dir_all("/sbin");
        let _ = fs::remove_dir_all("/root");
        let _ = fs::create_dir_all("/root");
        clone_dir("/sbin", "/root");
        if tmpfs_mount("/sbin") != 0 {
            let _ = mount(
                None::<&str>,
                "/",
                None::<&str>,
                MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
                None::<&str>,
            );
            return -1;
        }
        cstr!("/sbin").follow_link().set_secontext(cstr!("u:object_r:rootfs:s0")).ok();
        recreate_sbin("/root", false);
        let _ = mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
            None::<&str>,
        );
    } else {
        if tmpfs_mount("/sbin") != 0 {
            return -1;
        }
        cstr!("/sbin").follow_link().set_secontext(cstr!("u:object_r:rootfs:s0")).ok();
        let intlroot = format!("/sbin/{}", INTERNAL_DIR);
        let mirdir = format!("/sbin/{}/mirror", INTERNAL_DIR);
        let sysroot = format!("{}/system_root", mirdir);
        let _ = fs::create_dir_all(&intlroot);
        let _ = fs::create_dir_all(&sysroot);
        let _ = mount(
            Some("/"),
            sysroot.as_str(),
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        );
        let mirror_sbin = format!("{}/sbin", sysroot);
        recreate_sbin(&mirror_sbin, true);
        let _ = nix::mount::umount2(sysroot.as_str(), nix::mount::MntFlags::MNT_DETACH);
    }
    0
}

fn recreate_sbin(mirror: &str, use_bind_mount: bool) {
    use nix::mount::mount;
    let magisk_tmp = get_magisk_tmp();
    let entries = match fs::read_dir(mirror) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let sbin_path = format!("/sbin/{}", name_str);
        let src_path = format!("{}/{}", mirror, name_str);
        let tmp_path = if magisk_tmp.is_empty() {
            format!("/sbin/{}", name_str)
        } else {
            format!("{}/{}", magisk_tmp, name_str)
        };
        if Path::new(&tmp_path).exists() && !magisk_tmp.is_empty() {
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(&src_path) {
            if meta.is_symlink() {
                if let Ok(target) = fs::read_link(&src_path) {
                    let _ = symlink(&target, &sbin_path);
                }
            } else if use_bind_mount {
                let mode = meta.permissions();
                if meta.is_dir() {
                    let _ = fs::create_dir_all(&sbin_path);
                } else {
                    let _ = fs::File::create(&sbin_path);
                }
                let _ = fs::set_permissions(&sbin_path, mode);
                let _ = mount(
                    Some(src_path.as_str()),
                    sbin_path.as_str(),
                    None::<&str>,
                    MsFlags::MS_BIND,
                    None::<&str>,
                );
            } else {
                let _ = symlink(&src_path, &sbin_path);
            }
        }
    }
}

fn install_applet(path: &str) {
    for name in APPLET_NAMES {
        let dest = format!("{}/{}", path, name);
        let _ = symlink("./magisk", &dest);
    }
    let supolicy = format!("{}/supolicy", path);
    let _ = symlink("./magiskpolicy", &supolicy);
}

fn handle_auto_selinux() {
    use std::io::{Read, Write};
    if let Ok(mut f) = fs::OpenOptions::new().read(true).write(true).open("/proc/self/attr/current") {
        let magisk_con = format!("u:r:{}:s0\0", SEPOL_PROC_DOMAIN);
        let su_con = "u:r:su:s0\0";
        let written = f.write_all(magisk_con.as_bytes()).is_ok()
            || f.write_all(su_con.as_bytes()).is_ok();
        if written {
            let mut current = String::new();
            let _ = f.read_to_string(&mut current);
            eprintln!("SeLinux context: {}", current.trim_end_matches('\0'));
        }
    }
}

#[derive(FromArgs)]
struct Cli {
    #[argh(subcommand)]
    action: MagiskAction,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum MagiskAction {
    LocalVersion(LocalVersion),
    Version(Version),
    VersionCode(VersionCode),
    List(ListApplets),
    RemoveModules(RemoveModules),
    InstallModule(InstallModule),
    Daemon(StartDaemon),
    Stop(StopDaemon),
    PostFsData(PostFsData),
    Service(ServiceCmd),
    BootComplete(BootComplete),
    ZygoteRestart(ZygoteRestart),
    UnlockBlocks(UnlockBlocks),
    RestoreCon(RestoreCon),
    CloneAttr(CloneAttr),
    CloneFile(CloneFile),
    Sqlite(Sqlite),
    Path(PathCmd),
    DenyList(DenyList),
    PreInitDevice(PreInitDevice),
    SetupSbin(SetupSbin),
    MountSbin(MountSbinCmd),
    Install(InstallApplet),
}

#[derive(FromArgs)]
#[argh(subcommand, name = "-c")]
struct LocalVersion {}

#[derive(FromArgs)]
#[argh(subcommand, name = "-v")]
struct Version {}

#[derive(FromArgs)]
#[argh(subcommand, name = "-V")]
struct VersionCode {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--list")]
struct ListApplets {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--remove-modules")]
struct RemoveModules {
    #[argh(switch, short = 'n')]
    no_reboot: bool,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "--install-module")]
struct InstallModule {
    #[argh(positional)]
    zip: Utf8CString,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "--daemon")]
struct StartDaemon {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--stop")]
struct StopDaemon {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--post-fs-data")]
struct PostFsData {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--service")]
struct ServiceCmd {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--boot-complete")]
struct BootComplete {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--zygote-restart")]
struct ZygoteRestart {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--unlock-blocks")]
struct UnlockBlocks {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--restorecon")]
struct RestoreCon {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--clone-attr")]
struct CloneAttr {
    #[argh(positional)]
    from: Utf8CString,
    #[argh(positional)]
    to: Utf8CString,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "--clone")]
struct CloneFile {
    #[argh(positional)]
    from: Utf8CString,
    #[argh(positional)]
    to: Utf8CString,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "--sqlite")]
struct Sqlite {
    #[argh(positional)]
    sql: String,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "--path")]
struct PathCmd {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--denylist")]
struct DenyList {
    #[argh(positional, greedy)]
    args: Vec<String>,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "--preinit-device")]
struct PreInitDevice {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--setup-sbin")]
/// Setup sbin with magisk binaries
struct SetupSbin {
    #[argh(positional)]
    bin_dir: String,
    #[argh(positional)]
    tmpfs_dest: Option<String>,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "--mount-sbin")]
/// Mount tmpfs on /sbin
struct MountSbinCmd {}

#[derive(FromArgs)]
#[argh(subcommand, name = "--install")]
/// Install magisk applet symlinks
struct InstallApplet {
    #[argh(positional)]
    path: Option<String>,
}

impl MagiskAction {
    fn exec(self) -> LoggedResult<i32> {
        use MagiskAction::*;
        match self {
            LocalVersion(_) => {
                #[cfg(debug_assertions)]
                println!("{MAGISK_VERSION}:MAGISK:D ({MAGISK_VER_CODE})");
                #[cfg(not(debug_assertions))]
                println!("{MAGISK_VERSION}:MAGISK:R ({MAGISK_VER_CODE})");
            }
            Version(_) => {
                let mut fd = connect_daemon(RequestCode::CHECK_VERSION, false)?;
                let ver = String::decode(&mut fd)?;
                println!("{ver}");
            }
            VersionCode(_) => {
                let mut fd = connect_daemon(RequestCode::CHECK_VERSION_CODE, false)?;
                let ver = i32::decode(&mut fd)?;
                println!("{ver}");
            }
            List(_) => {
                for name in APPLET_NAMES {
                    println!("{name}");
                }
            }
            RemoveModules(self::RemoveModules { no_reboot }) => {
                let mut fd = connect_daemon(RequestCode::REMOVE_MODULES, false)?;
                let do_reboot = !no_reboot;
                do_reboot.encode(&mut fd)?;
                return Ok(i32::decode(&mut fd)?);
            }
            InstallModule(self::InstallModule { zip }) => {
                install_module(&zip);
            }
            Daemon(_) => {
                let _ = connect_daemon(RequestCode::START_DAEMON, true)?;
            }
            Stop(_) => {
                let mut fd = connect_daemon(RequestCode::STOP_DAEMON, false)?;
                return Ok(i32::decode(&mut fd)?);
            }
            PostFsData(_) => {
                let fd = connect_daemon(RequestCode::POST_FS_DATA, true)?;
                let mut pfd = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
                nix::poll::poll(
                    &mut pfd,
                    PollTimeout::try_from(POST_FS_DATA_WAIT_TIME * 1000)?,
                )?;
            }
            Service(_) => {
                let _ = connect_daemon(RequestCode::LATE_START, true)?;
            }
            BootComplete(_) => {
                let _ = connect_daemon(RequestCode::BOOT_COMPLETE, false)?;
            }
            ZygoteRestart(_) => {
                let _ = connect_daemon(RequestCode::ZYGOTE_RESTART, false)?;
            }
            UnlockBlocks(_) => {
                unlock_blocks();
            }
            RestoreCon(_) => {
                restorecon();
            }
            CloneAttr(self::CloneAttr { from, to }) => {
                clone_attr(&from, &to)?;
            }
            CloneFile(self::CloneFile { from, to }) => {
                from.copy_to(&to)?;
            }
            Sqlite(self::Sqlite { sql }) => {
                let mut fd = connect_daemon(RequestCode::SQLITE_CMD, false)?;
                sql.encode(&mut fd)?;
                loop {
                    let line = String::decode(&mut fd)?;
                    if line.is_empty() {
                        return Ok(0);
                    }
                    println!("{line}");
                }
            }
            Path(_) => {
                let tmp = get_magisk_tmp();
                if tmp.is_empty() {
                    return Ok(1);
                } else {
                    println!("{tmp}");
                }
            }
            DenyList(self::DenyList { mut args }) => {
                return Ok(denylist_cli(&mut args));
            }
            PreInitDevice(_) => {
                let name = find_preinit_device();
                if name.is_empty() {
                    return Ok(1);
                } else {
                    println!("{name}");
                }
            }
            SetupSbin(self::SetupSbin { bin_dir, tmpfs_dest }) => {
                let magisk_tmp = tmpfs_dest.as_deref().unwrap_or("/sbin");
                if magisk_tmp == "/sbin" {
                    if mount_sbin() != 0 {
                        return Ok(-1);
                    }
                } else if tmpfs_mount(magisk_tmp) != 0 {
                    return Ok(-1);
                }
                let bins = ["magisk", "magisk32", "magiskpolicy", "stub.apk"];
                for bin in &bins {
                    let src = format!("{}/{}", bin_dir, bin);
                    let dest = format!("{}/{}", magisk_tmp, bin);
                    if std::path::Path::new(&src).exists() {
                        let _ = fs::copy(&src, &dest);
                        let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o755));
                    }
                }
                if std::env::set_current_dir(magisk_tmp).is_err() {
                    return Ok(-1);
                }
                let _ = fs::create_dir_all(INTERNAL_DIR);
                let _ = fs::create_dir(DEVICEDIR);
                install_applet(magisk_tmp);
            }
            MountSbin(_) => {
                return Ok(mount_sbin());
            }
            Install(self::InstallApplet { path }) => {
                let p = path.as_deref().unwrap_or("/sbin");
                install_applet(p);
            }
        };
        Ok(0)
    }
}

pub fn magisk_main(argc: i32, argv: *mut *mut c_char) -> i32 {
    if argc < 2 {
        print_usage();
        exit(1);
    }
    let mut cmds = CmdArgs::new(argc, argv.cast()).0;
    if cmds.len() >= 2 && cmds[1] == "--auto-selinux" {
        handle_auto_selinux();
        cmds.remove(1);
        if cmds.len() < 2 {
            print_usage();
            exit(1);
        }
    }
    // We need to manually inject "--" so that all actions can be treated as subcommands
    cmds.insert(1, "--");
    let cli = Cli::from_args(&cmds[..1], &cmds[1..]).on_early_exit(print_usage);
    cli.action.exec().unwrap_or(1)
}
