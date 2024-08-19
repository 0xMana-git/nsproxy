//! Misc low-level code

use std::{
    collections::{HashMap, HashSet},
    env::var,
    ffi::{CStr, CString},
    fs::{
        create_dir, create_dir_all, read_dir, remove_dir_all, remove_file, File, FileType,
        OpenOptions,
    },
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::PathBuf,
    process::exit,
    sync::mpsc::sync_channel,
};

use anyhow::{bail, ensure};
use daggy::NodeIndex;
use id_alloc::NetRange;
use ipnetwork::{IpNetwork, Ipv4Network, Ipv6Network};
use libc::{pid_t, stat, syscall, uid_t};
use netlink_ops::{
    netlink::{nl_ctx, NLDriver, NLHandle, VPairKey, VethConn},
    state::{Existence, ExpCollection},
};
use tracing::info;
use uzers::os::unix::UserExt;
use zbus::Connection;

use super::*;
use crate::{
    data::*,
    paths::{Binds, PathState, Paths},
};

use nix::{
    mount::{mount, umount, umount2, MntFlags, MsFlags},
    sched::{setns, unshare, CloneFlags},
    sys::{signal::kill, stat::fstat, wait::waitpid},
    unistd::{
        fork, getresuid, getuid, initgroups, seteuid, setgroups, setresgid, setresuid, setuid,
        ForkResult, Gid, Pid, Uid,
    },
};

use std::{mem::size_of, os::fd::RawFd};

use anyhow::Result;
use nix::{
    errno::Errno,
    libc::{c_int, SYS_mount_setattr, AT_FDCWD, MS_PRIVATE},
    NixPath,
};

fn mount_single(mut pid: PidPath, binds: &Binds, really: bool, name: &str) -> Result<ExactNS> {
    // let name = K::NAME;
    // pid = pid.to_n();
    let path: PathBuf = ["/proc", pid.to_str().as_ref(), "ns", name]
        .iter()
        .collect();
    let stat = nix::sys::stat::stat(&path)?;
    let bindat = binds.ns(name);

    if really {
        let _ = File::create(&bindat)?;
        mount(
            Some(&path),
            &bindat,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )?;
    }

    Ok(ExactNS {
        source: NSSource::Path(bindat),
        unique: stat.into(),
    })
}

