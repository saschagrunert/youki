use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use libseccomp::ScmpAction;
use libseccomp::ScmpArch;
use libseccomp::ScmpArgCompare;
use libseccomp::ScmpCompareOp;
use libseccomp::ScmpFilterContext;
use libseccomp::ScmpSyscall;
use oci_spec::runtime::Arch;
use oci_spec::runtime::LinuxSeccomp;
use oci_spec::runtime::LinuxSeccompAction;
use oci_spec::runtime::LinuxSeccompOperator;
use std::os::unix::io;

fn translate_arch(arch: Arch) -> ScmpArch {
    match arch {
        Arch::ScmpArchNative => ScmpArch::Native,
        Arch::ScmpArchX86 => ScmpArch::X86,
        Arch::ScmpArchX86_64 => ScmpArch::X8664,
        Arch::ScmpArchX32 => ScmpArch::X32,
        Arch::ScmpArchArm => ScmpArch::Arm,
        Arch::ScmpArchAarch64 => ScmpArch::Aarch64,
        Arch::ScmpArchMips => ScmpArch::Mips,
        Arch::ScmpArchMips64 => ScmpArch::Mips64,
        Arch::ScmpArchMips64n32 => ScmpArch::Mips64N32,
        Arch::ScmpArchMipsel => ScmpArch::Mipsel,
        Arch::ScmpArchMipsel64 => ScmpArch::Mipsel64,
        Arch::ScmpArchMipsel64n32 => ScmpArch::Mipsel64N32,
        Arch::ScmpArchPpc => ScmpArch::Ppc,
        Arch::ScmpArchPpc64 => ScmpArch::Ppc64,
        Arch::ScmpArchPpc64le => ScmpArch::Ppc64Le,
        Arch::ScmpArchS390 => ScmpArch::S390,
        Arch::ScmpArchS390x => ScmpArch::S390X,
    }
}

fn translate_action(action: LinuxSeccompAction, errno: Option<u32>) -> Result<ScmpAction> {
    let errno = errno.map(|e| e as i32).unwrap_or(libc::EPERM);
    let action = match action {
        LinuxSeccompAction::ScmpActKill => ScmpAction::KillThread,
        LinuxSeccompAction::ScmpActTrap => ScmpAction::Trap,
        LinuxSeccompAction::ScmpActErrno => ScmpAction::Errno(errno),
        LinuxSeccompAction::ScmpActTrace => ScmpAction::Trace(errno.try_into()?),
        LinuxSeccompAction::ScmpActAllow => ScmpAction::Allow,
        LinuxSeccompAction::ScmpActKillProcess => ScmpAction::KillProcess,
        LinuxSeccompAction::ScmpActNotify => ScmpAction::Notify,
        LinuxSeccompAction::ScmpActLog => ScmpAction::Log,
    };

    Ok(action)
}

fn translate_op(op: LinuxSeccompOperator, datum_b: Option<u64>) -> ScmpCompareOp {
    match op {
        LinuxSeccompOperator::ScmpCmpNe => ScmpCompareOp::NotEqual,
        LinuxSeccompOperator::ScmpCmpLt => ScmpCompareOp::Less,
        LinuxSeccompOperator::ScmpCmpLe => ScmpCompareOp::LessOrEqual,
        LinuxSeccompOperator::ScmpCmpEq => ScmpCompareOp::Equal,
        LinuxSeccompOperator::ScmpCmpGe => ScmpCompareOp::GreaterEqual,
        LinuxSeccompOperator::ScmpCmpGt => ScmpCompareOp::Greater,
        LinuxSeccompOperator::ScmpCmpMaskedEq => ScmpCompareOp::MaskedEqual(datum_b.unwrap_or(0)),
    }
}

fn check_seccomp(seccomp: &LinuxSeccomp) -> Result<()> {
    // We don't support notify as default action. After the seccomp filter is
    // created with notify, the container process will have to communicate the
    // returned fd to another process. Therefore, we need the write syscall or
    // otherwise, the write syscall will be block by the seccomp filter causing
    // the container process to hang. `runc` also disallow notify as default
    // action.
    // Note: read and close syscall are also used, because if we can
    // successfully write fd to another process, the other process can choose to
    // handle read/close syscall and allow read and close to proceed as
    // expected.
    if seccomp.default_action() == LinuxSeccompAction::ScmpActNotify {
        bail!("SCMP_ACT_NOTIFY cannot be used as default action");
    }

    if let Some(syscalls) = seccomp.syscalls() {
        for syscall in syscalls {
            if syscall.action() == LinuxSeccompAction::ScmpActNotify {
                for name in syscall.names() {
                    if name == "write" {
                        bail!("SCMP_ACT_NOTIFY cannot be used for the write syscall");
                    }
                }
            }
        }
    }

    Ok(())
}

