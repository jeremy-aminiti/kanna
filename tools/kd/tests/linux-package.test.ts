import { mkdtempSync, mkdirSync, readFileSync, readdirSync, readlinkSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  DESKTOP_BINARY_NAME,
  INSTALLED_EXECUTABLES,
  buildDebControl,
  buildDesktopEntry,
  buildPostinst,
  buildPrerm,
  channelIdentity,
  debFileName,
  debianVersion,
  packageLayout,
  stageLinuxPackageTree,
} from "../src/runtime/linux-package";

const repoRoot = resolve(import.meta.dirname, "..", "..", "..");

const temporaries: string[] = [];
function scratch(): string {
  const dir = mkdtempSync(join(tmpdir(), "kd-linux-package-"));
  temporaries.push(dir);
  return dir;
}
afterEach(() => {
  while (temporaries.length > 0) rmSync(temporaries.pop() as string, { recursive: true, force: true });
});

describe("debianVersion", () => {
  /**
   * The ordering rule the whole channel design rests on: a staging candidate
   * must sort *below* the production release of the same version, or a machine
   * tracking production would be offered a staging build as an upgrade.
   */
  it("sorts staging below production for the same version", () => {
    expect(debianVersion("1.2.3", "staging", 4)).toBe("1.2.3~staging.4-1");
    expect(debianVersion("1.2.3", "production")).toBe("1.2.3-1");
    // `~` is dpkg's only character that sorts before the empty string.
    expect("1.2.3~staging.4-1" < "1.2.3-1").toBe(false);
    expect(debianVersion("1.2.3", "staging", 4).startsWith("1.2.3~")).toBe(true);
  });

  it("refuses versions and iterations it cannot order", () => {
    expect(() => debianVersion("1.2", "production")).toThrow(/X\.Y\.Z/);
    expect(() => debianVersion("1.2.3", "staging")).toThrow(/iteration/);
    expect(() => debianVersion("1.2.3", "staging", 0)).toThrow(/iteration/);
    expect(() => debianVersion("1.2.3", "production", 1)).toThrow(/no staging iteration/);
  });
});

describe("packageLayout", () => {
  it("keeps the two channels' installed files disjoint", () => {
    const production = packageLayout({ channel: "production" });
    const staging = packageLayout({ channel: "staging" });
    for (const key of ["libDir", "desktopBinary", "launcher", "desktopEntry"] as const) {
      expect(production[key]).not.toBe(staging[key]);
    }
    expect(production.icon(128)).not.toBe(staging.icon(128));
  });

  /**
   * The reason the layout is siblings-in-one-directory rather than anything
   * tidier: the runtime's existing sidecar search looks beside the running
   * executable, so an installed desktop needs no installed-only code path.
   */
  it("puts every sidecar beside the desktop binary", () => {
    const layout = packageLayout({ channel: "production" });
    expect(layout.desktopBinary).toBe(`/usr/lib/kanna/${DESKTOP_BINARY_NAME}`);
    expect(layout.libDir).toBe("/usr/lib/kanna");
  });
});

