//! Per-child seccomp network filter. No-op on non-Linux.

#[cfg(target_os = "linux")]
mod bpf {
    use libc::sock_filter;

    pub(super) const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    pub(super) const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    pub(super) const EPERM_VAL: u32 = 1;
    #[cfg(target_arch = "x86_64")]
    pub(super) const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    /// `seccomp_data` field offsets.
    pub(super) const OFF_NR: u32 = 0;
    pub(super) const OFF_ARCH: u32 = 4;
    #[cfg(target_arch = "x86_64")]
    pub(super) const EXPECTED_ARCH: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    pub(super) const EXPECTED_ARCH: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub(super) const EXPECTED_ARCH: u32 = 0;

    pub(super) fn stmt(code: u32, k: u32) -> sock_filter {
        sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        }
    }

    pub(super) fn jump(code: u32, k: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        }
    }

    /// Reject a foreign-arch (and, on x86_64, x32) syscall number before
    /// matching `nr`, so a syscall reached through another table cannot
    /// alias past the deny list.
    pub(super) fn push_arch_nr_gate(f: &mut Vec<sock_filter>) {
        use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};
        f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARCH));
        f.push(jump(BPF_JMP | BPF_JEQ | BPF_K, EXPECTED_ARCH, 1, 0));
        f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM_VAL));
        f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_NR));
        #[cfg(target_arch = "x86_64")]
        {
            f.push(jump(
                BPF_JMP | libc::BPF_JSET | BPF_K,
                X32_SYSCALL_BIT,
                0,
                1,
            ));
            f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM_VAL));
        }
    }
}

/// Syscalls that open or send on a socket, including the `io_uring` and
/// `sendmmsg` paths that bypass the classic entry points.
#[cfg(target_os = "linux")]
fn child_network_blocked_syscalls() -> [u32; 11] {
    use libc::{
        SYS_accept, SYS_accept4, SYS_bind, SYS_connect, SYS_io_uring_enter, SYS_io_uring_register,
        SYS_io_uring_setup, SYS_listen, SYS_sendmmsg, SYS_sendmsg, SYS_sendto,
    };
    [
        SYS_connect as u32,
        SYS_bind as u32,
        SYS_sendto as u32,
        SYS_sendmsg as u32,
        SYS_sendmmsg as u32,
        SYS_listen as u32,
        SYS_accept as u32,
        SYS_accept4 as u32,
        SYS_io_uring_setup as u32,
        SYS_io_uring_enter as u32,
        SYS_io_uring_register as u32,
    ]
}

#[cfg(target_os = "linux")]
fn build_child_network_filter() -> Vec<libc::sock_filter> {
    use bpf::{EPERM_VAL, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO, jump, stmt};
    use libc::{BPF_JEQ, BPF_JMP, BPF_K, BPF_RET};

    let blocked = child_network_blocked_syscalls();
    let mut f = Vec::with_capacity(blocked.len() + 10);
    bpf::push_arch_nr_gate(&mut f);
    let n = blocked.len();
    for (i, &sys) in blocked.iter().enumerate() {
        let remaining = n - i - 1;
        f.push(jump(BPF_JMP | BPF_JEQ | BPF_K, sys, remaining as u8 + 1, 0));
    }
    f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM_VAL));
    f
}

/// Built once: `pre_exec` runs after `fork`, where allocating is unsafe.
#[cfg(target_os = "linux")]
pub fn prebuilt_child_network_filter() -> &'static [libc::sock_filter] {
    static FILTER: std::sync::OnceLock<Vec<libc::sock_filter>> = std::sync::OnceLock::new();
    FILTER.get_or_init(build_child_network_filter)
}