#[public]
impl<K: NSTrait> NSSlot<ExactNS, K> {
    fn mount(mut pid: PidPath, binds: &Binds, really: bool) -> Result<Self> {
        let name = K::NAME;
        let e = mount_single(pid, binds, really, name)?;
        Ok(NSSlot::Provided(e, Default::default()))
    }
    fn source(mut self, replace: NSSource) -> Self {
        match self {
            NSSlot::Absent => (),
            NSSlot::Provided(ref mut n, _) => n.source = replace,
        };
        self
    }
    fn absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

#[public]
impl NSGroup<ExactNS> {
    /// Returns the mounted procNSes from /proc/mountinfo
    /// Remember to enter userns (usually) or mounts wont be visible
    fn mounted(paths: &PathState, id: NodeI, root: bool) -> Result<HashMap<Ix, NSGroup<ExactNS>>> {
        let mut map = HashMap::new();
        let binds = paths.mount(id, root)?.0;
        let it = proc_mounts::MountIter::new()?;
        let maps = nstypes();
        for m in it {
            let m = m?;
            let path = m.source;
            if m.fstype == "nsfs" && path.starts_with(&binds) {
                let ns = path.file_name().unwrap().to_string_lossy();
                let id = path.parent().unwrap().file_name().unwrap();
                let id: Ix = id.to_string_lossy().parse()?;
                let p = maps[ns.as_ref()];
                if !map.contains_key(&id) {
                    let g = NSGroup::default();
                    map.insert(id, g);
                }
                let mut g = map.get_mut(&id).unwrap();
                p(&mut g, ExactNS::from_source(path)?);
            }
        }
        Ok(map)
    }
    /// Umount all namespaces and remove the dir
    fn umount(id: NodeI, paths: &PathState, root: bool) -> Result<()> {
        let binds = paths.mount(id, root)?.0;
        for e in std::fs::read_dir(&binds)? {
            let e = e?;
            let p = e.path();
            info!("umount {:?}", &p);
            let rx = umount2(&p, MntFlags::MNT_DETACH | MntFlags::MNT_FORCE);
            match rx {
                Err(no) => {
                    match no {
                        Errno::EINVAL => {
                            // its not mounted
                            // but still weird because we tend to hold the contract
                            // that a file exists ==> it is mounted
                            log::warn!("EINVAL umount {:?}", &p);
                        }
                        k => return Err(k.into()),
                    }
                }
                _ => (),
            }
        }
        Ok(())
    }
    fn rmall(paths: &PathState, root: bool) -> Result<()> {
        for dir in std::fs::read_dir(paths.binds()?)? {
            let dir = dir?;
            if dir.file_type()?.is_dir() {
                let pa: Result<u32, _> = dir.file_name().to_string_lossy().parse();
                if let Ok(id) = pa {
                    Self::umount(id.into(), paths, root)?;
                }
            }
        }
        Ok(())
    }
    /// Identify the key as in the map
    fn key_ident(pid: PidPath) -> Result<ExactNS> {
        ExactNS::from_source((pid, "net"))
    }
}

// cargo test mount_self -- --nocapture
// use no capture to avoid rust hiding stdout
// test this with ./unshare.sh
#[test]
fn mount_self() -> Result<()> {
    let path = PathState::default(1000)?;
    let path: Paths = path.into();
    dbg!(path.clone());
    let mounted = mount_ns_by_pid(PidPath::Selfproc, &path, 3.into(), true, false)?;
    dbg!(mounted);

    Ok(())
}

// On the assumption that default NSes do not change across boots;
// Otherwise, we will have one new 'default node' each reboot.
impl ObjectNode {
    pub fn this() {}
}

impl NSEnter for NSSource {
    fn enter(&self, f: CloneFlags) -> Result<()> {
        match self {
            Self::Path(p) => {
                let fd = File::open(p)?;
                setns(fd, f)?;
            }
            Self::Pid(p) => {
                let fd = unsafe { pidfd::PidFd::open(*p, 0) }?;
                setns(fd, f)?;
            }
            Self::Unavail(_) => unreachable!(),
        }
        Ok(())
    }
}

impl NSEnter for ExactNS {
    fn enter(&self, f: CloneFlags) -> Result<()> {
        self.source.enter(f)
    }
}

pub trait NSEnter {
    fn enter(&self, f: CloneFlags) -> Result<()>;
}

pub struct UserNS<'p>(pub &'p PathState);

#[test]
fn sockpairfork() -> Result<()> {
    let (mut sa, mut sb) = UnixStream::pair()?;

    match unsafe { fork() }? {
        ForkResult::Child => {
            sa.write_all(&[2])?;
        }
        ForkResult::Parent { child } => {
            let mut k: [u8; 1] = [0];
            sb.read_exact(&mut k)?;
            dbg!(k);
        }
    }

    Ok(())
}

