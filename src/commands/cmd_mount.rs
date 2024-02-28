use bch_bindgen::{bcachefs, bcachefs::bch_sb_handle, opt_set};
use log::{info, debug, error, LevelFilter};
use clap::Parser;
use uuid::Uuid;
use std::io::{stdout, IsTerminal};
use std::path::PathBuf;
use crate::key;
use crate::key::UnlockPolicy;
use std::ffi::{CString, c_char, c_void};
use std::os::unix::ffi::OsStrExt;

fn mount_inner(
    src: String,
    target: impl AsRef<std::path::Path>,
    fstype: &str,
    mountflags: libc::c_ulong,
    data: Option<String>,
) -> anyhow::Result<()> {

    // bind the CStrings to keep them alive
    let src = CString::new(src)?;
    let target = CString::new(target.as_ref().as_os_str().as_bytes())?;
    let data = data.map(CString::new).transpose()?;
    let fstype = CString::new(fstype)?;

    // convert to pointers for ffi
    let src = src.as_c_str().to_bytes_with_nul().as_ptr() as *const c_char;
    let target = target.as_c_str().to_bytes_with_nul().as_ptr() as *const c_char;
    let data = data.as_ref().map_or(std::ptr::null(), |data| {
        data.as_c_str().to_bytes_with_nul().as_ptr() as *const c_void
    });
    let fstype = fstype.as_c_str().to_bytes_with_nul().as_ptr() as *const c_char;

    let ret = {
        info!("mounting filesystem");
        // REQUIRES: CAP_SYS_ADMIN
        unsafe { libc::mount(src, target, fstype, mountflags, data) }
    };
    match ret {
        0 => Ok(()),
        _ => Err(crate::ErrnoError(errno::errno()).into()),
    }
}

/// Parse a comma-separated mount options and split out mountflags and filesystem
/// specific options.
fn parse_mount_options(options: impl AsRef<str>) -> (Option<String>, libc::c_ulong) {
    use either::Either::*;
    debug!("parsing mount options: {}", options.as_ref());
    let (opts, flags) = options
        .as_ref()
        .split(",")
        .map(|o| match o {
            "dirsync"       => Left(libc::MS_DIRSYNC),
            "lazytime"      => Left(1 << 25), // MS_LAZYTIME
            "mand"          => Left(libc::MS_MANDLOCK),
            "noatime"       => Left(libc::MS_NOATIME),
            "nodev"         => Left(libc::MS_NODEV),
            "nodiratime"    => Left(libc::MS_NODIRATIME),
            "noexec"        => Left(libc::MS_NOEXEC),
            "nosuid"        => Left(libc::MS_NOSUID),
            "relatime"      => Left(libc::MS_RELATIME),
            "remount"       => Left(libc::MS_REMOUNT),
            "ro"            => Left(libc::MS_RDONLY),
            "rw"            => Left(0),
            "strictatime"   => Left(libc::MS_STRICTATIME),
            "sync"          => Left(libc::MS_SYNCHRONOUS),
            ""              => Left(0),
            o @ _           => Right(o),
        })
        .fold((Vec::new(), 0), |(mut opts, flags), next| match next {
            Left(f) => (opts, flags | f),
            Right(o) => {
                opts.push(o);
                (opts, flags)
            }
        });

    (
        if opts.len() == 0 {
            None
        } else {
            Some(opts.join(","))
        },
        flags,
    )
}

fn mount(
    device: String,
    target: impl AsRef<std::path::Path>,
    options: impl AsRef<str>,
) -> anyhow::Result<()> {
    let (data, mountflags) = parse_mount_options(options);

    info!(
        "mounting bcachefs filesystem, {}",
        target.as_ref().display()
    );
    mount_inner(device, target, "bcachefs", mountflags, data)
}

fn read_super_silent(path: &std::path::PathBuf) -> anyhow::Result<bch_sb_handle> {
    let mut opts = bcachefs::bch_opts::default();
    opt_set!(opts, noexcl, 1);

    bch_bindgen::sb_io::read_super_silent(&path, opts)
}

fn get_devices_by_uuid(uuid: Uuid) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    debug!("enumerating udev devices");
    let mut udev = udev::Enumerator::new()?;

    udev.match_subsystem("block")?;

    let devs = udev
        .scan_devices()?
        .into_iter()
        .filter_map(|dev| dev.devnode().map(ToOwned::to_owned))
        .map(|dev| (dev.clone(), read_super_silent(&dev)))
        .filter_map(|(dev, sb)| sb.ok().map(|sb| (dev, sb)))
        .filter(|(_, sb)| sb.sb().uuid() == uuid)
        .collect();
    Ok(devs)
}

fn get_uuid_for_dev_node(device: &std::path::PathBuf) ->  anyhow::Result<Uuid> {
    let mut udev = udev::Enumerator::new()?;
    udev.match_subsystem("block")?;

    for dev in udev.scan_devices()?.into_iter() {
        if let Some(devnode) = dev.devnode() {
            if devnode == device {
                let devnode_owned = devnode.to_owned();
                let sb_result = read_super_silent(&devnode_owned);
                if let Ok(sb) = sb_result {
                    return Ok(sb.sb().uuid());
                }
            }
        }
    }
    Err(anyhow::anyhow!("Could not find specified device {}",device.display()))
}