/// Install the seccomp BPF filter blocking network syscalls.
///
/// # Safety
///
/// Must be called in a `pre_exec` context (after `fork`, before `exec`).
#[cfg(target_os = "linux")]
pub unsafe fn install_child_network_filter(filter: &[libc::sock_filter]) -> std::io::Result<()> {
    use libc::{PR_SET_NO_NEW_PRIVS, PR_SET_SECCOMP, SECCOMP_MODE_FILTER, prctl, sock_fprog};

    let prog = sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: prctl with PR_SET_NO_NEW_PRIVS is safe in pre_exec context.
    if unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: prog is a valid sock_fprog pointing to a filter the kernel copies.
    if unsafe {
        prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER as libc::c_ulong,
            &prog as *const _ as libc::c_ulong,
            0,
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// # Safety
///
/// No-op on non-Linux.
#[cfg(not(target_os = "linux"))]
pub unsafe fn install_child_network_filter() -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::bpf::*;
    use super::{build_child_network_filter, child_network_blocked_syscalls};
    use libc::sock_filter;

    /// Minimal classic-BPF interpreter over synthetic `seccomp_data` fields.
    fn eval(filter: &[sock_filter], arch: u32, nr: u32) -> u32 {
        use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_JSET, BPF_K, BPF_LD, BPF_RET, BPF_W};
        let mut pc = 0usize;
        let mut a = 0u32;
        for _ in 0..filter.len().saturating_mul(2) {
            let insn = &filter[pc];
            let op = insn.code as u32;
            if op == (BPF_LD | BPF_W | BPF_ABS) {
                a = match insn.k {
                    OFF_NR => nr,
                    OFF_ARCH => arch,
                    _ => 0,
                };
                pc += 1;
            } else if op == (BPF_JMP | BPF_JEQ | BPF_K) {
                pc = if a == insn.k {
                    pc + 1 + insn.jt as usize
                } else {
                    pc + 1 + insn.jf as usize
                };
            } else if op == (BPF_JMP | BPF_JSET | BPF_K) {
                pc = if a & insn.k != 0 {
                    pc + 1 + insn.jt as usize
                } else {
                    pc + 1 + insn.jf as usize
                };
            } else if op == (BPF_RET | BPF_K) {
                return insn.k;
            } else {
                panic!("unsupported opcode {:#x} at {pc}", insn.code);
            }
            assert!(pc < filter.len(), "pc out of range");
        }
        panic!("filter did not RET");
    }

    fn is_allow(r: u32) -> bool {
        r == SECCOMP_RET_ALLOW
    }

    fn is_eperm(r: u32) -> bool {
        r == (SECCOMP_RET_ERRNO | EPERM_VAL)
    }

    #[test]
    fn blocks_every_network_syscall_and_allows_read_and_socket() {
        let f = build_child_network_filter();
        for sys in child_network_blocked_syscalls() {
            assert!(
                is_eperm(eval(&f, EXPECTED_ARCH, sys)),
                "syscall {sys} must be EPERM"
            );
        }
        assert!(is_allow(eval(&f, EXPECTED_ARCH, libc::SYS_read as u32)));
        assert!(is_allow(eval(&f, EXPECTED_ARCH, libc::SYS_socket as u32)));
    }

    #[test]
    fn covers_io_uring_and_sendmmsg() {
        let blocked = child_network_blocked_syscalls();
        for sys in [
            libc::SYS_sendmmsg,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            assert!(blocked.contains(&(sys as u32)), "{sys} missing");
        }
    }

    #[test]
    fn wrong_arch_and_x32_are_denied_before_the_nr_match() {
        let f = build_child_network_filter();
        assert!(is_eperm(eval(&f, 0xdead_beef, libc::SYS_read as u32)));
        #[cfg(target_arch = "x86_64")]
        assert!(is_eperm(eval(
            &f,
            EXPECTED_ARCH,
            (libc::SYS_read as u32) | X32_SYSCALL_BIT
        )));
    }

    #[test]
    fn prebuilt_filter_is_shared() {
        let a = super::prebuilt_child_network_filter();
        let b = super::prebuilt_child_network_filter();
        assert!(std::ptr::eq(a, b));
        assert_eq!(a.len(), build_child_network_filter().len());
    }
}
