# Duck Packages

Duck Packages is a focused GNOME application for viewing, opening, and safely
removing software installed by the Linux distribution. It uses PackageKit over
D-Bus, lets the distribution resolve transactions, and delegates authorization
to Polkit. It never invokes `dnf`, `apt`, `rpm`, `sudo`, or a shell.

The initial release targets Fedora 44+ while keeping the package backend
distribution-neutral. PackageKit 1.3.5 or newer is required for removal; when it
is unavailable or older, the application opens in read-only diagnostic mode.

## Dependencies

On Fedora 44+:

```sh
sudo dnf install rust cargo meson ninja-build \
  gtk4-devel libadwaita-devel appstream-devel \
  gettext-devel desktop-file-utils PackageKit
```

Required versions (the build fails if these are older):

- `meson >= 1.3`
- `gtk4-devel >= 4.14`
- `libadwaita-devel >= 1.5`
- `appstream-devel >= 1.0`
- `PackageKit >= 1.3.5` (for removal; otherwise the app opens read-only)

## Build

```sh
meson setup build -Dprofile=development
meson compile -C build
./build/src/duck-packages
```

Run checks:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
desktop-file-validate data/io.github.srwalkerb.DuckPackages.desktop
appstreamcli validate --no-net data/io.github.srwalkerb.DuckPackages.metainfo.xml
```

Build an RPM with vendored Rust dependencies:

```sh
./scripts/build-rpm.sh
```

The result is written below `packaging/rpmbuild/RPMS/`.

## Architecture

- `backend`: capability-based factory, PackageKit implementation, and diagnostic fallback.
- `catalog`: GIO desktop launchers, AppStream enrichment, and alias deduplication.
- `launch_diagnostic`: application-independent classification of launch evidence
  into actionable failure categories, with an honest fallback for unknown causes.
- `processes`: safe `/proc` inspection, verified application sessions, and
  revalidated same-user process termination without shell commands.
- `profile_locks`: detects stale Chromium-family `Singleton*` symlinks and
  revalidates them before removing only those lock entries.
- `domain`: opaque package identifiers, safe removal requests, plans, and events.
- `ui`: GTK4/libadwaita navigation, reusable application/package lists, and impact preview.

See [DESIGN.md](DESIGN.md) for the interface contract and
[docs/SPEC.md](docs/SPEC.md) for the implementation specification.

## Safety model

- Removal is always simulated first.
- `allow_dependencies` and `autoremove` are always false.
- A fresh simulation is compared with the confirmed plan before execution.
- A changed transaction is never executed without another review.
- PackageKit/Polkit owns authorization; Duck Packages has no privileged helper.
- User files and application profiles are never removed.

## License

GPL-3.0-or-later.
