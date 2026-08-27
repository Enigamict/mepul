// The handful of kernel definitions the observer needs.
//
// This is hand-written rather than dumped from BTF with `bpftool`. The generated
// article is 3 MB of declarations for a program that touches none of them: the
// observer reads only syscall tracepoint arguments, whose layout is stable ABI
// and visible in /sys/kernel/tracing/events/<...>/format. Keeping the surface
// this small is what lets the program build with nothing but clang — no BTF
// dump, no CO-RE toolchain, and nothing to regenerate per kernel.

#ifndef MEPUL_VMLINUX_H
#define MEPUL_VMLINUX_H

typedef signed char __s8;
typedef unsigned char __u8;
typedef short __s16;
typedef unsigned short __u16;
typedef int __s32;
typedef unsigned int __u32;
typedef long long __s64;
typedef unsigned long long __u64;

typedef __u8 u8;
typedef __u16 u16;
typedef __u32 u32;
typedef __u64 u64;

// `bpf_helper_defs.h` declares every helper the kernel has, including the
// networking ones, so these have to exist even though nothing here is a packet.
typedef __u16 __be16;
typedef __u16 __le16;
typedef __u32 __be32;
typedef __u32 __le32;
typedef __u64 __be64;
typedef __u64 __le64;
typedef __u32 __wsum;

#define BPF_MAP_TYPE_HASH 1
#define BPF_MAP_TYPE_RINGBUF 27

#define BPF_ANY 0

#endif /* MEPUL_VMLINUX_H */