#[public]
impl<'p> UserNS<'p> {
    fn exist(&self) -> Result<bool> {
        let mut f = OpenOptions::new().read(true).open("/proc/self/mountinfo")?;
        let read = BufReader::new(&mut f);
        let (u, p) = self.paths()?;
        let (mut ux, mut px) = Default::default();
        // They have to be UTF8 ?
        let (u, p) = (u.to_str().unwrap(), p.to_str().unwrap());
        for line in read.lines() {
            let line = line?;
            ux |= line.contains(u);
            px |= line.contains(p);
            if ux && px {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// A process with euid being owner may enter the user NS without the cap
    fn init(&self, owner: uid_t) -> Result<()> {
        let private = self.0.private(false)?;
        // create_dir_all(&private)?; // doesnt error when dir exists
        mount(
            // CAP_SYS_ADMIN
            Some(&private),
            &private,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )?;

        let mut att = MountAttr::default();
        att.propagation = MS_PRIVATE;
        unsafe { mount_setattr(AT_FDCWD, &private, 0, &att as *const _) }?;
        let (user, mnt) = self.paths()?;
        let _ = File::create(&mnt)?;
        let _ = File::create(&user)?;

        let (mut sa, mut sb) = UnixStream::pair()?;

        match unsafe { fork() }? {
            ForkResult::Child => {
                let u = Uid::from_raw(owner);
                setresuid(u, u, u)?;
                // After setting EUID, flag dumpable is changed, and perms in /proc get changed too
                capctl::prctl::set_dumpable(true)?;
                log::info!("unshare, owner uid is {u}");
                unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS)?;
                sa.write_all(&[0])?; // unshared

                let mut k: [u8; 1] = [0];
                sa.read_exact(&mut k)?;
                exit(0);
            }
            ForkResult::Parent { child } => {
                let puser: PathBuf = ["/proc", &child.as_raw().to_string(), "ns", "user"]
                    .iter()
                    .collect();
                let pmnt: PathBuf = ["/proc", &child.as_raw().to_string(), "ns", "mnt"]
                    .iter()
                    .collect();
                let mut k: [u8; 1] = [0];

                sb.read_exact(&mut k)?; // unshared
                let mut f = OpenOptions::new()
                    .write(true)
                    .open(format!("/proc/{child}/uid_map"))?;
                // f.write_all(format!("{u} {u} 1").as_bytes())?; // map uid (in user ns) to uid (outside) for range 1
                f.write_all(format!("0 0 4294967295").as_bytes())?;
                let mut f = OpenOptions::new()
                    .write(true)
                    .open(format!("/proc/{child}/gid_map"))?;
                f.write_all(format!("0 0 4294967295").as_bytes())?;

                mount(
                    Some(&puser),
                    &self.0.user()?,
                    None::<&str>,
                    MsFlags::MS_BIND,
                    None::<&str>,
                )?;
                mount(
                    Some(&pmnt),
                    &mnt,
                    None::<&str>,
                    MsFlags::MS_BIND,
                    None::<&str>,
                )?;
                sb.write_all(&[0])?;
                log::info!("UserNS inited")
            }
        }

        Ok(())
    }
    fn deinit(&self) -> Result<()> {
        let (user, mnt) = self.paths()?;
        let private = mnt.parent().unwrap();
        if private.exists() {
            if let Err(k) = umount(private) {
                if k == Errno::EINVAL {
                    // maybe no mount. ok
                } else {
                    // try umounting mnt, which is also ok
                    if let Err(x) = umount(&mnt) {
                        if x == Errno::EINVAL {
                            // maybe no mount. ok
                        } else {
                            bail!(x);
                        }
                    } else {
                        remove_file(&mnt)?;
                    }
                }
            } else {
                remove_dir_all(&private)?;
            }
        }
        if user.exists() {
            if let Err(k) = umount(&user) {
                if k == Errno::EINVAL {
                    // maybe no mount
                } else {
                    bail!(k);
                }
            }
            remove_file(&user)?;
        }
        log::info!("UserNS deinited");
        Ok(())
    }
    fn paths(&self) -> Result<(PathBuf, PathBuf)> {
        Ok((self.0.user()?, self.0.private(false)?.join("mnt")))
    }
    /// Generate a [ProcNS]
    fn procns(&self) -> Result<NSGroup<ExactNS>> {
        let (user, mnt) = self.paths()?;
        Ok(NSGroup {
            user: NSSlot::Provided(ExactNS::from_source(user)?, Default::default()),
            mnt: NSSlot::Provided(ExactNS::from_source(mnt)?, Default::default()),
            ..Default::default()
        })
    }
}

#[test]
fn show_userns_path() -> Result<()> {
    let path = PathState::default(1000)?;
    let usern = UserNS(&path);
    dbg!(usern.paths());

    Ok(())
}

#[test]
fn test_userns() -> Result<()> {
    let path = PathState::default(1000)?;
    let usern = UserNS(&path);
    dbg!(usern.paths());
    usern.init(1000)?;

    Ok(())
}

#[test]
fn userns_deinit() -> Result<()> {
    let path = PathState::default(1000)?;
    let usern = UserNS(&path);
    dbg!(usern.paths());
    usern.deinit()?;

    Ok(())
}

#[derive(Default)]
#[repr(C, align(8))]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    unserns_fd: u64,
}

unsafe fn mount_setattr(
    dirfd: RawFd,
    path: &impl NixPath,
    flags: c_int,
    attr: *const MountAttr,
) -> Result<(), Errno> {
    let k = path.with_nix_path(|pa| unsafe {
        syscall(
            SYS_mount_setattr,
            dirfd,
            pa.as_ptr(),
            flags,
            attr,
            size_of::<MountAttr>(),
        )
    })?;

    Errno::result(k).map(drop)
}

pub fn check_capsys() -> Result<()> {
    let caps = capctl::CapState::get_current().unwrap();
    if !caps.effective.has(capctl::Cap::SYS_ADMIN) {
        bail!("requires CAP_SYS_ADMIN. Use sproxy");
    }

    Ok(())
}

pub fn your_shell(specify: Option<String>, mut uid: Option<u32>) -> Result<Option<String>> {
    Ok(match specify {
        Some(k) => Some(k),
        None => {
            if uid.is_none() {
                uid = Some(what_uid(uid, false)?);
            }
            let user = uzers::get_user_by_uid(uid.unwrap());
            if let Some(user) = user {
                Some(user.shell().to_string_lossy().into_owned())
            } else {
                None
            }
        }
    })
}

pub fn enable_ping_all() -> Result<()> {
    let mut f = File::options()
        .write(true)
        .open("/proc/sys/net/ipv4/ping_group_range")?;
    f.write_all(b"0 2147483647")?;
    Ok(())
}

pub fn enable_ping_gid(gid: Gid) -> Result<()> {
    let mut f = File::options()
        .write(true)
        .open("/proc/sys/net/ipv4/ping_group_range")?;
    f.write_all(format!("{gid} {gid}").as_bytes())?;
    Ok(())
}

pub fn cmd_uid(uid: Option<u32>, allow_root: bool, change_uid: bool) -> Result<()> {
    let u = Uid::from_raw(what_uid(uid, allow_root)?);
    let user = uzers::get_user_by_uid(u.as_raw()).unwrap();
    let g = user.primary_group_id().into();
    info!("set initgroups");
    // This line failed for a flatpak ns
    let _ = initgroups(&CString::new(user.name().as_bytes())?, g);
    setresgid(g, g, g)?;
    if change_uid {
        setresuid(u, u, u)?;
    }
    Ok(())
}

/// The program keeps a special uid in mind, called the non-root uid.
pub fn what_uid(uid: Option<u32>, allow_root: bool) -> Result<u32> {
    if let Some(u) = uid {
        Ok(u)
    } else {
        if let Ok(id) = var(UID_HINT_VAR) {
            Ok(id.parse()?)
        } else if let Ok(id) = var("SUDO_UID") {
            Ok(id.parse()?)
        } else {
            let res = getresuid()?;
            if !res.real.is_root() {
                Ok(res.real.as_raw())
            } else if let Ok(kde) = var("KDE_SESSION_UID") {
                Ok(kde.parse()?)
            } else {
                if allow_root {
                    Ok(0)
                } else {
                    bail!("unable to find a non-root uid")
                }
            }
        }
    }
}

/// Unshare the process into a separate userns, rootless
/// Map one single uid, and gid.
pub fn unshare_user_standalone(uid: u32, gid: u32, mnt: bool) -> Result<NSGroup<ExactNS>> {
    log::warn!("Unsharing into a new UserNS. This method currently has limitations.");
    let flg = if mnt {
        CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS
    } else {
        CloneFlags::CLONE_NEWUSER
    };

    unshare(flg)?;
    let mut f = OpenOptions::new()
        .write(true)
        .open(format!("/proc/self/uid_map"))?;
    f.write_all(format!("{uid} {uid} 1").as_bytes())?;
    let mut f = OpenOptions::new()
        .write(true)
        .open(format!("/proc/self/setgroups"))?;
    f.write_all(b"deny")?;
    let mut f = OpenOptions::new()
        .write(true)
        .open(format!("/proc/self/gid_map"))?;
    f.write_all(format!("{gid} {gid} 1",).as_bytes())?;
    Ok(NSGroup {
        // we have unshared user ns
        user: NSSlot::from_source(PidPath::Selfproc)?,
        ..Default::default()
    })
}

pub async fn systemd_connection(root: bool) -> Result<Connection> {
    let z = if root {
        log::info!("Connecting systemd as root");
        zbus::Connection::system().await?
    } else {
        zbus::Connection::session().await?
    };
    Ok(z)
}

pub async fn connect_ns_veth(
    nl_ch: NLHandle,
    nl: NLHandle,
    mut veth_key: Option<VPairKey>,
) -> Result<VethConn> {
    let mut nl_ch = NLDriver::new(nl_ch);
    let mut nl = NLDriver::new(nl);
    log::info!("Fetch netlink");
    nl.fill().await?;
    log::info!("Fetch netlink (child process)");
    nl_ch.fill().await?;
    log::info!("Netlink fetched");
    let mut addrset: HashSet<IpNetwork> = HashSet::default(); // find unused subnet
    {
        nl_ctx!(link, conn, nl_ch);
        conn.set_up(link.map.get_mut(&"lo".parse()?).unwrap().exist_mut()?)
            .await?;
    }
    {
        nl_ctx!(link, conn, nl);
        for (k, ex) in link.map {
            if let Existence::Exist(li) = ex {
                match &li.addrs {
                    ExpCollection::Filled(addr) => {
                        addrset.extend(addr.keys().into_iter());
                    }
                    _ => (),
                }
            }
        }
    }
    let (p4, p6) = (30, 126);
    let (mut v4, mut v6, h4, h6) = id_alloc::from_ipnet(&addrset, p4, p6);
    let dom4: Ipv4Network = "100.67.0.0/16".parse()?;
    let r4 = dom4.range(h4);
    let dom6: Ipv6Network = "fe80:2e::/24".parse()?;
    let r6 = dom6.range(h6);
    let net4: Ipv4Network = v4.alloc_or(&r4)?.try_into()?;
    let net6: Ipv6Network = v6.alloc_or(&r6)?.try_into()?;
    let n6: [_; 2] = net6.iter().next_chunk().unwrap();
    let n6net: [_; 2] = n6.try_map(|n| Ipv6Network::new(n, p6))?.map(|n| n.into());
    let mask = (!0 >> dom4.prefix()) & net4.mask().to_bits();
    let num = (net4.ip().to_bits() & mask) >> h4;
    if veth_key.is_none() {
        veth_key = Some(format!("nsproxy{}", num).try_into()?);
    }
    let vc = VethConn {
        subnet_veth: net4.into(),
        subnet6_veth: net6.into(),
        ip_va: Ipv4Network::new(net4.nth(0).unwrap(), p4)?.into(),
        ip_vb: Ipv4Network::new(net4.nth(1).unwrap(), p4)?.into(),
        ip6_va: n6net[0],
        ip6_vb: n6net[1],
        key: veth_key.unwrap(),
    };
    // let edge = graphs.data.add_edge(src, out, None);
    vc.apply(&mut nl_ch, &mut nl).await?;
    let mut nl_ch = NLDriver::new(nl_ch.conn);
    let mut nl = NLDriver::new(nl.conn);
    nl_ch.fill().await?;
    nl.fill().await?;
    vc.apply_addr_up(&mut nl_ch, &mut nl).await?;
    Ok(vc)
}
