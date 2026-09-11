"""Bazel C/C++ toolchains backed by Kanna's pinned Zig SDK."""

load("@bazel_tools//tools/build_defs/cc:action_names.bzl", "ACTION_NAMES")
load("@bazel_tools//tools/cpp:cc_toolchain_config_lib.bzl", "feature", "flag_group", "flag_set", "tool_path")
load("@rules_cc//cc:defs.bzl", "cc_toolchain")
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/toolchains:cc_toolchain_config_info.bzl", "CcToolchainConfigInfo")

_COMPILE_ACTIONS = [
    ACTION_NAMES.c_compile,
    ACTION_NAMES.cpp_compile,
    ACTION_NAMES.linkstamp_compile,
    ACTION_NAMES.assemble,
    ACTION_NAMES.preprocess_assemble,
]

def _exec_path(file):
    if file.short_path.startswith("../"):
        return "external/" + file.short_path[3:]
    return file.short_path

def _generated_tool_path(file):
    # Toolchains are instantiated in the repository root, so Bazel resolves
    # this generated bazel-out path directly from the execution root.
    return file.path

def _zig_binary(zig_toolchain):
    for file in zig_toolchain.zig_files:
        if file.basename == "zig":
            return _exec_path(file)
    fail("the resolved pinned Zig toolchain does not expose its zig binary")

def _zig_cc_wrapper_impl(ctx):
    zig_toolchain = ctx.toolchains["@rules_zig//zig:toolchain_type"].zigtoolchaininfo
    zig = _zig_binary(zig_toolchain)
    marker = _exec_path(ctx.file.sysroot_marker)
    sysroot = marker[:-len("/.kanna-sysroot")]
    if ctx.attr.mode == "cc":
        command = """exec "$zig" cc -target {target} --sysroot "$sysroot" \\
  -isystem "$sysroot/usr/include" \\
  -isystem "$sysroot/usr/include/{multiarch}" \\
  -isystem "$sysroot/usr/include/gtk-3.0" \\
  -isystem "$sysroot/usr/include/glib-2.0" \\
  -isystem "$sysroot/usr/lib/{multiarch}/glib-2.0/include" \\
  -isystem "$sysroot/usr/include/pango-1.0" \\
  -isystem "$sysroot/usr/include/harfbuzz" \\
  -isystem "$sysroot/usr/include/cairo" \\
  -isystem "$sysroot/usr/include/gdk-pixbuf-2.0" \\
  -isystem "$sysroot/usr/include/atk-1.0" \\
  -isystem "$sysroot/usr/include/webkitgtk-4.1" \\
  -isystem "$sysroot/usr/include/libsoup-3.0" \\
  -L"$sysroot/usr/lib/{multiarch}" \"$@\"""".format(
            target = ctx.attr.target,
            multiarch = ctx.attr.multiarch,
        )
    elif ctx.attr.mode == "ar":
        command = "exec \"$zig\" ar \"$@\""
    elif ctx.attr.mode == "objcopy":
        command = "exec \"$zig\" objcopy \"$@\""
    elif ctx.attr.mode == "strip":
        command = "exec \"$zig\" objcopy --strip-all \"$@\""
    else:
        command = "echo 'tool not implemented by the pinned Zig driver: {}' >&2; exit 1".format(ctx.attr.mode)
    wrapper = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(
        wrapper,
        "#!/bin/sh\nset -eu\nzig=\"{}\"\nsysroot=\"{}\"\ncache=\"${{TMPDIR:-$PWD/.zig-cache}}\"\nmkdir -p \"$cache/global\" \"$cache/local\"\nexport ZIG_GLOBAL_CACHE_DIR=\"$cache/global\"\nexport ZIG_LOCAL_CACHE_DIR=\"$cache/local\"\nexport ZIG_LIB_DIR=\"$(dirname \"$zig\")/lib\"\n{}\n".format(
            zig,
            sysroot,
            command,
        ),
        is_executable = True,
    )
    return DefaultInfo(
        executable = wrapper,
        files = depset([wrapper]),
        runfiles = ctx.runfiles(
            files = [wrapper] + zig_toolchain.zig_files,
            transitive_files = depset(transitive = [
                ctx.attr.sysroot_files.files,
            ]),
        ),
    )

zig_cc_wrapper = rule(
    implementation = _zig_cc_wrapper_impl,
    executable = True,
    attrs = {
        "mode": attr.string(mandatory = True, values = ["ar", "cc", "nm", "objcopy", "strip"]),
        "multiarch": attr.string(mandatory = True),
        "sysroot_files": attr.label(mandatory = True),
        "sysroot_marker": attr.label(mandatory = True, allow_single_file = True),
        "target": attr.string(mandatory = True),
    },
    toolchains = ["@rules_zig//zig:toolchain_type"],
)

