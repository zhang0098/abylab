//! Linux child-process confinement used by the local Bash tool.
//!
//! This is the crate's only unsafe module: `pre_exec` is necessary to install
//! Landlock and seccomp in the forked child without restricting the SDK host.

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus,
};
use std::{io, os::unix::process::CommandExt, path::Path, process::Command, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfinedMode {
    ReadOnly,
    WorkspaceWrite,
}

pub(crate) fn confine(
    command: &mut Command,
    mode: ConfinedMode,
    workspace: &Path,
    writable_temp_roots: &[std::path::PathBuf],
) -> io::Result<()> {
    // ABI v3 is the first version that governs truncation in addition to the
    // ABI v1/v2 write, create, remove, rename and link operations. Requiring
    // it avoids presenting a partially enforced write boundary as read-only.
    let abi = ABI::V3;
    let all = AccessFs::from_all(abi);
    let read = AccessFs::from_read(abi);
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(all)
        .map_err(other)?
        .create()
        .map_err(other)?
        .add_rule(PathBeneath::new(PathFd::new("/").map_err(other)?, read))
        .map_err(other)?;

    // The shell and common Unix programs need a writable sink even in
    // read-only mode. This matches the Harness policy and does not expose a
    // persistent file.
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            PathFd::new("/dev/null").map_err(other)?,
            AccessFs::from_file(abi),
        ))
        .map_err(other)?;

    if mode == ConfinedMode::WorkspaceWrite {
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                PathFd::new(workspace).map_err(other)?,
                all,
            ))
            .map_err(other)?;
        for root in writable_temp_roots {
            ruleset = ruleset
                .add_rule(PathBeneath::new(PathFd::new(root).map_err(other)?, all))
                .map_err(other)?;
        }
    }

    let ruleset = Arc::new(ruleset);
    let metadata_filter = metadata_filter()?;
    // SAFETY: Both policies are compiled before fork. The closure only installs
    // them using dup/prctl/Landlock/seccomp syscalls, without allocation or host
    // locks. Both policies survive exec and are inherited by descendants.
    unsafe {
        command.pre_exec(move || {
            let status = ruleset
                .try_clone()?
                .restrict_self()
                .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
            if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            seccompiler::apply_filter(&metadata_filter)
                .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
            Ok(())
        });
    }
    Ok(())
}

/// Landlock does not mediate metadata mutations. Seccomp cannot resolve paths,
/// so these operations are denied globally in both restricted modes. io_uring
/// and ioctl are also blocked: they expose alternate metadata/device effects.
#[cfg(all(
    target_pointer_width = "64",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
fn metadata_filter() -> io::Result<seccompiler::BpfProgram> {
    use seccompiler::{SeccompAction, SeccompFilter, sock_filter};
    // Linux UAPI assigns 452 on all three supported native architectures;
    // libc does not yet expose SYS_fchmodat2 for aarch64 and riscv64.
    const FCHMODAT2: libc::c_long = 452;
    let denied = vec![
        libc::SYS_fchmod,
        libc::SYS_fchmodat,
        FCHMODAT2,
        libc::SYS_fchown,
        libc::SYS_fchownat,
        libc::SYS_utimensat,
        libc::SYS_setxattr,
        libc::SYS_lsetxattr,
        libc::SYS_fsetxattr,
        libc::SYS_removexattr,
        libc::SYS_lremovexattr,
        libc::SYS_fremovexattr,
        libc::SYS_ioctl,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_open_tree,
        libc::SYS_move_mount,
        libc::SYS_mount_setattr,
        libc::SYS_quotactl,
        libc::SYS_quotactl_fd,
    ];
    #[cfg(target_arch = "x86_64")]
    let denied = denied
        .into_iter()
        .chain([
            libc::SYS_chmod,
            libc::SYS_chown,
            libc::SYS_lchown,
            libc::SYS_utime,
            libc::SYS_utimes,
            libc::SYS_futimesat,
        ])
        .collect::<Vec<_>>();
    let compiled: seccompiler::BpfProgram = SeccompFilter::new(
        denied.into_iter().map(|number| (number, vec![])).collect(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH.try_into().map_err(other)?,
    )
    .map_err(other)?
    .try_into()
    .map_err(other)?;
    // Reject newer, unaudited syscalls (including *xattrat/file_setattr) and
    // x86's x32 ABI, which shares its audit architecture but not syscall numbers.
    // ENOSYS lets libc fall back to existing, filtered syscall implementations.
    let mut filter = vec![
        sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: 0,
        },
        sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: FCHMODAT2 as u32 + 1,
        },
        sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
        },
    ];
    filter.extend(compiled);
    Ok(filter)
}

#[cfg(not(all(
    target_pointer_width = "64",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
)))]
fn metadata_filter() -> io::Result<seccompiler::BpfProgram> {
    Err(io::Error::other(
        "no metadata syscall policy for this Linux architecture",
    ))
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_filter_child_probe() {
        if std::env::var_os("ABYCORE_METADATA_PROBE").is_none() {
            return;
        }
        // Null pointers keep the fixture side-effect-free even if the filter is
        // absent; EPERM distinguishes policy denial from EFAULT/EBADF/EINVAL.
        for number in [
            libc::SYS_fchmod,
            452, // fchmodat2, including on architectures missing libc's constant.
            libc::SYS_fchown,
            libc::SYS_utimensat,
            libc::SYS_setxattr,
            libc::SYS_fsetxattr,
            libc::SYS_removexattr,
            libc::SYS_ioctl,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            // SAFETY: All six syscall arguments are supplied, with no valid
            // pointers or descriptors; these calls cannot mutate host data.
            let result = unsafe { libc::syscall(number, -1i64, 0i64, 0i64, 0i64, 0i64, 0i64) };
            assert_eq!(result, -1);
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM),
                "syscall {number}"
            );
        }
        for number in [453, 463, 466, 0x4000005a] {
            // SAFETY: Same invalid arguments; these unaudited syscall numbers
            // must be rejected before the kernel can interpret any pointers.
            let result = unsafe {
                libc::syscall(number as libc::c_long, -1i64, 0i64, 0i64, 0i64, 0i64, 0i64)
            };
            assert_eq!(result, -1);
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOSYS)
            );
        }
    }

    #[test]
    fn syscall_filter_is_inherited_without_restricting_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "subprocess_sandbox::tests::metadata_filter_child_probe",
                "--nocapture",
            ])
            .env("ABYCORE_METADATA_PROBE", "1");
        confine(&mut command, ConfinedMode::ReadOnly, dir.path(), &[]).unwrap();
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // A filter accidentally installed in this test thread would deny this.
        let path = dir.path().join("host");
        std::fs::write(&path, "host remains writable").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
