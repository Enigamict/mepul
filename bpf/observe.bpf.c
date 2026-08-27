// Component (1), kernel side: report the paths a job reaches for.
//
// Everything here answers one question — "did the traced job touch this path?"
// — and nothing else. No file contents ever cross into the kernel program; the
// only thing sent up the ring buffer is the path string the syscall was given.
//
// Two filters do the real work, and both exist because a pid alone is not
// enough to identify the job:
//
//   `tracked`  follows the process tree. A CI job forks, and the tree is what
//              we care about, not the one process we were handed.
//   `inside`   holds only processes that have already called chroot. Getting a
//              job into its rootfs takes host binaries (unshare, env, chroot)
//              running under the very pid the job will use, and they open their
//              own libraries and locales first. Those opens are
//              indistinguishable from the job's by path alone — and when the
//              host and the image are the same distribution, the paths resolve
//              on both sides. chroot is the moment the process stops looking at
//              the host, which makes it the honest boundary.
//
// A mount namespace would not work here: `unshare -m` creates the new namespace
// before env and chroot run, so they are already inside it.

#include "vmlinux.h"

#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

char LICENSE[] SEC("license") = "GPL";

#define PATH_MAX 4096

// One observation. Fixed size so `bpf_ringbuf_reserve` gets a constant the
// verifier can check; the ring is sized to hold far more of these than a job
// produces.
struct event {
    __u32 len;
    char path[PATH_MAX];
};

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 16 * 1024 * 1024);
} events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, __u32);
    __type(value, __u8);
} tracked SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, __u32);
    __type(value, __u8);
} inside SEC(".maps");

static __always_inline __u32 current_pid(void)
{
    // The tracepoints report thread ids, and so does bpf_get_current_pid_tgid's
    // low half; the process tree is tracked in the same terms throughout.
    return (__u32)bpf_get_current_pid_tgid();
}

static __always_inline int in_rootfs(void)
{
    __u32 pid = current_pid();
    return bpf_map_lookup_elem(&inside, &pid) != 0;
}

// Copies a userspace path into the ring buffer. `user_path` is the pointer the
// syscall was given, so this reports what the program asked for, not what the
// kernel resolved — resolution happens later, in userspace, against the job's
// own root.
static __always_inline void report(const char *user_path)
{
    struct event *e;
    long len;

    if (!user_path)
        return;

    e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e)
        return;

    len = bpf_probe_read_user_str(&e->path, sizeof(e->path), user_path);
    if (len <= 1) {
        // Unreadable or empty: nothing a profile could replay.
        bpf_ringbuf_discard(e, 0);
        return;
    }

    e->len = (__u32)(len - 1); // drop the trailing NUL
    bpf_ringbuf_submit(e, 0);
}

// Tracepoint contexts are declared by hand rather than pulled from vmlinux BTF.
// The syscall tracepoint layout is part of the kernel's stable ABI and is
// visible in /sys/kernel/tracing/events/<...>/format, so the offsets below are
// checkable without a CO-RE toolchain.
struct enter_args_16 {
    __u64 common;
    __u32 syscall_nr;
    __u32 pad;
    const char *filename; // first argument
};

struct enter_args_24 {
    __u64 common;
    __u32 syscall_nr;
    __u32 pad;
    __u64 dfd;            // first argument
    const char *filename; // second argument
};

struct fork_args {
    __u64 common;
    __u32 parent_comm;
    __s32 parent_pid;
    __u32 child_comm;
    __s32 child_pid;
};

struct exit_args {
    __u64 common;
    __u32 comm_loc;
    __u32 pad;
    __s32 pid;
};

// A job is a tree. Both the membership and the "already chrooted" state have to
// reach children, or a job that forks would go half-observed.
SEC("tracepoint/sched/sched_process_fork")
int on_fork(struct fork_args *ctx)
{
    __u32 parent = (__u32)ctx->parent_pid;
    __u32 child = (__u32)ctx->child_pid;
    __u8 one = 1;

    if (!bpf_map_lookup_elem(&tracked, &parent))
        return 0;

    bpf_map_update_elem(&tracked, &child, &one, BPF_ANY);
    if (bpf_map_lookup_elem(&inside, &parent))
        bpf_map_update_elem(&inside, &child, &one, BPF_ANY);
    return 0;
}

SEC("tracepoint/sched/sched_process_exit")
int on_exit(struct exit_args *ctx)
{
    __u32 pid = (__u32)ctx->pid;

    bpf_map_delete_elem(&tracked, &pid);
    bpf_map_delete_elem(&inside, &pid);
    return 0;
}

// The boundary. Everything reported below this line happened after the process
// stopped being able to see the host filesystem.
SEC("tracepoint/syscalls/sys_enter_chroot")
int on_chroot(struct enter_args_16 *ctx)
{
    __u32 pid = current_pid();
    __u8 one = 1;

    if (!bpf_map_lookup_elem(&tracked, &pid))
        return 0;

    bpf_map_update_elem(&inside, &pid, &one, BPF_ANY);
    return 0;
}

SEC("tracepoint/syscalls/sys_enter_execve")
int on_execve(struct enter_args_16 *ctx)
{
    if (in_rootfs())
        report(ctx->filename);
    return 0;
}

SEC("tracepoint/syscalls/sys_enter_openat")
int on_openat(struct enter_args_24 *ctx)
{
    if (in_rootfs())
        report(ctx->filename);
    return 0;
}

// A job fails just as hard on a file it only ever stat'ed: `test -f`, `ls`, and
// every configure script decide what to do from metadata alone, so a profile
// built from opens alone would leave those files out of the rootfs.
SEC("tracepoint/syscalls/sys_enter_newfstatat")
int on_newfstatat(struct enter_args_24 *ctx)
{
    if (in_rootfs())
        report(ctx->filename);
    return 0;
}

SEC("tracepoint/syscalls/sys_enter_statx")
int on_statx(struct enter_args_24 *ctx)
{
    if (in_rootfs())
        report(ctx->filename);
    return 0;
}

SEC("tracepoint/syscalls/sys_enter_faccessat")
int on_faccessat(struct enter_args_24 *ctx)
{
    if (in_rootfs())
        report(ctx->filename);
    return 0;
}

SEC("tracepoint/syscalls/sys_enter_faccessat2")
int on_faccessat2(struct enter_args_24 *ctx)
{
    if (in_rootfs())
        report(ctx->filename);
    return 0;
}

SEC("tracepoint/syscalls/sys_enter_readlinkat")
int on_readlinkat(struct enter_args_24 *ctx)
{
    if (in_rootfs())
        report(ctx->filename);
    return 0;
}