def _zig_cc_toolchain_config_impl(ctx):
    wrapper = ctx.file.compiler_wrapper
    marker = _exec_path(ctx.file.sysroot_marker)
    sysroot = marker[:-len("/.kanna-sysroot")]
    return cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        toolchain_identifier = ctx.attr.toolchain_identifier,
        host_system_name = "local",
        target_system_name = ctx.attr.target,
        target_cpu = ctx.attr.target_cpu,
        target_libc = "glibc-2.39",
        compiler = "zig-0.15.2",
        abi_version = "gnu.2.39",
        abi_libc_version = "2.39",
        builtin_sysroot = sysroot,
        cxx_builtin_include_directories = [
            "%package({}//)%/lib".format(ctx.attr.zig_repository),
            "%sysroot%/usr/include",
            "%sysroot%/usr/include/" + ctx.attr.multiarch,
            "%sysroot%/usr/lib/{}/glib-2.0/include".format(ctx.attr.multiarch),
        ],
        features = [
            feature(name = "supports_pic", enabled = True),
            feature(name = "supports_dynamic_linker", enabled = True),
            feature(
                name = "kanna_reproducible_compile",
                enabled = True,
                flag_sets = [flag_set(
                    actions = _COMPILE_ACTIONS,
                    flag_groups = [flag_group(flags = [
                        "-no-canonical-prefixes",
                    ])],
                )],
            ),
        ],
        tool_paths = [
            tool_path(name = "gcc", path = _generated_tool_path(wrapper)),
            tool_path(name = "ld", path = _generated_tool_path(wrapper)),
            tool_path(name = "cpp", path = _generated_tool_path(wrapper)),
            tool_path(name = "ar", path = _generated_tool_path(ctx.file.ar)),
            tool_path(name = "nm", path = _generated_tool_path(ctx.file.nm)),
            tool_path(name = "objcopy", path = _generated_tool_path(ctx.file.objcopy)),
            tool_path(name = "objdump", path = _generated_tool_path(ctx.file.nm)),
            tool_path(name = "strip", path = _generated_tool_path(ctx.file.strip)),
            tool_path(name = "gcov", path = _generated_tool_path(ctx.file.nm)),
        ],
    )

zig_cc_toolchain_config = rule(
    implementation = _zig_cc_toolchain_config_impl,
    attrs = {
        "ar": attr.label(mandatory = True, allow_single_file = True),
        "compiler_wrapper": attr.label(mandatory = True, allow_single_file = True),
        "multiarch": attr.string(mandatory = True),
        "nm": attr.label(mandatory = True, allow_single_file = True),
        "objcopy": attr.label(mandatory = True, allow_single_file = True),
        "strip": attr.label(mandatory = True, allow_single_file = True),
        "sysroot_marker": attr.label(mandatory = True, allow_single_file = True),
        "target": attr.string(mandatory = True),
        "target_cpu": attr.string(mandatory = True),
        "toolchain_identifier": attr.string(mandatory = True),
        "zig_repository": attr.string(mandatory = True),
    },
    provides = [CcToolchainConfigInfo],
)

def zig_linux_cc_toolchain(name, target, target_cpu, multiarch, sysroot, sysroot_marker, zig_repository, exec_compatible_with, target_compatible_with):
    wrappers = {}
    for mode in ("cc", "ar", "nm", "objcopy", "strip"):
        wrapper = name + "_" + mode
        zig_cc_wrapper(
            name = wrapper,
            mode = mode,
            multiarch = multiarch,
            sysroot_files = sysroot,
            sysroot_marker = sysroot_marker,
            target = target,
        )
        wrappers[mode] = ":" + wrapper

    native.filegroup(
        name = name + "_all_files",
        srcs = wrappers.values() + [
            "@rules_zig//zig:resolved_toolchain",
            sysroot,
        ],
    )
    zig_cc_toolchain_config(
        name = name + "_config",
        ar = wrappers["ar"],
        compiler_wrapper = wrappers["cc"],
        multiarch = multiarch,
        nm = wrappers["nm"],
        objcopy = wrappers["objcopy"],
        strip = wrappers["strip"],
        sysroot_marker = sysroot_marker,
        target = target,
        target_cpu = target_cpu,
        toolchain_identifier = name,
        zig_repository = zig_repository,
    )
    cc_toolchain(
        name = name + "_impl",
        all_files = ":" + name + "_all_files",
        ar_files = ":" + name + "_all_files",
        as_files = ":" + name + "_all_files",
        compiler_files = ":" + name + "_all_files",
        dwp_files = ":" + name + "_all_files",
        linker_files = ":" + name + "_all_files",
        objcopy_files = ":" + name + "_all_files",
        strip_files = ":" + name + "_all_files",
        supports_param_files = 1,
        toolchain_config = ":" + name + "_config",
        toolchain_identifier = name,
    )
    native.toolchain(
        name = name,
        exec_compatible_with = exec_compatible_with,
        target_compatible_with = target_compatible_with,
        toolchain = ":" + name + "_impl",
        toolchain_type = "@bazel_tools//tools/cpp:toolchain_type",
    )

def zig_linux_cc_toolchains(name, target, target_cpu, multiarch, sysroot, sysroot_marker, target_compatible_with):
    """Declare one target toolchain for each supported execution host."""
    for exec_name, zig_repository, exec_constraints in (
        ("macos_arm64", "@@rules_zig++zig+zig_0.15.2_aarch64-macos", ["@platforms//cpu:aarch64", "@platforms//os:osx"]),
        ("linux_x86_64", "@@rules_zig++zig+zig_0.15.2_x86_64-linux", ["@platforms//cpu:x86_64", "@platforms//os:linux"]),
        ("linux_arm64", "@@rules_zig++zig+zig_0.15.2_aarch64-linux", ["@platforms//cpu:aarch64", "@platforms//os:linux"]),
    ):
        zig_linux_cc_toolchain(
            name = "{}_on_{}".format(name, exec_name),
            exec_compatible_with = exec_constraints,
            multiarch = multiarch,
            sysroot = sysroot,
            sysroot_marker = sysroot_marker,
            target = target,
            target_compatible_with = target_compatible_with,
            target_cpu = target_cpu,
            zig_repository = zig_repository,
        )