/// Mount a bcachefs filesystem by its UUID.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to passphrase/key file
    ///
    /// Precedes key_location/unlock_policy: if the filesystem can be decrypted
    /// by the specified passphrase file; it is decrypted. (i.e. Regardless
    /// if "fail" is specified for key_location/unlock_policy.)
    #[arg(short = 'f', long)]
    passphrase_file:       Option<PathBuf>,

    /// Password policy to use in case of encrypted filesystem.
    ///
    /// Possible values are:
    /// "fail" - don't ask for password, fail if filesystem is encrypted;
    /// "wait" - wait for password to become available before mounting;
    /// "ask" -  prompt the user for password;
    #[arg(short = 'k', long = "key_location", default_value = "ask", verbatim_doc_comment)]
    unlock_policy:     UnlockPolicy,

    /// Device, or UUID=\<UUID\>
    dev:            String,

    /// Where the filesystem should be mounted. If not set, then the filesystem
    /// won't actually be mounted. But all steps preceeding mounting the
    /// filesystem (e.g. asking for passphrase) will still be performed.
    mountpoint:     Option<PathBuf>,

    /// Mount options
    #[arg(short, default_value = "")]
    options:        String,

    /// Force color on/off. Autodetect tty is used to define default:
    #[arg(short, long, action = clap::ArgAction::Set, default_value_t=stdout().is_terminal())]
    colorize:       bool,

    /// Verbose mode
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose:        u8,
}

fn devs_str_sbs_from_uuid(uuid: String) -> anyhow::Result<(String, Vec<bch_sb_handle>)> {
    debug!("enumerating devices with UUID {}", uuid);

    let devs_sbs = Uuid::parse_str(&uuid)
        .map(|uuid| get_devices_by_uuid(uuid))??;

    let devs_str = devs_sbs
        .iter()
        .map(|(dev, _)| dev.to_str().unwrap())
        .collect::<Vec<_>>()
        .join(":");

    let sbs: Vec<bch_sb_handle> = devs_sbs.iter().map(|(_, sb)| *sb).collect();

    Ok((devs_str, sbs))

}

fn devs_str_sbs_from_device(device: &std::path::PathBuf) -> anyhow::Result<(String, Vec<bch_sb_handle>)> {
    let bcache_fs_uuid = get_uuid_for_dev_node(device)?;

    devs_str_sbs_from_uuid(bcache_fs_uuid.to_string())
}

fn cmd_mount_inner(opt: Cli) -> anyhow::Result<()> {
    let (devices, block_devices_to_mount) = if opt.dev.starts_with("UUID=") {
        let uuid = opt.dev.replacen("UUID=", "", 1);
        devs_str_sbs_from_uuid(uuid)?
    } else if opt.dev.starts_with("OLD_BLKID_UUID=") {
        let uuid = opt.dev.replacen("OLD_BLKID_UUID=", "", 1);
        devs_str_sbs_from_uuid(uuid)?
    } else {
        // If the device string contains ":" we will assume the user knows the entire list.
        // If they supply a single device it could be either the FS only has 1 device or it's
        // only 1 of a number of devices which are part of the FS. This appears to be the case
        // when we get called during fstab mount processing and the fstab specifies a UUID.
        if opt.dev.contains(":") {
            let mut block_devices_to_mount = Vec::new();

            for dev in opt.dev.split(':') {
                let dev = PathBuf::from(dev);
                block_devices_to_mount.push(read_super_silent(&dev)?);
            }

            (opt.dev, block_devices_to_mount)
        } else {
            devs_str_sbs_from_device(&PathBuf::from(opt.dev))?
        }
    };

    if block_devices_to_mount.len() == 0 {
        Err(anyhow::anyhow!("No device found from specified parameters"))?;
    }
    // Check if the filesystem's master key is encrypted
    if unsafe { bcachefs::bch2_sb_is_encrypted_and_locked(block_devices_to_mount[0].sb) } {
        // First by password_file, if available
        let fallback_to_unlock_policy = if let Some(passphrase_file) = &opt.passphrase_file {
            match key::read_from_passphrase_file(&block_devices_to_mount[0], passphrase_file.as_path()) {
                Ok(()) => {
                    // Decryption succeeded
                    false
                }
                Err(err) => {
                    // Decryption failed, fall back to unlock_policy
                    error!("Failed to decrypt using passphrase_file: {}", err);
                    true
                }
            }
        } else {
            // No passphrase_file specified, fall back to unlock_policy
            true
        };
        // If decryption by key_file was unsuccesful, prompt for passphrase (or follow key_policy)
        if fallback_to_unlock_policy {
            key::apply_key_unlocking_policy(&block_devices_to_mount[0], opt.unlock_policy)?;
        };
    }

    if let Some(mountpoint) = opt.mountpoint {
        info!(
            "mounting with params: device: {}, target: {}, options: {}",
            devices,
            mountpoint.to_string_lossy(),
            &opt.options
        );

        mount(devices, mountpoint, &opt.options)?;
    } else {
        info!(
            "would mount with params: device: {}, options: {}",
            devices,
            &opt.options
        );
    }

    Ok(())
}

pub fn cmd_mount(mut argv: Vec<String>, symlink_cmd: Option<&str>) -> i32 {
    // If the bcachefs tool is being called as "bcachefs mount dev ..." (as opposed to via a
    // symlink like "/usr/sbin/mount.bcachefs dev ...", then we need to pop the 0th argument
    // ("bcachefs") since the CLI parser here expects the device at position 1.
    if symlink_cmd.is_none() {
        argv.remove(0);
    }

    let opt = Cli::parse_from(argv);

    // @TODO : more granular log levels via mount option
    log::set_max_level(match opt.verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Trace,
        2_u8..=u8::MAX => todo!(),
    });

    colored::control::set_override(opt.colorize);
    if let Err(e) = cmd_mount_inner(opt) {
        error!("Fatal error: {}", e);
        1
    } else {
        info!("Successfully mounted");
        0
    }
}