describe("stageLinuxPackageTree", () => {
  function fixture(): { root: string; input: Parameters<typeof stageLinuxPackageTree>[0] } {
    const dir = scratch();
    const binaries = join(dir, "bin");
    const icons = join(dir, "icons");
    const resources = join(dir, "kanna");
    mkdirSync(binaries, { recursive: true });
    mkdirSync(icons, { recursive: true });
    for (const name of INSTALLED_EXECUTABLES) writeFileSync(join(binaries, name), `#!/bin/sh\n# ${name}\n`);
    for (const size of [32, 64, 128]) writeFileSync(join(icons, `${size}x${size}.png`), "png");
    for (const section of ["agents", "workflows", "tasks"]) {
      mkdirSync(join(resources, section), { recursive: true });
      writeFileSync(join(resources, section, "sample.json"), "{}");
    }
    return {
      root: join(dir, "tree"),
      input: {
        channel: "production",
        root: join(dir, "tree"),
        binariesDir: binaries,
        builtinResourcesDir: resources,
        iconsDir: icons,
        control: { version: "1.2.3", architecture: "x86_64", depends: ["libc6"] },
      },
    };
  }

  it("lays down every executable, the resources, the entry and the icons", () => {
    const { root, input } = fixture();
    stageLinuxPackageTree(input);
    for (const name of INSTALLED_EXECUTABLES) {
      const path = join(root, "usr", "lib", "kanna", name);
      expect(statSync(path).mode & 0o111).toBeGreaterThan(0);
    }
    expect(readFileSync(join(root, "usr", "lib", "kanna", ".kanna", "workflows", "sample.json"), "utf8")).toBe("{}");
    expect(readFileSync(join(root, "usr", "share", "applications", "build.kanna.desktop"), "utf8")).toContain(
      "Exec=/usr/bin/kanna %U"
    );
    expect(statSync(join(root, "usr", "share", "icons", "hicolor", "128x128", "apps", "build.kanna.png")).isFile()).toBe(true);
  });

  /**
   * A copy would make `current_exe()` resolve to `/usr/bin`, where none of the
   * sidecars live, and every task spawn would fail on an installed machine
   * while passing on every build machine.
   */
  it("installs the PATH launcher as a symlink into the library directory", () => {
    const { root, input } = fixture();
    stageLinuxPackageTree(input);
    expect(readlinkSync(join(root, "usr", "bin", "kanna"))).toBe(`../lib/kanna/${DESKTOP_BINARY_NAME}`);
  });

  it("refuses to build a package missing a binary, a resource section or an icon", () => {
    for (const [remove, message] of [
      [join("bin", "kanna-worker"), /kanna-worker/],
      [join("kanna", "workflows"), /workflows/],
      [join("icons", "64x64.png"), /64px icon/],
    ] as const) {
      const { input } = fixture();
      rmSync(join(input.binariesDir, "..", remove), { recursive: true, force: true });
      expect(() => stageLinuxPackageTree(input)).toThrow(message);
    }
  });

  /**
   * Found by building the first real package: `writeFileSync` and `mkdirSync`
   * take their modes from the builder's umask, and the desktop entry shipped
   * 0664. A build machine with a group-writable default would install
   * group-writable files onto every user's machine, invisibly — the tree looks
   * fine, and only `dpkg-deb --contents` shows it.
   */
  it("gives every packaged file a mode the package chose, not the builder's umask", () => {
    const { root, input } = fixture();
    stageLinuxPackageTree(input);
    const modes = new Map<string, number>();
    const walk = (dir: string) => {
      for (const entry of readdirSync(dir, { withFileTypes: true })) {
        const path = join(dir, entry.name);
        if (entry.isSymbolicLink()) continue;
        if (entry.isDirectory()) {
          modes.set(path, statSync(path).mode & 0o777);
          walk(path);
        } else {
          modes.set(path, statSync(path).mode & 0o777);
        }
      }
    };
    walk(root);
    // `dpkg-deb` records the tree root as `./`, so it ships too.
    modes.set(root, statSync(root).mode & 0o777);
    for (const [path, mode] of modes) {
      expect([0o755, 0o644], `${path} has mode ${mode.toString(8)}`).toContain(mode);
      // Nothing group- or world-writable, whatever the builder's umask was.
      expect(mode & 0o022, `${path} is writable beyond its owner`).toBe(0);
    }
    expect(modes.get(join(root, "usr", "share", "applications", "build.kanna.desktop"))).toBe(0o644);
    expect(modes.get(join(root, "usr", "lib", "kanna", "kanna-daemon"))).toBe(0o755);
    expect(modes.get(join(root, "DEBIAN", "postinst"))).toBe(0o755);
  });

  it("writes executable maintainer scripts", () => {
    const { root, input } = fixture();
    stageLinuxPackageTree(input);
    for (const name of ["postinst", "prerm"]) {
      expect(statSync(join(root, "DEBIAN", name)).mode & 0o111).toBeGreaterThan(0);
    }
  });
});

describe("maintainer scripts", () => {
  /**
   * The single most important property of the upgrade path: replacing package
   * files must not touch the daemon or its live agent sessions. A maintainer
   * script that stopped the daemon would kill every running agent on the
   * machine during an unattended `apt upgrade`.
   */
  it("never stops a daemon, kills a cgroup or starts a service", () => {
    for (const script of [buildPostinst("production"), buildPostinst("staging"), buildPrerm("production"), buildPrerm("staging")]) {
      expect(script).not.toMatch(/stop-daemon|pkill|killall|kill -|systemd-run/);
      expect(script).not.toMatch(/systemctl --user (start|enable)\b/);
      expect(script).not.toMatch(/enable-linger/);
    }
  });

  it("tells the operator which restart applies the new version", () => {
    expect(buildPostinst("production")).toContain("systemctl --user restart kanna-worker.service");
    expect(buildPostinst("staging")).toContain("systemctl --user restart kanna-staging-worker.service");
  });

  it("keeps user data on removal", () => {
    expect(buildPrerm("production")).toContain("NOT removed");
    expect(buildPrerm("production")).not.toMatch(/rm -rf/);
  });
});