/// All filter return actions except SECCOMP_RET_ALLOW should be logged. An administrator may
/// override this filter flag by preventing specific actions from being logged via the
/// /proc/sys/kernel/seccomp/actions_logged file. (since Linux 4.14)
const SECCOMP_FILTER_FLAG_LOG: &str = "SECCOMP_FILTER_FLAG_LOG";

/// When adding a new filter, synchronize all other threads of the calling process to the same
/// seccomp filter tree. A "filter tree" is the ordered list of filters attached to a thread.
/// (Attaching identical filters in separate seccomp() calls results in different filters from this
/// perspective.)
///
/// If any thread cannot synchronize to the same filter tree, the call will not attach the new
/// seccomp filter, and will fail, returning the first thread ID found that cannot synchronize.
/// Synchronization will fail if another thread in the same process is in SECCOMP_MODE_STRICT or if
/// it has attached new seccomp filters to itself, diverging from the calling thread's filter tree.
const SECCOMP_FILTER_FLAG_TSYNC: &str = "SECCOMP_FILTER_FLAG_TSYNC";

/// Disable Speculative Store Bypass mitigation. (since Linux 4.17)
const SECCOMP_FILTER_FLAG_SPEC_ALLOW: &str = "SECCOMP_FILTER_FLAG_SPEC_ALLOW";

pub fn initialize_seccomp(seccomp: &LinuxSeccomp) -> Result<Option<io::RawFd>> {
    check_seccomp(seccomp)?;

    let default_action = translate_action(seccomp.default_action(), seccomp.default_errno_ret())?;
    let mut ctx = ScmpFilterContext::new_filter(translate_action(
        seccomp.default_action(),
        seccomp.default_errno_ret(),
    )?)?;

    if let Some(flags) = seccomp.flags() {
        for flag in flags {
            match flag.as_ref() {
                SECCOMP_FILTER_FLAG_LOG => ctx.set_ctl_log(true)?,
                SECCOMP_FILTER_FLAG_TSYNC => ctx.set_ctl_tsync(true)?,
                SECCOMP_FILTER_FLAG_SPEC_ALLOW => ctx.set_ctl_ssb(true)?,
                f => bail!("seccomp flag {} is not supported", f),
            }
        }
    }

    if let Some(architectures) = seccomp.architectures() {
        for &arch in architectures {
            ctx.add_arch(translate_arch(arch))
                .context("failed to add arch to seccomp")?;
        }
    }

    // The SCMP_FLTATR_CTL_NNP controls if the seccomp load function will set
    // the new privilege bit automatically in prctl. Normally this is a good
    // thing, but for us we need better control. Based on the spec, if OCI
    // runtime spec doesn't set the no new privileges in Process, we should not
    // set it here.  If the seccomp load operation fails without enough
    // privilege, so be it. To prevent this automatic behavior, we unset the
    // value here.
    ctx.set_ctl_nnp(false)?;

    if let Some(syscalls) = seccomp.syscalls() {
        for syscall in syscalls {
            let action = translate_action(syscall.action(), syscall.errno_ret())?;
            if action == default_action {
                // When the action is the same as the default action, the rule is redundant. We can
                // skip this here to avoid failing when we add the rules.
                log::warn!(
                    "Detect a seccomp action that is the same as the default action: {:?}",
                    syscall
                );
                continue;
            }

            for name in syscall.names() {
                let sc = match ScmpSyscall::from_name(name) {
                    Ok(x) => x,
                    Err(_) => {
                        // If we failed to resolve the syscall by name, likely the kernel
                        // doeesn't support this syscall. So it is safe to skip...
                        log::warn!(
                            "failed to resolve syscall, likely kernel doesn't support this. {:?}",
                            name
                        );
                        continue;
                    }
                };
                // Not clear why but if there are multiple arg attached to one
                // syscall rule, we have to add them seperatly. add_rule will
                // return EINVAL. runc does the same but doesn't explain why.
                match syscall.args() {
                    Some(args) => {
                        for arg in args {
                            let cmp = ScmpArgCompare::new(
                                arg.index() as u32,
                                translate_op(arg.op(), arg.value_two()),
                                arg.value(),
                            );
                            ctx.add_rule_conditional(action, sc, &[cmp])
                                .with_context(|| {
                                    format!(
                                        "failed to add seccomp action: {:?}. Cmp: {:?} Syscall: {name}",
                                        &action, cmp,
                                    )
                                })?;
                        }
                    }
                    None => {
                        ctx.add_rule(action, sc).with_context(|| {
                            format!("failed to add seccomp rule: {:?}. Syscall: {name}", &sc)
                        })?;
                    }
                }
            }
        }
    }

    // In order to use the SECCOMP_SET_MODE_FILTER operation, either the calling
    // thread must have the CAP_SYS_ADMIN capability in its user namespace, or
    // the thread must already have the no_new_privs bit set.
    // Ref: https://man7.org/linux/man-pages/man2/seccomp.2.html
    ctx.load().context("failed to load seccomp context")?;

    let fd = if is_notify(seccomp) {
        Some(
            ctx.get_notify_fd()
                .context("failed to get seccomp notify fd")?,
        )
    } else {
        None
    };

    Ok(fd)
}

