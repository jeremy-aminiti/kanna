"""Fetch a hash-locked Ubuntu .deb set into a compile sysroot."""

_HEX_DIGITS = {
    "0": True,
    "1": True,
    "2": True,
    "3": True,
    "4": True,
    "5": True,
    "6": True,
    "7": True,
    "8": True,
    "9": True,
    "a": True,
    "b": True,
    "c": True,
    "d": True,
    "e": True,
    "f": True,
}

def _host_zig(repository_ctx):
    name = repository_ctx.os.name.lower()
    arch = repository_ctx.os.arch.lower()
    if name.startswith("mac os") and arch in ("aarch64", "arm64"):
        return repository_ctx.path(repository_ctx.attr._zig_macos_arm64)
    if name == "linux" and arch in ("x86_64", "amd64"):
        return repository_ctx.path(repository_ctx.attr._zig_linux_amd64)
    if name == "linux" and arch in ("aarch64", "arm64"):
        return repository_ctx.path(repository_ctx.attr._zig_linux_arm64)
    fail("Ubuntu sysroot extraction has no pinned Zig 0.15.2 tool for {} {}".format(name, arch))

def _validate_lock(repository_ctx, lock):
    if lock.get("formatVersion") != 1:
        fail("{}: unsupported sysroot lock format".format(repository_ctx.attr.lock))
    if lock.get("architecture") != repository_ctx.attr.architecture:
        fail("{}: lock architecture is {}, expected {}".format(
            repository_ctx.attr.lock,
            lock.get("architecture"),
            repository_ctx.attr.architecture,
        ))
    if lock.get("release") != "noble":
        fail("{}: sysroot release must be noble".format(repository_ctx.attr.lock))
    root = lock.get("snapshotRoot")
    if not root or not root.startswith("https://snapshot.ubuntu.com/ubuntu/"):
        fail("{}: sysroot must use the Ubuntu snapshot service".format(repository_ctx.attr.lock))
    packages = lock.get("packages")
    if type(packages) != "list" or not packages:
        fail("{}: sysroot lock has no packages".format(repository_ctx.attr.lock))
    names = {}
    for package in packages:
        name = package.get("name")
        architecture = package.get("architecture")
        sha256 = package.get("sha256")
        size = package.get("size")
        url = package.get("url")
        if not name or name in names:
            fail("{}: duplicate or empty package {}".format(repository_ctx.attr.lock, name))
        if architecture not in (repository_ctx.attr.architecture, "all"):
            fail("{}: {} has architecture {}".format(repository_ctx.attr.lock, name, architecture))
        if not sha256 or len(sha256) != 64 or any([sha256[i] not in _HEX_DIGITS for i in range(len(sha256))]):
            fail("{}: {} has no SHA-256".format(repository_ctx.attr.lock, name))
        if type(size) != "int" or size <= 0:
            fail("{}: {} has invalid size".format(repository_ctx.attr.lock, name))
        if not url or not url.startswith(root + "/") or not url.endswith(".deb"):
            fail("{}: {} URL is outside its pinned snapshot".format(repository_ctx.attr.lock, name))
        names[name] = True
    return packages

def _sysroot_repository_impl(repository_ctx):
    lock = json.decode(repository_ctx.read(repository_ctx.attr.lock))
    packages = _validate_lock(repository_ctx, lock)
    zig = _host_zig(repository_ctx)
    repository_ctx.file("sysroot/.kanna-sysroot", "{} {}\n".format(
        lock["snapshot"],
        lock["architecture"],
    ))

    for index, package in enumerate(packages):
        stem = "{}-{}".format(index, package["name"])
        deb = repository_ctx.path(".debs/{}.deb".format(stem))
        unpack = repository_ctx.path(".unpack/{}".format(stem))
        repository_ctx.download(
            package["url"],
            deb,
            sha256 = package["sha256"],
        )
        repository_ctx.file(".unpack/{}/.keep".format(stem), "")
        result = repository_ctx.execute(
            [zig, "ar", "x", deb],
            working_directory = str(unpack),
            quiet = True,
        )
        if result.return_code:
            fail("zig ar failed for {}: {}\n{}".format(
                package["name"],
                result.stdout,
                result.stderr,
            ))
        data_archive = None
        for suffix in ("zst", "xz", "gz"):
            candidate = unpack.get_child("data.tar." + suffix)
            if candidate.exists:
                data_archive = candidate
                break
        if data_archive == None:
            fail("{} has no supported data.tar payload".format(package["name"]))
        repository_ctx.extract(data_archive, output = "sysroot")
        repository_ctx.delete(unpack)
        repository_ctx.delete(deb)

    repository_ctx.file("BUILD.bazel", """
package(default_visibility = ["//visibility:public"])

exports_files(["sysroot/.kanna-sysroot"])

filegroup(
    name = "files",
    srcs = glob([
        "sysroot/lib/**",
        "sysroot/lib64/**",
        "sysroot/usr/include/**",
        "sysroot/usr/lib/**",
        "sysroot/usr/share/pkgconfig/**",
    ], allow_empty = True) + ["sysroot/.kanna-sysroot"],
)
""")

ubuntu_sysroot_repository = repository_rule(
    implementation = _sysroot_repository_impl,
    attrs = {
        "architecture": attr.string(mandatory = True, values = ["amd64", "arm64"]),
        "lock": attr.label(mandatory = True, allow_single_file = True),
        # All three are generated by the repository's one rules_zig extension
        # at the one pinned version. Selection here is for the execution host,
        # not the target architecture of the sysroot being unpacked.
        "_zig_macos_arm64": attr.label(
            default = "@zig_0.15.2_aarch64-macos//:zig",
            allow_single_file = True,
        ),
        "_zig_linux_amd64": attr.label(
            default = "@zig_0.15.2_x86_64-linux//:zig",
            allow_single_file = True,
        ),
        "_zig_linux_arm64": attr.label(
            default = "@zig_0.15.2_aarch64-linux//:zig",
            allow_single_file = True,
        ),
    },
)