describe("buildDebControl", () => {
  it("refuses an unaudited package", () => {
    expect(() =>
      buildDebControl({ channel: "production", version: "1.2.3", architecture: "arm64", depends: [], installedSizeKb: 10 })
    ).toThrow(/audited/);
  });

  /**
   * Both channels are installable side by side on a developer's machine, which
   * is only true while neither declares a conflict with the other.
   */
  it("does not make the channels conflict", () => {
    const control = buildDebControl({
      channel: "staging",
      version: "1.2.3",
      stagingIteration: 2,
      architecture: "arm64",
      depends: ["libc6 (>= 2.39)"],
      installedSizeKb: 10,
    });
    expect(control).toContain("Package: kanna-staging");
    expect(control).toContain("Version: 1.2.3~staging.2-1");
    expect(control).toContain("Architecture: arm64");
    expect(control).not.toMatch(/^Conflicts:/m);
    expect(control).not.toMatch(/^Replaces:/m);
  });

  it("names the file the way the pool and the manifest refer to it", () => {
    expect(debFileName({ channel: "production", version: "2.0.1", architecture: "x86_64" })).toBe(
      "kanna_2.0.1-1_amd64.deb"
    );
    expect(debFileName({ channel: "staging", version: "2.0.1", stagingIteration: 3, architecture: "arm64" })).toBe(
      "kanna-staging_2.0.1~staging.3-1_arm64.deb"
    );
  });
});

describe("desktop entry", () => {
  it("claims its own window class so the shell does not show a second icon", () => {
    expect(buildDesktopEntry("production")).toContain("StartupWMClass=build.kanna");
    expect(buildDesktopEntry("staging")).toContain("StartupWMClass=build.kanna.staging");
    expect(buildDesktopEntry("staging")).toContain("Name=Kanna Staging");
  });
});

/**
 * The layout exists in two languages — here for the build and in
 * `crates/runtime-defaults/src/linux_install.rs` for the runtime that has to
 * find the files again. Nothing on a build machine can catch them disagreeing:
 * the package would install perfectly and the desktop would fail to find a
 * sidecar on a user's machine. So they are compared directly.
 */
describe("the Rust runtime's copy of the same layout", () => {
  const rust = readFileSync(join(repoRoot, "crates", "runtime-defaults", "src", "linux_install.rs"), "utf8");

  it("agrees on the executable set", () => {
    const declared = /INSTALLED_EXECUTABLES: \[&str; (\d+)\] = \[([\s\S]*?)\];/.exec(rust);
    expect(declared).not.toBeNull();
    const names = [...(declared as RegExpExecArray)[2].matchAll(/"([^"]+)"/g)].map((match) => match[1]);
    // `DESKTOP_BINARY_NAME` appears as a constant rather than a literal.
    expect([DESKTOP_BINARY_NAME, ...names]).toEqual([...INSTALLED_EXECUTABLES]);
    expect(Number((declared as RegExpExecArray)[1])).toBe(INSTALLED_EXECUTABLES.length);
  });

  it("agrees on the package names, the binary name and the icon sizes", () => {
    expect(rust).toContain(`pub const DESKTOP_BINARY_NAME: &str = "${DESKTOP_BINARY_NAME}";`);
    expect(rust).toContain(`Self::Production => "${channelIdentity("production").packageName}"`);
    expect(rust).toContain(`Self::Staging => "${channelIdentity("staging").packageName}"`);
    expect(rust).toContain(`Self::Production => "${channelIdentity("production").workerUnitName}"`);
    expect(rust).toContain(`Self::Staging => "${channelIdentity("staging").workerUnitName}"`);
    expect(rust).toContain("INSTALLED_ICON_SIZES: [u32; 3] = [32, 64, 128]");
  });
});