pub fn is_notify(seccomp: &LinuxSeccomp) -> bool {
    seccomp
        .syscalls()
        .iter()
        .flatten()
        .any(|syscall| syscall.action() == LinuxSeccompAction::ScmpActNotify)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::test_utils;
    use anyhow::Result;
    use oci_spec::runtime::Arch;
    use oci_spec::runtime::{LinuxSeccompBuilder, LinuxSyscallBuilder};
    use serial_test::serial;
    use std::path;

    #[test]
    #[serial]
    fn test_basic() -> Result<()> {
        // Note: seccomp profile is really hard to write unit test for. First,
        // we can't really test default error or kill action, since rust test
        // actually relies on certain syscalls. Second, some of the syscall will
        // not return errorno. These syscalls will just send an abort signal or
        // even just segfaults.  Here we choose to use `getcwd` syscall for
        // testing, since it will correctly return an error under seccomp rule.
        // This is more of a sanity check.

        // Here, we choose an error that getcwd call would never return on its own, so
        // we can make sure that getcwd failed because of seccomp rule.
        let expect_error = libc::EAGAIN;

        let syscall = LinuxSyscallBuilder::default()
            .names(vec![String::from("getcwd")])
            .action(LinuxSeccompAction::ScmpActErrno)
            .errno_ret(expect_error as u32)
            .build()?;
        let seccomp_profile = LinuxSeccompBuilder::default()
            .default_action(LinuxSeccompAction::ScmpActAllow)
            .architectures(vec![Arch::ScmpArchNative])
            .syscalls(vec![syscall])
            .build()?;

        test_utils::test_in_child_process(|| {
            let _ = prctl::set_no_new_privileges(true);
            initialize_seccomp(&seccomp_profile)?;
            let ret = nix::unistd::getcwd();
            if ret.is_ok() {
                bail!("getcwd didn't error out as seccomp profile specified");
            }

            if let Some(errno) = ret.err() {
                if errno != nix::errno::from_i32(expect_error) {
                    bail!(
                        "getcwd failed but we didn't get the expected error from seccomp profile: {}", errno
                    );
                }
            }

            Ok(())
        })?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_moby() -> Result<()> {
        let fixture_path =
            path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/seccomp/fixture/config.json");
        let spec = oci_spec::runtime::Spec::load(fixture_path)
            .context("Failed to load test spec for seccomp")?;

        // We know linux and seccomp exist, so let's just unwrap.
        let seccomp_profile = spec.linux().as_ref().unwrap().seccomp().as_ref().unwrap();
        test_utils::test_in_child_process(|| {
            let _ = prctl::set_no_new_privileges(true);
            initialize_seccomp(seccomp_profile)?;

            Ok(())
        })?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_seccomp_notify() -> Result<()> {
        let syscall = LinuxSyscallBuilder::default()
            .names(vec![String::from("getcwd")])
            .action(LinuxSeccompAction::ScmpActNotify)
            .build()?;
        let seccomp_profile = LinuxSeccompBuilder::default()
            .default_action(LinuxSeccompAction::ScmpActAllow)
            .architectures(vec![Arch::ScmpArchNative])
            .syscalls(vec![syscall])
            .build()?;
        test_utils::test_in_child_process(|| {
            let _ = prctl::set_no_new_privileges(true);
            let fd = initialize_seccomp(&seccomp_profile)?;
            if fd.is_none() {
                bail!("failed to get a seccomp notify fd with notify seccomp profile");
            }

            Ok(())
        })?;

        Ok(())
    }
}
